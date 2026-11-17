//! What happens when something rings: a desktop notification plus a
//! looping sound, both of which stop when the alarm is dismissed or snoozed.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::task::JoinHandle;
use zbus::zvariant::{OwnedFd, Value};

const DEFAULT_SOUND: &str = "/usr/share/sounds/freedesktop/stereo/alarm-clock-elapsed.oga";

/// Client side of the freedesktop notification spec. Any notification
/// server (Plasma, GNOME, mako, dunst, or one built into your shell) works.
#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
pub trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// `reason` in NotificationClosed: the user closed it (e.g. swiped it away).
pub const CLOSED_BY_USER: u32 = 2;

/// Don't re-show a closed notification more often than this, in case the
/// server closes every notification straight away (e.g. do-not-disturb).
const RESHOW_INTERVAL: Duration = Duration::from_secs(5);

/// systemd-logind, on the system bus. We only need inhibitor locks.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait LoginManager {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;
}

/// A ringing alarm or timer. Dropping/stopping it silences the sound.
pub struct Ringing {
    pub notification_id: Option<u32>,
    summary: String,
    body: String,
    shown_at: Instant,
    sound: JoinHandle<()>,
    /// logind releases the lock when this fd is closed, i.e. when the
    /// Ringing is stopped or dropped.
    _inhibit: Option<OwnedFd>,
}

impl Ringing {
    pub async fn start(
        notifications: &NotificationsProxy<'_>,
        login: Option<&LoginManagerProxy<'_>>,
        summary: &str,
        body: &str,
        sound: Option<&str>,
    ) -> Self {
        // Take the lock first: the RTC just woke the device, and the launcher's
        // idle policy may try to suspend it again before anyone dismisses.
        let _inhibit = match login {
            Some(login) => login
                .inhibit("sleep:idle", "clockd", "An alarm is ringing", "block")
                .await
                .map_err(|e| tracing::warn!("could not inhibit suspend: {e}"))
                .ok(),
            None => None,
        };

        let notification_id = show(notifications, summary, body).await;

        let path = sound.unwrap_or(DEFAULT_SOUND).to_owned();
        let sound = tokio::spawn(async move {
            loop {
                if !play_once(&path).await {
                    tracing::warn!("no audio player worked for {path}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        });

        Self {
            notification_id,
            summary: summary.to_owned(),
            body: body.to_owned(),
            shown_at: Instant::now(),
            sound,
            _inhibit,
        }
    }

    /// Show the notification again after it was closed while still ringing,
    /// so the Snooze/Dismiss buttons come back. Returns false if it was
    /// re-shown too recently and was left closed.
    pub async fn reshow(&mut self, notifications: &NotificationsProxy<'_>) -> bool {
        if self.shown_at.elapsed() < RESHOW_INTERVAL {
            return false;
        }
        self.notification_id = show(notifications, &self.summary, &self.body).await;
        self.shown_at = Instant::now();
        true
    }

    pub async fn stop(self, notifications: &NotificationsProxy<'_>) {
        // Aborting drops the running Child, and kill_on_drop stops playback.
        self.sound.abort();
        if let Some(id) = self.notification_id {
            let _ = notifications.close_notification(id).await;
        }
    }
}

async fn show(notifications: &NotificationsProxy<'_>, summary: &str, body: &str) -> Option<u32> {
    let mut hints = HashMap::new();
    hints.insert("urgency", Value::U8(2)); // critical: shown over fullscreen apps
    hints.insert("resident", Value::Bool(true)); // stay after an action is clicked
    hints.insert("category", Value::from("alarm"));

    notifications
        .notify(
            "Clock",
            0,
            "alarm-symbolic",
            summary,
            body,
            &["snooze", "Snooze", "dismiss", "Dismiss"],
            hints,
            0, // never expire
        )
        .await
        .map_err(|e| tracing::warn!("could not show notification: {e}"))
        .ok()
}

/// Play the file once with PipeWire, falling back to PulseAudio's player.
///
/// The stream is tagged `media.role=Alarm` (pw-play would default to
/// "Music"), so WirePlumber can give alarms their own volume and routing.
async fn play_once(path: &str) -> bool {
    let players: [(&str, &[&str]); 2] = [
        ("pw-play", &["--media-role", "Alarm"]),
        ("paplay", &["--property=media.role=Alarm"]),
    ];
    for (player, role_args) in players {
        let status =
            Command::new(player).args(role_args).arg(path).kill_on_drop(true).status().await;
        if matches!(status, Ok(s) if s.success()) {
            return true;
        }
    }
    false
}

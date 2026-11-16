//! What happens when something rings: a desktop notification plus a
//! looping sound, both of which stop when the alarm is dismissed or snoozed.

use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;
use tokio::task::JoinHandle;
use zbus::zvariant::Value;

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
}

/// A ringing alarm or timer. Dropping/stopping it silences the sound.
pub struct Ringing {
    pub notification_id: Option<u32>,
    sound: JoinHandle<()>,
}

impl Ringing {
    pub async fn start(
        notifications: &NotificationsProxy<'_>,
        summary: &str,
        body: &str,
        sound: Option<&str>,
    ) -> Self {
        let mut hints = HashMap::new();
        hints.insert("urgency", Value::U8(2)); // critical: shown over fullscreen apps
        hints.insert("resident", Value::Bool(true)); // stay after an action is clicked
        hints.insert("category", Value::from("alarm"));

        let notification_id = notifications
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
            .ok();

        let path = sound.unwrap_or(DEFAULT_SOUND).to_owned();
        let sound = tokio::spawn(async move {
            loop {
                if !play_once(&path).await {
                    tracing::warn!("no audio player worked for {path}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        });

        Self { notification_id, sound }
    }

    pub async fn stop(self, notifications: &NotificationsProxy<'_>) {
        // Aborting drops the running Child, and kill_on_drop stops playback.
        self.sound.abort();
        if let Some(id) = self.notification_id {
            let _ = notifications.close_notification(id).await;
        }
    }
}

/// Play the file once with PipeWire, falling back to PulseAudio's player.
async fn play_once(path: &str) -> bool {
    for player in ["pw-play", "paplay"] {
        let status = Command::new(player).arg(path).kill_on_drop(true).status().await;
        if matches!(status, Ok(s) if s.success()) {
            return true;
        }
    }
    false
}

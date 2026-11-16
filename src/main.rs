mod core;
mod dbus;
mod model;
mod ring;
mod store;
mod tzwatch;
mod waker;

use crate::core::{Core, Due};
use crate::dbus::{ClockdIface, BUS_NAME, OBJECT_PATH};
use crate::ring::{LoginManagerProxy, NotificationsProxy};
use crate::waker::{WakeReason, Waker};
use anyhow::Result;
use chrono::Local;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let state = store::State::load()?;
    let conn = zbus::Connection::session().await?;
    let notifications = NotificationsProxy::new(&conn).await?;
    let login = connect_logind()
        .await
        .map_err(|e| tracing::warn!("no logind: alarms can't keep the system awake while ringing: {e}"))
        .ok();
    let core = Arc::new(Core::new(state, notifications, login));

    // Export the object first, *then* claim the name, so a client started
    // by D-Bus activation never sees the name without the object behind it.
    conn.object_server()
        .at(OBJECT_PATH, ClockdIface { core: core.clone() })
        .await?;
    conn.request_name(BUS_NAME).await?;
    tracing::info!("{BUS_NAME} ready");

    let actions_core = core.clone();
    tokio::spawn(async move {
        if let Err(e) = handle_notification_actions(actions_core).await {
            tracing::error!("notification Snooze/Dismiss buttons stopped working: {e:#}");
        }
    });
    let tz_core = core.clone();
    tokio::spawn(async move {
        if let Err(e) = tzwatch::watch(tz_core).await {
            tracing::warn!("not watching /etc/localtime, time zone changes need a restart: {e}");
        }
    });
    let scheduler = tokio::spawn(scheduler(conn.clone(), core.clone()));

    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        r = scheduler => r??,
        _ = term.recv() => tracing::info!("SIGTERM, exiting"),
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted, exiting"),
    }
    Ok(())
}

/// The heart of the daemon: ring whatever is due, then sleep until the next
/// deadline or until the state changes, whichever comes first.
async fn scheduler(conn: zbus::Connection, core: Arc<Core>) -> Result<()> {
    let iface = conn
        .object_server()
        .interface::<_, ClockdIface>(OBJECT_PATH)
        .await?;
    let emitter = iface.signal_emitter();

    let mut waker = Waker::new()?;
    tracing::info!(can_wake_system = waker.can_wake_system, "scheduler started");
    let mut last_next_alarm = None;

    loop {
        let now = Local::now();
        for due in core.take_due(now).await {
            match due {
                Due::Alarm { id, label, sound } => {
                    tracing::info!("alarm {id} ringing");
                    let title = if label.is_empty() { "Alarm".to_owned() } else { label.clone() };
                    let body = now.format("%H:%M").to_string();
                    core.start_ringing(&id, &title, &body, sound.as_deref()).await;
                    ClockdIface::alarm_ringing(emitter, &id, &label).await?;
                }
                Due::Timer { id, label } => {
                    tracing::info!("timer {id} finished");
                    core.start_ringing(&id, "Timer finished", &label, None).await;
                    ClockdIface::timer_finished(emitter, &id, &label).await?;
                }
            }
        }

        let (deadline, next_alarm) = {
            let state = core.state.lock().await;
            (Core::next_deadline(&state, now), Core::next_alarm(&state, now))
        };
        if next_alarm != last_next_alarm {
            iface.get().await.next_alarm_changed(emitter).await?;
            last_next_alarm = next_alarm;
        }

        tokio::select! {
            reason = waker.sleep_until(deadline) => {
                if let WakeReason::ClockChanged = reason? {
                    // take_due clamps the saved last check, so a backwards
                    // jump isn't treated as "time passed".
                    tracing::info!("system clock changed, rescheduling");
                }
            }
            _ = core.changed.notified() => {}
        }
    }
}

async fn connect_logind() -> Result<LoginManagerProxy<'static>> {
    let system = zbus::Connection::system().await?;
    Ok(LoginManagerProxy::new(&system).await?)
}

/// Snooze/Dismiss buttons on the notification come back as D-Bus signals.
async fn handle_notification_actions(core: Arc<Core>) -> Result<()> {
    let mut actions = core.notifications.receive_action_invoked().await?;
    // One bad signal or failed snooze must not end this loop: nothing would
    // handle the buttons afterwards, and nobody would know.
    while let Some(signal) = actions.next().await {
        let args = match signal.args() {
            Ok(args) => args,
            Err(e) => {
                tracing::warn!("ignoring malformed ActionInvoked signal: {e}");
                continue;
            }
        };
        let Some(id) = core.id_for_notification(args.id).await else { continue };
        match args.action_key.as_str() {
            "snooze" => {
                if let Err(e) = core.snooze(&id, 5).await {
                    tracing::warn!("snoozing {id}: {e:#}");
                }
            }
            _ => {
                core.dismiss(&id).await;
            }
        }
    }
    anyhow::bail!("notification signal stream ended")
}

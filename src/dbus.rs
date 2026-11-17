//! The D-Bus API your clock UI and launcher talk to.
//!
//!   service:   org.mechanix.services.Clock
//!   object:    /org/mechanix/services/Clock
//!   interface: org.mechanix.services.Clock
//!
//! Try it with busctl:
//!   busctl --user call org.mechanix.services.Clock /org/mechanix/services/Clock \
//!       org.mechanix.services.Clock AddAlarm yyys 7 30 31 "Work"
//!   busctl --user get-property org.mechanix.services.Clock /org/mechanix/services/Clock \
//!       org.mechanix.services.Clock NextAlarm

use crate::core::Core;
use crate::model::{Alarm, Timer};
use chrono::Local;
use std::sync::Arc;
use zbus::{fdo, interface, object_server::SignalEmitter};

pub const BUS_NAME: &str = "org.mechanix.services.Clock";
pub const OBJECT_PATH: &str = "/org/mechanix/services/Clock";

pub struct ClockdIface {
    pub core: Arc<Core>,
}

fn err(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

#[interface(name = "org.mechanix.services.Clock")]
impl ClockdIface {
    // ---------- alarms ----------

    /// `days`: bitmask, bit 0 = Monday ... bit 6 = Sunday, 0 = one-shot.
    async fn add_alarm(
        &self,
        hour: u8,
        minute: u8,
        days: u8,
        label: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        if hour > 23 || minute > 59 || days > 0x7f {
            return Err(fdo::Error::InvalidArgs("hour/minute/days out of range".into()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut state = self.core.state.lock().await;
        state.alarms.push(Alarm {
            id: id.clone(),
            label,
            hour,
            minute,
            days,
            enabled: true,
            snoozed_until: None,
            sound: None,
        });
        self.core.commit(&state).await.map_err(err)?;
        drop(state);
        Self::alarms_changed(&emitter).await?;
        Ok(id)
    }

    async fn remove_alarm(
        &self,
        id: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.core.dismiss(&id).await;
        let mut state = self.core.state.lock().await;
        let before = state.alarms.len();
        state.alarms.retain(|a| a.id != id);
        if state.alarms.len() == before {
            return Err(fdo::Error::InvalidArgs(format!("no alarm {id}")));
        }
        self.core.commit(&state).await.map_err(err)?;
        drop(state);
        Self::alarms_changed(&emitter).await?;
        Ok(())
    }

    async fn set_alarm_enabled(
        &self,
        id: String,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let mut state = self.core.state.lock().await;
        let alarm = state
            .alarms
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("no alarm {id}")))?;
        alarm.enabled = enabled;
        alarm.snoozed_until = None;
        self.core.commit(&state).await.map_err(err)?;
        drop(state);
        Self::alarms_changed(&emitter).await?;
        Ok(())
    }

    /// Returns a(ssyyyb): id, label, hour, minute, days, enabled.
    async fn list_alarms(&self) -> Vec<(String, String, u8, u8, u8, bool)> {
        let state = self.core.state.lock().await;
        state
            .alarms
            .iter()
            .map(|a| (a.id.clone(), a.label.clone(), a.hour, a.minute, a.days, a.enabled))
            .collect()
    }

    // ---------- timers ----------

    async fn add_timer(&self, seconds: u32, label: String) -> fdo::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut state = self.core.state.lock().await;
        state.timers.push(Timer {
            id: id.clone(),
            label,
            ends_at: Local::now().timestamp() + i64::from(seconds),
        });
        self.core.commit(&state).await.map_err(err)?;
        Ok(id)
    }

    async fn remove_timer(&self, id: String) -> fdo::Result<()> {
        self.core.dismiss(&id).await;
        let mut state = self.core.state.lock().await;
        state.timers.retain(|t| t.id != id);
        self.core.commit(&state).await.map_err(err)?;
        Ok(())
    }

    /// Returns a(ssx): id, label, end time (Unix seconds).
    async fn list_timers(&self) -> Vec<(String, String, i64)> {
        let state = self.core.state.lock().await;
        state.timers.iter().map(|t| (t.id.clone(), t.label.clone(), t.ends_at)).collect()
    }

    // ---------- while ringing ----------

    async fn snooze(&self, id: String, minutes: u32) -> fdo::Result<()> {
        self.core.snooze(&id, minutes.clamp(1, 60)).await.map_err(err)?;
        Ok(())
    }

    async fn dismiss(&self, id: String) {
        self.core.dismiss(&id).await;
    }

    // ---------- for the launcher / status bar ----------

    /// Unix timestamp of the next alarm, or 0 if none. Emits
    /// PropertiesChanged, so a status bar can just watch it.
    #[zbus(property)]
    async fn next_alarm(&self) -> i64 {
        let state = self.core.state.lock().await;
        Core::next_alarm(&state, Local::now()).map_or(0, |t| t.timestamp())
    }

    // ---------- signals ----------

    #[zbus(signal)]
    pub async fn alarms_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn alarm_ringing(emitter: &SignalEmitter<'_>, id: &str, label: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn timer_finished(emitter: &SignalEmitter<'_>, id: &str, label: &str) -> zbus::Result<()>;
}

//! The D-Bus API your clock UI and launcher talk to.
//!
//!   service:   org.mechanix.services.Clock
//!   object:    /org/mechanix/services/Clock
//!   interface: org.mechanix.services.Clock
//!
//! Try it with busctl:
//!   busctl --user call org.mechanix.services.Clock /org/mechanix/services/Clock \
//!       org.mechanix.services.Clock AddAlarm syyysuu "Work" 7 30 31 "" 5 10
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

/// The editable fields of an alarm, as the API takes them.
struct AlarmFields {
    label: String,
    hour: u8,
    minute: u8,
    days: u8,
    sound: Option<String>,
    ring_minutes: u32,
    snooze_minutes: u32,
}

impl AlarmFields {
    #[allow(clippy::too_many_arguments)]
    fn check(
        label: String,
        hour: u8,
        minute: u8,
        days: u8,
        sound: String,
        ring_minutes: u32,
        snooze_minutes: u32,
    ) -> fdo::Result<Self> {
        let invalid = |msg: String| Err(fdo::Error::InvalidArgs(msg));
        if hour > 23 || minute > 59 || days > 0x7f {
            return invalid("hour/minute/days out of range".into());
        }
        if !(1..=60).contains(&ring_minutes) || !(1..=60).contains(&snooze_minutes) {
            return invalid("ring and snooze minutes must be 1..=60".into());
        }
        // Empty means the default sound. Check the file now, so the app can
        // tell the user; if it goes away later the default plays instead.
        let sound = if sound.is_empty() {
            None
        } else if !sound.starts_with('/') {
            return invalid(format!("sound must be an absolute path: {sound}"));
        } else if let Err(e) = std::fs::File::open(&sound) {
            return invalid(format!("can't read sound {sound}: {e}"));
        } else {
            Some(sound)
        };
        Ok(Self { label, hour, minute, days, sound, ring_minutes, snooze_minutes })
    }

    fn into_alarm(self, id: String) -> Alarm {
        Alarm {
            id,
            label: self.label,
            hour: self.hour,
            minute: self.minute,
            days: self.days,
            enabled: true,
            snoozed_until: None,
            sound: self.sound,
            ring_minutes: self.ring_minutes,
            snooze_minutes: self.snooze_minutes,
        }
    }

    fn apply(self, alarm: &mut Alarm) {
        alarm.label = self.label;
        alarm.hour = self.hour;
        alarm.minute = self.minute;
        alarm.days = self.days;
        alarm.sound = self.sound;
        alarm.ring_minutes = self.ring_minutes;
        alarm.snooze_minutes = self.snooze_minutes;
    }
}

#[interface(name = "org.mechanix.services.Clock")]
impl ClockdIface {
    // ---------- alarms ----------

    /// Add an alarm; returns its id.
    ///   `days`: bitmask, bit 0 = Monday ... bit 6 = Sunday, 0 = one-shot.
    ///   `sound`: absolute path to an audio file, or "" for the default.
    ///   `ring_minutes`: stop ringing on its own after this long (1..=60).
    ///   `snooze_minutes`: how long Snooze puts it off (1..=60).
    #[allow(clippy::too_many_arguments)]
    async fn add_alarm(
        &self,
        label: String,
        hour: u8,
        minute: u8,
        days: u8,
        sound: String,
        ring_minutes: u32,
        snooze_minutes: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let fields =
            AlarmFields::check(label, hour, minute, days, sound, ring_minutes, snooze_minutes)?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut state = self.core.state.lock().await;
        state.alarms.push(fields.into_alarm(id.clone()));
        self.core.commit(&state).await.map_err(err)?;
        drop(state);
        Self::alarms_changed(&emitter).await?;
        Ok(id)
    }

    /// Replace every editable field of an alarm (the app's edit form saves
    /// them all at once). Takes the same arguments as AddAlarm. The alarm is
    /// enabled again and any snooze is cleared.
    #[allow(clippy::too_many_arguments)]
    async fn update_alarm(
        &self,
        id: String,
        label: String,
        hour: u8,
        minute: u8,
        days: u8,
        sound: String,
        ring_minutes: u32,
        snooze_minutes: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let fields =
            AlarmFields::check(label, hour, minute, days, sound, ring_minutes, snooze_minutes)?;
        let mut state = self.core.state.lock().await;
        let alarm = state
            .alarms
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("no alarm {id}")))?;
        fields.apply(alarm);
        alarm.enabled = true;
        alarm.snoozed_until = None;
        self.core.commit(&state).await.map_err(err)?;
        drop(state);
        Self::alarms_changed(&emitter).await?;
        Ok(())
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

    /// Returns a(ssyyybsuu): id, label, hour, minute, days, enabled,
    /// sound ("" = default), ring minutes, snooze minutes.
    #[allow(clippy::type_complexity)]
    async fn list_alarms(&self) -> Vec<(String, String, u8, u8, u8, bool, String, u32, u32)> {
        let state = self.core.state.lock().await;
        state
            .alarms
            .iter()
            .map(|a| {
                (
                    a.id.clone(),
                    a.label.clone(),
                    a.hour,
                    a.minute,
                    a.days,
                    a.enabled,
                    a.sound.clone().unwrap_or_default(),
                    a.ring_minutes,
                    a.snooze_minutes,
                )
            })
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

    /// Put a ringing alarm off by its own snooze length.
    async fn snooze(&self, id: String) -> fdo::Result<()> {
        self.core.snooze(&id).await.map_err(err)?;
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

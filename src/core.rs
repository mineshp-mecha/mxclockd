//! Shared daemon state. The D-Bus interface mutates it; the scheduler loop
//! reads it and decides what rings when. `changed` wakes the scheduler.

use crate::model::{Alarm, Timer};
use crate::ring::{NotificationsProxy, Ringing};
use crate::store::State;
use anyhow::Result;
use chrono::{DateTime, Duration, Local};
use std::collections::HashMap;
use tokio::sync::{Mutex, Notify};

/// If the machine was off/asleep and an alarm was missed by more than this,
/// don't ring it late; just log it.
const MISSED_GRACE: Duration = Duration::minutes(30);

pub enum Due {
    Alarm { id: String, label: String, sound: Option<String> },
    Timer { id: String, label: String },
}

pub struct Core {
    pub state: Mutex<State>,
    pub changed: Notify,
    ringing: Mutex<HashMap<String, Ringing>>,
    pub notifications: NotificationsProxy<'static>,
}

impl Core {
    pub fn new(state: State, notifications: NotificationsProxy<'static>) -> Self {
        Self {
            state: Mutex::new(state),
            changed: Notify::new(),
            ringing: Mutex::new(HashMap::new()),
            notifications,
        }
    }

    /// Persist, then wake the scheduler so it recomputes the next deadline.
    pub async fn commit(&self, state: &State) -> Result<()> {
        state.save()?;
        self.changed.notify_one();
        Ok(())
    }

    /// Earliest upcoming alarm or timer.
    pub fn next_deadline(state: &State, now: DateTime<Local>) -> Option<DateTime<Local>> {
        let alarms = state.alarms.iter().filter_map(|a| a.next_fire(now));
        let timers = state
            .timers
            .iter()
            .filter_map(|t| DateTime::from_timestamp(t.ends_at, 0))
            .map(|t| t.with_timezone(&Local));
        alarms.chain(timers).min()
    }

    pub fn next_alarm(state: &State, now: DateTime<Local>) -> Option<DateTime<Local>> {
        state.alarms.iter().filter_map(|a| a.next_fire(now)).min()
    }

    /// Everything that should have fired in (since, now]. Updates the state
    /// (one-shot alarms get disabled, finished timers removed, snoozes cleared).
    pub async fn take_due(&self, since: DateTime<Local>, now: DateTime<Local>) -> Result<Vec<Due>> {
        let mut state = self.state.lock().await;
        let mut due = Vec::new();

        for a in state.alarms.iter_mut() {
            let Some(at) = a.next_fire(since) else { continue };
            if at > now {
                continue;
            }
            a.snoozed_until = None;
            if a.days == 0 {
                a.enabled = false; // one-shot
            }
            if now - at > MISSED_GRACE {
                tracing::info!("missed alarm {} at {at}, not ringing late", a.id);
                continue;
            }
            due.push(Due::Alarm { id: a.id.clone(), label: a.label.clone(), sound: a.sound.clone() });
        }

        let now_ts = now.timestamp();
        let (finished, running): (Vec<Timer>, Vec<Timer>) =
            std::mem::take(&mut state.timers).into_iter().partition(|t| t.ends_at <= now_ts);
        state.timers = running;
        due.extend(finished.into_iter().map(|t| Due::Timer { id: t.id, label: t.label }));

        if !due.is_empty() {
            state.save()?;
        }
        Ok(due)
    }

    pub async fn start_ringing(&self, id: &str, summary: &str, body: &str, sound: Option<&str>) {
        let r = Ringing::start(&self.notifications, summary, body, sound).await;
        if let Some(old) = self.ringing.lock().await.insert(id.to_owned(), r) {
            old.stop(&self.notifications).await;
        }
    }

    /// Stop ringing. Returns false if `id` wasn't ringing.
    pub async fn dismiss(&self, id: &str) -> bool {
        let r = self.ringing.lock().await.remove(id);
        match r {
            Some(r) => {
                r.stop(&self.notifications).await;
                true
            }
            None => false,
        }
    }

    pub async fn snooze(&self, id: &str, minutes: u32) -> Result<bool> {
        let was_ringing = self.dismiss(id).await;
        let mut state = self.state.lock().await;
        let Some(alarm) = state.alarms.iter_mut().find(|a: &&mut Alarm| a.id == id) else {
            return Ok(false);
        };
        alarm.enabled = true;
        alarm.snoozed_until = Some((Local::now() + Duration::minutes(minutes.into())).timestamp());
        self.commit(&state).await?;
        Ok(was_ringing)
    }

    /// Map a notification button click back to the alarm it belongs to.
    pub async fn id_for_notification(&self, notification_id: u32) -> Option<String> {
        self.ringing
            .lock()
            .await
            .iter()
            .find(|(_, r)| r.notification_id == Some(notification_id))
            .map(|(id, _)| id.clone())
    }
}

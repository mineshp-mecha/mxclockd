//! Shared daemon state. The D-Bus interface mutates it; the scheduler loop
//! reads it and decides what rings when. `changed` wakes the scheduler.

use crate::model::{Alarm, Timer};
use crate::ring::{LoginManagerProxy, NotificationsProxy, Ringing, CLOSED_BY_USER};
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
    /// `None` if the system bus or logind isn't available.
    login: Option<LoginManagerProxy<'static>>,
}

impl Core {
    pub fn new(
        state: State,
        notifications: NotificationsProxy<'static>,
        login: Option<LoginManagerProxy<'static>>,
    ) -> Self {
        Self {
            state: Mutex::new(state),
            changed: Notify::new(),
            ringing: Mutex::new(HashMap::new()),
            notifications,
            login,
        }
    }

    /// Persist, and wake the scheduler so it recomputes the next deadline.
    /// The in-memory state has already changed, so the scheduler is woken
    /// even if saving fails; otherwise e.g. a snooze would never ring.
    pub async fn commit(&self, state: &State) -> Result<()> {
        let saved = state.save();
        self.changed.notify_one();
        saved
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

    /// Everything that should have fired since the last check, up to `now`.
    /// Updates and saves the state (one-shot alarms get disabled, finished
    /// timers removed, snoozes cleared, `last_check` moved to `now`).
    pub async fn take_due(&self, now: DateTime<Local>) -> Vec<Due> {
        let mut state = self.state.lock().await;
        let due = collect_due(&mut state, now);
        // Not fatal: the worst case is re-ringing after a restart, which is
        // better than the scheduler (and every alarm) stopping.
        if let Err(e) = state.save() {
            tracing::warn!("could not save state: {e:#}");
        }
        due
    }

    pub async fn start_ringing(&self, id: &str, summary: &str, body: &str, sound: Option<&str>) {
        let r = Ringing::start(&self.notifications, self.login.as_ref(), summary, body, sound).await;
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

    /// The notification of something still ringing was closed. If the user
    /// closed it they meant to stop it; otherwise (expired, closed by the
    /// server) show it again, or the sound would play with no way to stop it.
    pub async fn notification_closed(&self, notification_id: u32, reason: u32) {
        let Some(id) = self.id_for_notification(notification_id).await else { return };
        if reason == CLOSED_BY_USER {
            tracing::info!("notification for {id} closed by the user, dismissing");
            self.dismiss(&id).await;
            return;
        }
        let mut ringing = self.ringing.lock().await;
        let Some(r) = ringing.get_mut(&id) else { return };
        if r.reshow(&self.notifications).await {
            tracing::info!("notification for {id} closed (reason {reason}), showing it again");
        } else {
            tracing::warn!("notification for {id} keeps being closed (reason {reason}); not re-showing");
        }
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

/// The pure part of `take_due`: what is due in (state.last_check, now].
fn collect_due(state: &mut State, now: DateTime<Local>) -> Vec<Due> {
    // No saved check yet (first run): nothing is "missed". A saved check in
    // the future means the clock went backwards; don't treat that as time passed.
    let since = state
        .last_check
        .and_then(|ts| DateTime::from_timestamp(ts, 0))
        .map(|t| t.with_timezone(&Local).min(now))
        .unwrap_or(now);
    state.last_check = Some(now.timestamp());

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
    due
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn state_with_alarm(hour: u8, minute: u8, days: u8, last_check: DateTime<Local>) -> State {
        State {
            alarms: vec![Alarm {
                id: "a".into(),
                label: String::new(),
                hour,
                minute,
                days,
                enabled: true,
                snoozed_until: None,
                sound: None,
            }],
            timers: vec![],
            last_check: Some(last_check.timestamp()),
        }
    }

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 16, h, m, 0).unwrap()
    }

    #[test]
    fn one_shot_missed_while_off_is_disabled_not_rung_tomorrow() {
        // Off from 06:00 to 08:00; a one-shot alarm at 07:00 was missed by an hour.
        let mut state = state_with_alarm(7, 0, 0, at(6, 0));
        let due = collect_due(&mut state, at(8, 0));
        assert!(due.is_empty());
        assert!(!state.alarms[0].enabled);
    }

    #[test]
    fn short_reboot_rings_late_within_grace() {
        // Rebooted 06:58 -> 07:05; the 07:00 alarm should still ring.
        let mut state = state_with_alarm(7, 0, 0b111_1111, at(6, 58));
        let due = collect_due(&mut state, at(7, 5));
        assert!(matches!(due.as_slice(), [Due::Alarm { .. }]));
        assert_eq!(state.last_check, Some(at(7, 5).timestamp()));
    }

    #[test]
    fn first_run_and_backwards_clock_ring_nothing() {
        let mut state = state_with_alarm(7, 0, 0, at(6, 0));
        state.last_check = None;
        assert!(collect_due(&mut state, at(8, 0)).is_empty());
        assert!(state.alarms[0].enabled);

        // Last checked at 08:00, clock set back to 06:30: nothing has "passed".
        state.last_check = Some(at(8, 0).timestamp());
        assert!(collect_due(&mut state, at(6, 30)).is_empty());
        assert!(state.alarms[0].enabled);
    }
}

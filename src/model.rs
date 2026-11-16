//! Data model: what an alarm is and when it fires next.

use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};

/// Bit flags for repeating days. Bit 0 = Monday ... bit 6 = Sunday.
/// `0` means "one-shot": ring once at the next matching time, then disable.
pub type DayMask = u8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alarm {
    pub id: String,
    pub label: String,
    pub hour: u8,
    pub minute: u8,
    pub days: DayMask,
    pub enabled: bool,
    /// Unix timestamp; when set, it overrides the normal schedule (snooze).
    #[serde(default)]
    pub snoozed_until: Option<i64>,
    /// Sound file to play; `None` means the daemon's default.
    #[serde(default)]
    pub sound: Option<String>,
}

impl Alarm {
    /// The next moment (strictly after `now`) this alarm should ring,
    /// or `None` if it's disabled.
    pub fn next_fire(&self, now: DateTime<Local>) -> Option<DateTime<Local>> {
        if !self.enabled {
            return None;
        }
        if let Some(ts) = self.snoozed_until {
            if let Some(t) = Local.timestamp_opt(ts, 0).single() {
                if t > now {
                    return Some(t);
                }
            }
        }

        let time = NaiveTime::from_hms_opt(self.hour.into(), self.minute.into(), 0)?;
        // Look at today plus the next 7 days; the first match wins.
        for offset in 0..=7 {
            let date = now.date_naive() + Duration::days(offset);
            let weekday_bit = 1u8 << date.weekday().num_days_from_monday();
            if self.days != 0 && self.days & weekday_bit == 0 {
                continue;
            }
            // `earliest()` handles DST: if 02:30 doesn't exist today it
            // returns None and we skip; if it exists twice we take the first.
            if let Some(candidate) = Local.from_local_datetime(&date.and_time(time)).earliest() {
                if candidate > now {
                    return Some(candidate);
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timer {
    pub id: String,
    pub label: String,
    /// Absolute end time as a Unix timestamp, so it survives restarts.
    pub ends_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alarm(h: u8, m: u8, days: DayMask) -> Alarm {
        Alarm {
            id: "t".into(),
            label: String::new(),
            hour: h,
            minute: m,
            days,
            enabled: true,
            snoozed_until: None,
            sound: None,
        }
    }

    #[test]
    fn one_shot_later_today() {
        let now = Local.with_ymd_and_hms(2026, 11, 16, 6, 0, 0).unwrap(); // a Monday
        let next = alarm(7, 30, 0).next_fire(now).unwrap();
        assert_eq!((next.day(), next.hour_min()), (16, (7, 30)));
    }

    #[test]
    fn weekdays_skip_weekend() {
        let fri_evening = Local.with_ymd_and_hms(2026, 11, 20, 20, 0, 0).unwrap();
        let next = alarm(7, 0, 0b0001_1111).next_fire(fri_evening).unwrap();
        assert_eq!(next.weekday(), chrono::Weekday::Mon);
    }

    trait HourMin {
        fn hour_min(&self) -> (u32, u32);
    }
    impl HourMin for DateTime<Local> {
        fn hour_min(&self) -> (u32, u32) {
            use chrono::Timelike;
            (self.hour(), self.minute())
        }
    }
}

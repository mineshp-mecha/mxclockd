//! Sleeping until a wall-clock time, the way an alarm clock needs to.
//!
//! `tokio::time::sleep` uses CLOCK_MONOTONIC, which *stops* during suspend
//! and ignores wall-clock changes. For alarms we want a timerfd on
//! CLOCK_REALTIME_ALARM instead:
//!   * it uses an absolute wall-clock deadline ("07:00", not "in 8h"),
//!   * the kernel programs the RTC so the machine wakes from suspend,
//!   * with TFD_TIMER_CANCEL_ON_SET we're told when the clock is changed
//!     (NTP jump, user sets the time) so we can recompute.
//!
//! CLOCK_REALTIME_ALARM needs CAP_WAKE_ALARM. Without it we fall back to
//! CLOCK_REALTIME: alarms still fire on time while awake, and right after
//! resume, but they won't wake a suspended machine.

use anyhow::Result;
use chrono::{DateTime, Local};
use nix::errno::Errno;
use nix::sys::time::TimeSpec;
use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use tokio::io::{unix::AsyncFd, Interest};

pub enum WakeReason {
    /// The deadline was reached.
    Elapsed,
    /// The system clock was changed; deadlines must be recomputed.
    ClockChanged,
}

/// tokio's AsyncFd wants `AsRawFd`; nix's TimerFd only implements `AsFd`.
struct Tfd(TimerFd);

impl AsRawFd for Tfd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_fd().as_raw_fd()
    }
}

pub struct Waker {
    fd: AsyncFd<Tfd>,
    pub can_wake_system: bool,
}

impl Waker {
    pub fn new() -> Result<Self> {
        let flags = TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC;
        let (timer, can_wake_system) = match TimerFd::new(ClockId::CLOCK_REALTIME_ALARM, flags) {
            Ok(t) => (t, true),
            Err(Errno::EPERM) => {
                tracing::warn!(
                    "no CAP_WAKE_ALARM: alarms will NOT wake the system from suspend"
                );
                (TimerFd::new(ClockId::CLOCK_REALTIME, flags)?, false)
            }
            Err(e) => return Err(e.into()),
        };
        // SAFETY: the Tfd owns the timerfd, so the fd stays open and
        // unchanged for as long as the AsyncFd that owns the Tfd exists.
        let fd = unsafe { AsyncFd::register_with_interest(Tfd(timer), Interest::READABLE) }
            .map_err(|_| anyhow::anyhow!("failed to register timerfd with tokio"))?;
        Ok(Self { fd, can_wake_system })
    }

    /// Sleep until `deadline`, or forever if `None` (callers race this
    /// against a "state changed" notification with `tokio::select!`).
    pub async fn sleep_until(&mut self, deadline: Option<DateTime<Local>>) -> Result<WakeReason> {
        let Some(deadline) = deadline else {
            std::future::pending::<()>().await;
            unreachable!();
        };

        let ts = TimeSpec::new(deadline.timestamp(), 0);
        self.fd.get_ref().0.set(
            Expiration::OneShot(ts),
            TimerSetTimeFlags::TFD_TIMER_ABSTIME | TimerSetTimeFlags::TFD_TIMER_CANCEL_ON_SET,
        )?;

        loop {
            let mut guard = self.fd.readable().await?;
            match guard.try_io(|inner| inner.get_ref().0.wait().map_err(std::io::Error::from)) {
                Ok(Ok(())) => return Ok(WakeReason::Elapsed),
                Ok(Err(e)) if e.raw_os_error() == Some(Errno::ECANCELED as i32) => {
                    return Ok(WakeReason::ClockChanged)
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_would_block) => continue,
            }
        }
    }
}

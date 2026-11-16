//! Rescheduling when the time zone changes.
//!
//! A time zone change doesn't touch CLOCK_REALTIME, so the timerfd's
//! TFD_TIMER_CANCEL_ON_SET never fires for it; "07:00" just silently becomes
//! a different instant. We watch /etc/localtime ourselves instead.
//!
//! We watch /etc rather than the file: timedatectl (and most tools) replace
//! the symlink with a rename, which a watch on the old inode would miss.

use crate::core::Core;
use anyhow::Result;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{unix::AsyncFd, Interest};

/// chrono re-reads /etc/localtime at most once a second per thread, so
/// recomputing right away could still see the old zone.
const SETTLE: Duration = Duration::from_millis(1100);

/// tokio's AsyncFd wants `AsRawFd`; nix's Inotify only implements `AsFd`.
struct Ino(Inotify);

impl AsRawFd for Ino {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_fd().as_raw_fd()
    }
}

pub async fn watch(core: Arc<Core>) -> Result<()> {
    let ino = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)?;
    ino.add_watch(
        "/etc",
        AddWatchFlags::IN_CREATE
            | AddWatchFlags::IN_MOVED_TO
            | AddWatchFlags::IN_CLOSE_WRITE
            | AddWatchFlags::IN_DELETE,
    )?;
    // SAFETY: the Ino owns the inotify fd for as long as the AsyncFd exists.
    let fd = unsafe { AsyncFd::register_with_interest(Ino(ino), Interest::READABLE) }
        .map_err(|_| anyhow::anyhow!("failed to register inotify with tokio"))?;

    loop {
        if !localtime_touched(&fd).await? {
            continue;
        }
        // One change is often several events (create tmp link, rename);
        // let them land, drop the rest, then recompute once.
        tokio::time::sleep(SETTLE).await;
        drain(&fd);
        tracing::info!("time zone changed, rescheduling");
        core.changed.notify_one();
    }
}

async fn localtime_touched(fd: &AsyncFd<Ino>) -> Result<bool> {
    loop {
        let mut guard = fd.readable().await?;
        match guard.try_io(|inner| inner.get_ref().0.read_events().map_err(std::io::Error::from)) {
            Ok(events) => {
                return Ok(events?.iter().any(|e| e.name.as_deref() == Some("localtime".as_ref())))
            }
            Err(_would_block) => continue,
        }
    }
}

fn drain(fd: &AsyncFd<Ino>) {
    // Stops at EAGAIN; tokio's stale readiness is cleared by the next try_io.
    while fd.get_ref().0.read_events().is_ok() {}
}

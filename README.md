# clockd

A minimal alarm/timer daemon for Linux, in Rust. Clock UI and
Launcher are D-Bus clients of it; it keeps working when they're closed.

## Layout

| File            | Role |
|-----------------|------|
| `src/main.rs`   | startup, scheduler loop, notification-button handling |
| `src/dbus.rs`   | the `org.example.Clockd1` D-Bus API |
| `src/core.rs`   | shared state, what's due, ringing/snooze/dismiss |
| `src/model.rs`  | `Alarm` / `Timer` and next-occurrence logic (+ tests) |
| `src/waker.rs`  | timerfd on `CLOCK_REALTIME_ALARM`: wakes from suspend, detects clock changes |
| `src/tzwatch.rs` | inotify on `/etc/localtime`: reschedules when the time zone changes |
| `src/ring.rs`   | freedesktop notification + looping sound (`pw-play`, falling back to `paplay`) + logind sleep inhibitor |
| `src/store.rs`  | JSON persistence in `$XDG_DATA_HOME/clockd/state.json` |
| `data/`         | systemd user unit and D-Bus activation file |

## Run it

    cargo run
    # in another terminal
    busctl --user call org.example.Clockd1 /org/example/Clockd1 org.example.Clockd1 AddTimer us 10 "Tea"
    busctl --user call org.example.Clockd1 /org/example/Clockd1 org.example.Clockd1 AddAlarm yyys 7 30 31 "Work"
    busctl --user get-property org.example.Clockd1 /org/example/Clockd1 org.example.Clockd1 NextAlarm
    busctl --user monitor org.example.Clockd1     # watch AlarmRinging / TimerFinished

`days` is a bitmask: bit 0 = Monday ... bit 6 = Sunday (31 = weekdays, 127 = every day, 0 = once).

## Waking from suspend

`CLOCK_REALTIME_ALARM` needs `CAP_WAKE_ALARM`. Without it the daemon logs a
warning and falls back to `CLOCK_REALTIME` (alarms ring on time while awake,
but don't wake a suspended device). Options:

* Give the binary the file capability:
  `sudo setcap cap_wake_alarm+ep /usr/bin/clockd`. Simple and common on
  single-purpose devices (phones, kiosks).
* Keep the daemon unprivileged and add a small system-bus helper (polkit-guarded)
  that only schedules RTC wakeups.
* On KDE Plasma, ask PowerDevil to schedule the wakeup (what kclockd does).

## Staying awake while ringing

While an alarm or timer rings, the daemon holds a logind inhibitor lock
(`sleep:idle`, mode `block`), so the device isn't suspended again before
someone dismisses it. The lock is released on Dismiss/Snooze, or if the
daemon exits. Check it with `systemd-inhibit --list`.

* The lock only stops suspends that go through logind (`systemctl suspend`,
  `login1.Manager.Suspend`). A launcher that writes `/sys/power/state`
  directly bypasses it.
* Taking a `block` lock is a polkit action
  (`org.freedesktop.login1.inhibit-block-sleep`). If the daemon logs
  `could not inhibit suspend`, add a polkit rule allowing it for the device user.
* Without a system bus or logind, alarms still ring, just without the lock.

## Time zone changes

A time zone change doesn't change `CLOCK_REALTIME`, so the timerfd isn't
cancelled by it. The daemon watches `/etc` for `localtime` being replaced and
reschedules about a second later. If `/etc/localtime` is a symlink into a
writable partition, change the time zone by replacing the link (as
`timedatectl set-timezone` does), not by rewriting the file it points to.

# clockd

A minimal alarm/timer daemon for Linux, in Rust. Clock UI and
Launcher are D-Bus clients of it; it keeps working when they're closed.

## Layout

| File            | Role |
|-----------------|------|
| `src/main.rs`   | startup, scheduler loop, notification-button handling |
| `src/dbus.rs`   | the `org.mechanix.services.Clock` D-Bus API |
| `src/core.rs`   | shared state, what's due, ringing/snooze/dismiss |
| `src/model.rs`  | `Alarm` / `Timer` and next-occurrence logic (+ tests) |
| `src/waker.rs`  | timerfd on `CLOCK_REALTIME_ALARM`: wakes from suspend, detects clock changes |
| `src/tzwatch.rs` | inotify on `/etc/localtime`: reschedules when the time zone changes |
| `src/ring.rs`   | freedesktop notification + looping sound (`pw-play`, falling back to `paplay`) + logind sleep inhibitor |
| `src/store.rs`  | JSON persistence in `$XDG_DATA_HOME/clockd/state.json` (fsync'd; a corrupt file is moved to `state.json.bad`) |
| `data/`         | systemd user unit and D-Bus activation file |

## Run it

    cargo run
    # in another terminal
    busctl --user call org.mechanix.services.Clock /org/mechanix/services/Clock org.mechanix.services.Clock AddTimer us 10 "Tea"
    busctl --user call org.mechanix.services.Clock /org/mechanix/services/Clock org.mechanix.services.Clock AddAlarm syyysuu "Work" 7 30 31 "" 5 10
    busctl --user get-property org.mechanix.services.Clock /org/mechanix/services/Clock org.mechanix.services.Clock NextAlarm
    busctl --user monitor org.mechanix.services.Clock     # watch AlarmRinging / TimerFinished

`AddAlarm` takes: label, hour, minute, days, sound, ring minutes, snooze
minutes. `UpdateAlarm` takes the
alarm id followed by the same fields, and `ListAlarms` returns them all.

* `days` is a bitmask: bit 0 = Monday ... bit 6 = Sunday (31 = weekdays,
  127 = every day, 0 = once). Monday + Tuesday + Friday = 1 + 2 + 16 = 19.
* `sound` is an absolute path to an audio file, or `""` for the default
  (`/usr/share/sounds/freedesktop/stereo/alarm-clock-elapsed.oga`). It must be
  readable when the alarm is saved; if it's gone by the time the alarm rings,
  the default plays instead. Prefer WAV/OGG/FLAC (`pw-play` uses libsndfile).
* Ring minutes (1..=60): the alarm stops ringing on its own after this long.
  Timers ring for 5 minutes.
* Snooze minutes (1..=60): used by the notification's Snooze button and by
  `Snooze(id)`.

## Waking from suspend

`CLOCK_REALTIME_ALARM` needs `CAP_WAKE_ALARM`. Without it the daemon logs a
warning and falls back to `CLOCK_REALTIME` (alarms ring on time while awake,
but don't wake a suspended device). Options:

* Give the binary the file capability:
  `sudo setcap cap_wake_alarm+ep /usr/bin/clockd`. Simple and common on
  single-purpose devices (phones, kiosks).
* Keep the daemon unprivileged and add a small system-bus helper (polkit-guarded)
  that only schedules RTC wakeups.

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

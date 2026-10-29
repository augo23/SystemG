# SystemG (System Generic)

SystemG, short for System Generic: systemd-compatible interfaces on top of
real, swappable Unix tools, written in Rust. Software that expects systemd
keeps working, while dinit and friends do the actual work.

## Why

systemd has grown from an init system into one project that is also the
service manager, logging, logins, device management, DNS, time sync,
networking and more, all tied to PID 1. SystemG brings Linux back to the Unix
philosophy: each of those jobs is done by a small, separate tool that does
one thing and can be replaced on its own. SystemG itself is only the
compatibility layer that lets software written for systemd run on them.
That keeps the code in the most privileged places (PID 1, the service
manager) small.

It was made for [SunairiesOS](https://github.com/augo23/SunairiesOS),
Debian without systemd, which shows how the pieces are put together.

## What replaces each part of systemd

| systemd | With SystemG |
|---|---|
| `systemd` as PID 1, the service manager | **dinit** |
| unit files (`.service`, `.socket`, `.target`) | translated to dinit services by `sg-dinit` (`systemctl daemon-reload`) |
| `systemctl` | SystemG's `systemctl`: start, stop, restart, status, enable, disable, show, list-units, daemon-reload, poweroff, reboot, ... |
| `systemd --user` | SystemG's per-user manager, started at login, translating user units the same way |
| `dbus-user-session` | the session bus that per-user manager provides |
| `systemd-logind`, `loginctl` | **elogind** (with its own `loginctl`) |
| `systemd-udevd`, `udevadm` | **eudev** |
| `systemd-journald`, `journalctl` | no journal: each service's output goes to its own plain-text log, `/var/log/systemg/<service>.log` |
| `sd_notify`, `sd_listen_fds`, `sd_booted` (libsystemd) | `libsd-shim`, and `sg-notify-proxy` for `Type=notify` daemons |
| the `org.freedesktop.systemd1` D-Bus API | `compatd` |
| D-Bus activation (`SystemdService=`) | through dinit |
| `systemd-sysusers`, `systemd-tmpfiles` | the standalone versions of these one-shot tools |
| `systemd-networkd`, `systemd-resolved`, `systemd-timesyncd` | not part of SystemG: use NetworkManager or any other network tool |

## Layers

SystemG is built in layers, each a crate. Only the bottom one knows about dinit:

| Layer | Crate | What it is |
|---|---|---|
| Units | `unitparse` | systemd unit file parser: drop-ins, continuations, exec lines, specifiers, time spans |
| Backend interface | `sg-core` | what a service manager must provide (`ServiceManager`), so dinit could be swapped for another |
| Backend | `sg-dinit` | the dinit backend: unit → dinit service translation, and control through `dinitctl` |
| Front ends | `systemctl` | the `systemctl` command, on top of the backend |
| | `compatd` | the `org.freedesktop.systemd1` D-Bus service |
| | `libsd-shim` | a `libsystemd.so.0` replacement: `sd_notify`, `sd_listen_fds`, `sd_booted`, `sd_watchdog_enabled` |
| | `sg-notify-proxy` | bridges `Type=notify` (`$NOTIFY_SOCKET`) to dinit's readiness pipe, for any daemon |

```sh
cargo test --workspace
```

`systemctl` honours these for testing without root: `SYSTEMG_DINIT_DIR`,
`SYSTEMG_DINITCTL`, `SYSTEMG_UNIT_PATH`, `SYSTEMG_VERBOSE=1`.

## License

GPL-3.0 (see `LICENSE`).

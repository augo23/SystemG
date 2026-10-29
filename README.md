# SystemG (System Generic)

SystemG, short for System Generic: systemd-compatible interfaces on top of real, swappable Unix tools, written
in Rust. Software that expects systemd keeps working, while dinit and
friends do the actual work.

## Crates

| Crate | What it is |
|---|---|
| `unitparse` | systemd unit file parser (drop-ins, continuations, exec lines, time spans) |
| `sg-core` | Backend traits (`ServiceManager`) |
| `sg-dinit` | dinit backend: unit → dinit translation and `dinitctl` control |
| `systemctl` | `systemctl` CLI on top of the backend |
| `libsd-shim` | `libsystemd.so.0` replacement: `sd_notify`, `sd_listen_fds`, `sd_booted` |
| `sg-notify-proxy` | Bridges `Type=notify` (`$NOTIFY_SOCKET`) to dinit's readiness pipe, for any daemon |
| `compatd` | `org.freedesktop.systemd1` D-Bus service |

```sh
cargo test --workspace
```

`systemctl` honours these for testing without root: `SYSTEMG_DINIT_DIR`,
`SYSTEMG_DINITCTL`, `SYSTEMG_UNIT_PATH`, `SYSTEMG_VERBOSE=1`.

## License

GPL-3.0 (see LICENSE).

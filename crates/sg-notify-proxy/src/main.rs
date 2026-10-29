//! `sg-notify-proxy CMD [ARGS...]`
//!
//! Lets `Type=notify` daemons run under dinit. Many daemons (sshd, most Go and
//! Rust daemons) implement systemd's readiness protocol themselves: they send
//! `READY=1` as a datagram to `$NOTIFY_SOCKET`. dinit instead waits for a byte
//! on a pipe whose fd it passes in `$SYSTEMG_READY_FD`.
//!
//! The proxy binds a socket file (like systemd's `/run/systemd/notify`; some
//! daemons, sshd among them, accept only a path there), forks a small relay that waits for
//! `READY=1` and writes to dinit's pipe, then `exec`s the daemon with
//! `NOTIFY_SOCKET` set. The daemon keeps the proxy's PID, which is the PID dinit
//! started, so supervision is unaffected. The relay stays until the daemon is
//! gone, so the socket is there for everything it sends after `READY=1`.

use std::ffi::c_int;
use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
use std::time::Duration;

const READY_FD_VAR: &str = "SYSTEMG_READY_FD";
const F_GETFD: c_int = 1;
const F_SETFD: c_int = 2;
const FD_CLOEXEC: c_int = 1;

extern "C" {
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn getppid() -> c_int;
    fn close(fd: c_int) -> c_int;
    fn getuid() -> u32;
}

fn set_cloexec(fd: RawFd, on: bool) {
    unsafe {
        let flags = fcntl(fd, F_GETFD);
        if flags >= 0 {
            fcntl(
                fd,
                F_SETFD,
                if on {
                    flags | FD_CLOEXEC
                } else {
                    flags & !FD_CLOEXEC
                },
            );
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--relay") {
        return relay(&args[1..]);
    }
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        eprintln!("usage: sg-notify-proxy CMD [ARGS...]");
        return ExitCode::from(2);
    }

    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);

    // Without a readiness pipe there is nothing to relay; just run the command.
    let ready_fd: Option<RawFd> = std::env::var(READY_FD_VAR)
        .ok()
        .and_then(|v| v.parse().ok());
    if let Some(fd) = ready_fd {
        match start_relay(fd) {
            Ok(name) => {
                cmd.env("NOTIFY_SOCKET", name);
                cmd.env_remove(READY_FD_VAR);
                unsafe { close(fd) }; // only the relay should hold the pipe
            }
            Err(e) => eprintln!(
                "sg-notify-proxy: relay unavailable ({e}); readiness will not be reported"
            ),
        }
    }

    let err = cmd.exec();
    eprintln!("sg-notify-proxy: cannot execute {}: {err}", args[0]);
    ExitCode::from(127)
}

/// Where notify sockets go: /run/systemg/notify for system services,
/// $XDG_RUNTIME_DIR/systemg/notify for a user's services.
fn socket_dir() -> std::path::PathBuf {
    let base = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(rt) if unsafe { getuid() } != 0 && !rt.is_empty() => std::path::PathBuf::from(rt),
        _ => "/run".into(),
    };
    base.join("systemg/notify")
}

/// Bind the socket here (so it exists before the daemon starts) and hand it to
/// a relay process. Returns the value for `$NOTIFY_SOCKET`: a socket file, or
/// an abstract name (`@...`) if no directory is writable.
fn start_relay(ready_fd: RawFd) -> std::io::Result<String> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::fs::PermissionsExt;
    let pid = std::process::id();
    let dir = socket_dir();
    let path = dir.join(pid.to_string());
    let bound = std::fs::create_dir_all(&dir).and_then(|_| {
        let _ = std::fs::remove_file(&path);
        let s = UnixDatagram::bind(&path)?;
        // The daemon may drop privileges (User=) before it reports readiness.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        Ok(s)
    });
    let (sock, name, file) = match bound {
        Ok(s) => (s, path.to_string_lossy().into_owned(), Some(path)),
        Err(_) => {
            let name = format!("systemg-notify-{pid}");
            let s = UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes())?)?;
            (s, format!("@{name}"), None)
        }
    };
    set_cloexec(sock.as_raw_fd(), false);
    set_cloexec(ready_fd, false);
    Command::new("/proc/self/exe")
        .arg("--relay")
        .arg(sock.as_raw_fd().to_string())
        .arg(ready_fd.to_string())
        .arg(pid.to_string())
        .args(file.iter())
        .env_remove(READY_FD_VAR)
        .spawn()?;
    Ok(name) // `sock` drops here: the relay keeps its own copy
}

fn relay(args: &[String]) -> ExitCode {
    let parse = |i: usize| args.get(i).and_then(|v| v.parse::<i32>().ok());
    let (Some(sock_fd), Some(ready_fd), Some(daemon_pid)) = (parse(0), parse(1), parse(2)) else {
        return ExitCode::from(2);
    };
    // The socket file goes when the relay does.
    struct Cleanup(Option<String>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(p) = &self.0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    let _cleanup = Cleanup(args.get(3).cloned());
    set_cloexec(sock_fd, true);
    set_cloexec(ready_fd, true);
    let sock = unsafe { UnixDatagram::from_raw_fd(sock_fd) };
    let mut ready = Some(unsafe { std::fs::File::from_raw_fd(ready_fd) });
    let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));

    // As systemd's notify socket, it stays for the daemon's whole life:
    // daemons keep sending (STATUS=, WATCHDOG=1, FDSTORE=1, STOPPING=1), and
    // some fail when the socket is gone (logind's FDSTORE=1 for each device
    // it hands to a session). After READY=1 the rest is accepted and dropped;
    // file descriptors sent along are closed by the kernel, as nothing
    // receives them.
    let mut buf = [0u8; 4096];
    loop {
        match sock.recv(&mut buf) {
            Ok(n) => {
                let msg = String::from_utf8_lossy(&buf[..n]);
                if msg.lines().any(|l| l == "READY=1") {
                    if let Some(mut r) = ready.take() {
                        let _ = r.write_all(b"\n");
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // The daemon is our parent; if we were reparented it is gone.
                if unsafe { getppid() } != daemon_pid {
                    return ExitCode::SUCCESS;
                }
            }
            Err(_) => return ExitCode::FAILURE,
        }
    }
}

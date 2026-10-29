//! Drop-in replacement for the parts of `libsystemd.so.0` that daemons use to
//! talk to their service manager.
//!
//! - `sd_notify("READY=1")`: under dinit, writes to the readiness pipe that
//!   dinit passes in `$SYSTEMG_READY_FD` (`ready-notification = pipevar:...`).
//!   If a real `$NOTIFY_SOCKET` is set instead, sends the datagram exactly as
//!   systemd's library does, so the shim also works under systemd.
//! - `sd_listen_fds`: reads `$LISTEN_PID`/`$LISTEN_FDS`, which dinit's
//!   `socket-listen` sets the same way systemd does.
//! - `sd_booted`: true when `/run/systemd/system` exists (SystemG creates it).
//!
//! Return values follow libsystemd: positive on success, 0 when there is no
//! service manager to talk to, negative errno on error.

use std::ffi::{c_char, c_int, CStr};
use std::io::Write;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixDatagram;

const READY_FD_VAR: &str = "SYSTEMG_READY_FD";
const EINVAL: c_int = 22;

fn errno_of(e: &std::io::Error) -> c_int {
    -(e.raw_os_error().unwrap_or(5 /* EIO */))
}

/// Core of `sd_notify`, callable from Rust for testing.
pub fn notify(state: &str, unset_environment: bool) -> c_int {
    let result = notify_inner(state);
    if unset_environment {
        std::env::remove_var(READY_FD_VAR);
        std::env::remove_var("NOTIFY_SOCKET");
    }
    result
}

fn notify_inner(state: &str) -> c_int {
    if let Some(fd) = std::env::var(READY_FD_VAR)
        .ok()
        .and_then(|v| v.parse::<c_int>().ok())
    {
        if !state.lines().any(|l| l == "READY=1") {
            return 1; // STATUS=, WATCHDOG= etc. have no dinit equivalent; accept quietly
        }
        // Borrow the fd without closing it: the caller may notify again.
        let mut f = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
        return match f.write_all(b"\n") {
            Ok(()) => {
                // dinit only reads one notification; close it so it sees EOF and
                // the daemon does not keep a dangling fd.
                drop(std::mem::ManuallyDrop::into_inner(f));
                std::env::remove_var(READY_FD_VAR);
                1
            }
            Err(e) => errno_of(&e),
        };
    }

    let Ok(path) = std::env::var("NOTIFY_SOCKET") else {
        return 0;
    };
    let sock = match UnixDatagram::unbound() {
        Ok(s) => s,
        Err(e) => return errno_of(&e),
    };
    let sent = if let Some(name) = path.strip_prefix('@') {
        abstract_addr(name).and_then(|a| sock.send_to_addr(state.as_bytes(), &a))
    } else if path.starts_with('/') {
        sock.send_to(state.as_bytes(), &path)
    } else {
        return -EINVAL;
    };
    match sent {
        Ok(_) => 1,
        Err(e) => errno_of(&e),
    }
}

#[cfg(target_os = "linux")]
fn abstract_addr(name: &str) -> std::io::Result<std::os::unix::net::SocketAddr> {
    use std::os::linux::net::SocketAddrExt;
    std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
}

#[cfg(not(target_os = "linux"))]
fn abstract_addr(_: &str) -> std::io::Result<std::os::unix::net::SocketAddr> {
    Err(std::io::Error::from_raw_os_error(EINVAL))
}

/// Core of `sd_listen_fds`.
pub fn listen_fds(unset_environment: bool) -> c_int {
    let pid_ok = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        .is_some_and(|p| p == std::process::id());
    let n = if pid_ok {
        std::env::var("LISTEN_FDS")
            .ok()
            .and_then(|n| n.parse::<c_int>().ok())
            .unwrap_or(0)
    } else {
        0
    };
    if unset_environment {
        for v in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
            std::env::remove_var(v);
        }
    }
    if n < 0 {
        -EINVAL
    } else {
        n
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        CStr::from_ptr(p).to_str().ok()
    }
}

/// # Safety
/// `state` must be null or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn sd_notify(unset_environment: c_int, state: *const c_char) -> c_int {
    match cstr(state) {
        Some(s) => notify(s, unset_environment != 0),
        None => -EINVAL,
    }
}

/// # Safety
/// `state` must be null or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn sd_pid_notify(
    _pid: c_int,
    unset_environment: c_int,
    state: *const c_char,
) -> c_int {
    sd_notify(unset_environment, state)
}

/// # Safety
/// `state` must be null or a valid NUL-terminated string. Passed fds are ignored.
#[no_mangle]
pub unsafe extern "C" fn sd_pid_notify_with_fds(
    _pid: c_int,
    unset_environment: c_int,
    state: *const c_char,
    _fds: *const c_int,
    _n_fds: u32,
) -> c_int {
    sd_notify(unset_environment, state)
}

#[no_mangle]
pub extern "C" fn sd_listen_fds(unset_environment: c_int) -> c_int {
    listen_fds(unset_environment != 0)
}

extern "C" {
    fn malloc(size: usize) -> *mut std::ffi::c_void;
    fn strdup(s: *const c_char) -> *mut c_char;
}

/// # Safety
/// `names` must be null or point to writable storage for one pointer. The
/// returned array and strings are allocated with malloc; the caller frees them.
#[no_mangle]
pub unsafe extern "C" fn sd_listen_fds_with_names(
    unset_environment: c_int,
    names: *mut *mut *mut c_char,
) -> c_int {
    let fdnames = std::env::var("LISTEN_FDNAMES").ok();
    let n = listen_fds(unset_environment != 0);
    if n <= 0 || names.is_null() {
        return n;
    }
    let list: Vec<&str> = fdnames
        .as_deref()
        .map(|s| s.split(':').collect())
        .unwrap_or_default();
    let arr = malloc(std::mem::size_of::<*mut c_char>() * (n as usize + 1)) as *mut *mut c_char;
    if arr.is_null() {
        return -12; // ENOMEM
    }
    for i in 0..n as usize {
        let name = list
            .get(i)
            .copied()
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown");
        let c = std::ffi::CString::new(name).unwrap_or_else(|_| c"unknown".into());
        *arr.add(i) = strdup(c.as_ptr());
    }
    *arr.add(n as usize) = std::ptr::null_mut();
    *names = arr;
    n
}

#[no_mangle]
pub extern "C" fn sd_booted() -> c_int {
    std::path::Path::new("/run/systemd/system").is_dir() as c_int
}

/// Watchdog is not supported by dinit; report it disabled so daemons don't ping.
#[no_mangle]
pub extern "C" fn sd_watchdog_enabled(_unset_environment: c_int, _usec: *mut u64) -> c_int {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::io::IntoRawFd;

    // Environment is process-global; run the env-dependent checks in one test.
    #[test]
    fn notify_and_listen() {
        std::env::remove_var(READY_FD_VAR);
        std::env::remove_var("NOTIFY_SOCKET");
        assert_eq!(notify("READY=1", false), 0, "no manager -> 0");

        // dinit readiness pipe
        let (mut r, w) = std::io::pipe().unwrap();
        let wfd = w.into_raw_fd();
        std::env::set_var(READY_FD_VAR, wfd.to_string());
        assert_eq!(notify("STATUS=warming up", false), 1);
        assert_eq!(notify("STATUS=ok\nREADY=1", false), 1);
        let mut buf = String::new();
        r.read_to_string(&mut buf).unwrap(); // EOF proves the fd was closed
        assert_eq!(buf, "\n");
        assert!(std::env::var(READY_FD_VAR).is_err());

        // real NOTIFY_SOCKET
        let dir = std::env::temp_dir().join(format!("sdshim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notify");
        let server = UnixDatagram::bind(&path).unwrap();
        std::env::set_var("NOTIFY_SOCKET", &path);
        assert_eq!(notify("READY=1", true), 1);
        let mut b = [0u8; 64];
        let n = server.recv(&mut b).unwrap();
        assert_eq!(&b[..n], b"READY=1");
        assert!(
            std::env::var("NOTIFY_SOCKET").is_err(),
            "unset_environment honored"
        );
        std::fs::remove_dir_all(dir).unwrap();

        // socket activation
        std::env::set_var("LISTEN_PID", std::process::id().to_string());
        std::env::set_var("LISTEN_FDS", "2");
        assert_eq!(listen_fds(false), 2);
        std::env::set_var("LISTEN_PID", "1");
        assert_eq!(listen_fds(true), 0, "fds meant for another pid");
        assert!(std::env::var("LISTEN_FDS").is_err());
    }
}

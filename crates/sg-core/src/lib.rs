//! Backend traits shared by every SystemG component.
//!
//! SystemG speaks systemd's interfaces upward (D-Bus, `systemctl`, `sd_notify`)
//! and drives real tools underneath. Each kind of tool sits behind a trait here,
//! so a backend can be swapped (dinit today; s6, runit or OpenRC later) without
//! touching the systemd-facing side.

use std::fmt;

/// systemd's view of a unit's state (`ActiveState` on D-Bus, `systemctl is-active`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveState {
    Active,
    Inactive,
    Activating,
    Deactivating,
    Failed,
}

impl ActiveState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Activating => "activating",
            Self::Deactivating => "deactivating",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for ActiveState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    /// Backend service name (e.g. `nginx`).
    pub name: String,
    pub state: ActiveState,
    pub pid: Option<u32>,
    /// Raw backend output, shown by `systemctl status`.
    pub detail: String,
}

#[derive(Debug)]
pub enum Error {
    /// The service does not exist in the backend.
    NotFound(String),
    /// The backend tool ran but reported failure.
    Backend(String),
    /// The backend daemon is not running (e.g. inside a chroot).
    NotRunning(String),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(n) => write!(f, "Unit {n} not found."),
            Self::Backend(m) | Self::NotRunning(m) => f.write_str(m.trim_end()),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A process supervisor that SystemG maps `org.freedesktop.systemd1` onto.
pub trait ServiceManager {
    fn start(&self, name: &str) -> Result<()>;
    fn stop(&self, name: &str) -> Result<()>;
    fn restart(&self, name: &str) -> Result<()>;
    fn status(&self, name: &str) -> Result<ServiceStatus>;
    /// Start at boot, as a dependency of `target`.
    fn enable(&self, name: &str, target: &str) -> Result<()>;
    fn disable(&self, name: &str, target: &str) -> Result<()>;
    fn is_enabled(&self, name: &str) -> Result<bool>;
    fn list(&self) -> Result<Vec<ServiceStatus>>;
    /// Re-read service definitions after they change on disk.
    fn reload_definitions(&self, name: &str) -> Result<()>;
}

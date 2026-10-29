//! `compatd`: `org.freedesktop.systemd1` on the system D-Bus, as systemd's PID 1
//! provides it, for programs that ask the service manager about the system.
//!
//! So far the Manager's properties that desktops read (GNOME's power daemon:
//! `Virtualization`, to not suspend a virtual machine on its own). Unit
//! control (StartUnit, StopUnit, GetUnit, ...) onto `sg_core::ServiceManager`
//! follows; see ARCHITECTURE.md, milestone 2.

mod virt;

use std::path::Path;
use zbus::{blocking::connection, interface};

struct Manager {
    virtualization: String,
}

#[interface(name = "org.freedesktop.systemd1.Manager")]
impl Manager {
    /// systemd's format: callers parse the leading number.
    #[zbus(property)]
    fn version(&self) -> String {
        format!("257 (SystemG {})", env!("CARGO_PKG_VERSION"))
    }

    #[zbus(property)]
    fn features(&self) -> String {
        "+PAM -SELINUX -APPARMOR -SECCOMP backend=dinit logind=elogind".to_string()
    }

    #[zbus(property)]
    fn virtualization(&self) -> String {
        self.virtualization.clone()
    }

    #[zbus(property)]
    fn architecture(&self) -> String {
        match std::env::consts::ARCH {
            "x86_64" => "x86-64",
            "aarch64" => "arm64",
            "x86" => "x86",
            "arm" => "arm",
            other => other,
        }
        .to_string()
    }

    #[zbus(property)]
    fn system_state(&self) -> String {
        "running".to_string()
    }

    #[zbus(property)]
    fn tainted(&self) -> String {
        String::new()
    }
}

fn main() -> zbus::Result<()> {
    let virtualization = virt::detect(&virt::Facts::read(Path::new("/"))).to_string();
    let _bus = connection::Builder::system()?
        .name("org.freedesktop.systemd1")?
        .serve_at("/org/freedesktop/systemd1", Manager { virtualization })?
        .build()?;
    loop {
        std::thread::park();
    }
}

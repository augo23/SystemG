//! Which hypervisor this runs under, named as `systemd-detect-virt` names it
//! (systemd's Manager.Virtualization property): "" on real hardware.
//!
//! Desktops read it: GNOME's power daemon does not suspend a virtual machine
//! on its own, since a VM often cannot be woken up again.

use std::fs;
use std::path::Path;

/// What the machine tells about itself, gathered from /sys and /proc.
#[derive(Default)]
pub struct Facts {
    pub dmi: Vec<String>, // sys_vendor, product_name, board_vendor, bios_vendor
    pub device_tree: String,
    pub xen: bool,
    pub cpu_hypervisor_flag: bool,
}

impl Facts {
    pub fn read(root: &Path) -> Facts {
        let read = |p: &str| fs::read_to_string(root.join(p)).unwrap_or_default();
        let dmi = ["sys_vendor", "product_name", "board_vendor", "bios_vendor"]
            .iter()
            .map(|f| read(&format!("sys/class/dmi/id/{f}")).trim().to_string())
            .collect();
        Facts {
            dmi,
            device_tree: read("proc/device-tree/compatible").replace('\0', " "),
            xen: root.join("proc/xen").exists() || read("sys/hypervisor/type").trim() == "xen",
            cpu_hypervisor_flag: read("proc/cpuinfo")
                .lines()
                .any(|l| l.starts_with("flags") && l.split_whitespace().any(|w| w == "hypervisor")),
        }
    }
}

/// systemd's name for the hypervisor, or "" when there is none.
pub fn detect(f: &Facts) -> &'static str {
    // DMI first, as systemd does: the vendor names their machine.
    const VENDORS: &[(&str, &str)] = &[
        ("KVM", "kvm"),
        ("Amazon EC2", "amazon"),
        ("QEMU", "qemu"),
        ("VMware", "vmware"),
        ("VMW", "vmware"),
        ("innotek GmbH", "oracle"),
        ("VirtualBox", "oracle"),
        ("Oracle Corporation", "oracle"),
        ("Xen", "xen"),
        ("Bochs", "bochs"),
        ("Parallels", "parallels"),
        ("BHYVE", "bhyve"),
        ("Hyper-V", "microsoft"),
        ("Apple Virtualization", "apple"),
        ("Google Compute Engine", "google"),
    ];
    for value in &f.dmi {
        for (vendor, name) in VENDORS {
            if value.starts_with(vendor) {
                return name;
            }
        }
    }
    // Hyper-V says "Microsoft Corporation" / "Virtual Machine".
    if f.dmi.iter().any(|v| v == "Microsoft Corporation") && f.dmi.iter().any(|v| v == "Virtual Machine") {
        return "microsoft";
    }
    if f.xen {
        return "xen";
    }
    // ARM machines without DMI: QEMU's "virt" board.
    if f.device_tree.contains("linux,dummy-virt") {
        return "qemu";
    }
    if f.cpu_hypervisor_flag {
        return "vm-other";
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dmi(v: &[&str]) -> Facts {
        Facts { dmi: v.iter().map(|s| s.to_string()).collect(), ..Facts::default() }
    }

    #[test]
    fn hypervisors_by_name() {
        assert_eq!(detect(&dmi(&["QEMU", "Standard PC (Q35 + ICH9, 2009)", "", "EFI Development Kit II / OVMF"])), "qemu");
        assert_eq!(detect(&dmi(&["VMware, Inc.", "VMware Virtual Platform"])), "vmware");
        assert_eq!(detect(&dmi(&["innotek GmbH", "VirtualBox"])), "oracle");
        assert_eq!(detect(&dmi(&["Microsoft Corporation", "Virtual Machine"])), "microsoft");
        assert_eq!(detect(&dmi(&["Apple Inc.", "Apple Virtualization Generic Platform"])), "apple");
    }

    #[test]
    fn real_hardware_is_empty() {
        assert_eq!(detect(&dmi(&["LENOVO", "21HD", "LENOVO", "LENOVO"])), "");
        assert_eq!(detect(&dmi(&["Microsoft Corporation", "Surface Laptop 5"])), "");
        assert_eq!(detect(&Facts::default()), "");
    }

    #[test]
    fn arm_device_tree_and_cpu_flag() {
        let virt = Facts { device_tree: "linux,dummy-virt ".into(), ..Facts::default() };
        assert_eq!(detect(&virt), "qemu");
        let other = Facts { cpu_hypervisor_flag: true, ..Facts::default() };
        assert_eq!(detect(&other), "vm-other");
        let xen = Facts { xen: true, ..Facts::default() };
        assert_eq!(detect(&xen), "xen");
    }
}

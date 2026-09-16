//! Detection of **virtualization** (KVM/QEMU/Xen/VMware/…) and of **virtio**
//! devices, so Delonix can extract maximum performance when it runs inside
//! a Linux VM (#1 — "native KVM integration").
//!
//! Everything is read from `/sys` and `/proc` (no external dependencies → works in
//! the static musl binary). Detection never changes anything; the performance
//! tunings are explicit (see [`set_blk_scheduler_none`]).

use std::path::Path;

/// Summary of the virtualization environment of the host where Delonix runs.
#[derive(Debug, Clone, Default)]
pub struct VirtInfo {
    /// `true` if we are inside a VM (any hypervisor).
    pub virtualized: bool,
    /// Hypervisor name: `kvm`, `qemu`, `xen`, `vmware`, `virtualbox`,
    /// `hyper-v`, `unknown` or `none` (bare-metal).
    pub hypervisor: String,
    /// `true` when the guest is KVM-accelerated (maximum performance path).
    pub is_kvm: bool,
    /// `/dev/kvm` present (KVM acceleration available — e.g. nested virtualization).
    pub kvm_accel: bool,
    /// Network interfaces served by `virtio_net` (e.g. `enp1s0`).
    pub virtio_net: Vec<String>,
    /// Disks served by `virtio_blk` (e.g. `vda`).
    pub virtio_blk: Vec<String>,
    /// Total number of devices on the virtio bus (`/sys/bus/virtio/devices`).
    pub virtio_count: usize,
}

fn read_trim(p: &str) -> String {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Decides the hypervisor name from the DMI/CPU signals. Pure function
/// (separated from [`detect`]) so it is testable without a real VM.
fn classify(
    product: &str,
    sys_vendor: &str,
    xen_type: &str,
    hv_flag: bool,
    has_virtio: bool,
) -> String {
    let pl = product.to_lowercase();
    let vl = sys_vendor.to_lowercase();
    if xen_type == "xen" {
        "xen".into()
    } else if pl.contains("kvm") {
        "kvm".into()
    } else if vl.contains("qemu") || vl.contains("red hat") {
        // A QEMU/Red Hat vendor with virtio is, in practice, always KVM-accelerated.
        if has_virtio {
            "kvm".into()
        } else {
            "qemu".into()
        }
    } else if vl.contains("vmware") {
        "vmware".into()
    } else if pl.contains("virtualbox") || vl.contains("innotek") || vl.contains("oracle") {
        "virtualbox".into()
    } else if vl.contains("microsoft") && pl.contains("virtual") {
        "hyper-v".into()
    } else if hv_flag || has_virtio {
        "unknown".into()
    } else {
        "none".into()
    }
}

/// Reads the `device/driver` of a `/sys` entry and returns the driver's basename.
fn driver_of(dir: &Path) -> String {
    std::fs::read_link(dir.join("device/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// Detects the current virtualization environment (cheap; may be called freely).
pub fn detect() -> VirtInfo {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let hv_flag = cpuinfo
        .lines()
        .any(|l| l.starts_with("flags") && l.contains(" hypervisor"));

    // virtio-net
    let mut virtio_net = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        for e in rd.flatten() {
            if driver_of(&e.path()) == "virtio_net" {
                virtio_net.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    // virtio-blk
    let mut virtio_blk = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/block") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if driver_of(&e.path()) == "virtio_blk" || name.starts_with("vd") {
                virtio_blk.push(name);
            }
        }
    }
    virtio_net.sort();
    virtio_blk.sort();
    virtio_blk.dedup();
    let virtio_count = std::fs::read_dir("/sys/bus/virtio/devices")
        .map(|r| r.flatten().count())
        .unwrap_or(0);
    let has_virtio = !virtio_net.is_empty() || !virtio_blk.is_empty() || virtio_count > 0;

    let product = read_trim("/sys/class/dmi/id/product_name");
    let sys_vendor = read_trim("/sys/class/dmi/id/sys_vendor");
    let xen_type = read_trim("/sys/hypervisor/type");
    let hypervisor = classify(&product, &sys_vendor, &xen_type, hv_flag, has_virtio);

    let virtualized = hypervisor != "none";
    let is_kvm = hypervisor == "kvm" || hypervisor == "qemu";
    let kvm_accel = Path::new("/dev/kvm").exists();

    VirtInfo {
        virtualized,
        hypervisor,
        is_kvm,
        kvm_accel,
        virtio_net,
        virtio_blk,
        virtio_count,
    }
}

/// Reads the current I/O scheduler of a disk and says whether it is worth setting `none`.
///
/// Returns `(current, should_set_none)`. On a KVM guest the host already schedules the I/O of the
/// physical disk — keeping a scheduler in the guest only duplicates work and latency,
/// so `none` is the recommended one for `virtio-blk`.
pub fn blk_scheduler(dev: &str) -> Option<(String, bool)> {
    let raw = read_trim(&format!("/sys/block/{dev}/queue/scheduler"));
    if raw.is_empty() {
        return None;
    }
    // Kernel format: "[none] mq-deadline" — the active one is between [].
    let current = raw
        .split_whitespace()
        .find(|t| t.starts_with('['))
        .map(|t| t.trim_matches(|c| c == '[' || c == ']').to_string())
        .unwrap_or_else(|| raw.clone());
    let wants_none = current != "none" && raw.contains("none");
    Some((current, wants_none))
}

/// Applies `none` to the I/O scheduler of a virtio-blk disk (needs root).
pub fn set_blk_scheduler_none(dev: &str) -> std::io::Result<()> {
    std::fs::write(format!("/sys/block/{dev}/queue/scheduler"), "none")
}

#[cfg(test)]
mod tests {
    use super::{classify, driver_of};

    #[test]
    fn driver_of_resolves_symlink() {
        // Sets up `<tmp>/eth0/device -> ../drivers/virtio_net` like the real sysfs
        // (`/sys/class/net/eth0/device/driver` points to the virtio driver).
        let root = std::env::temp_dir().join(format!("delonix-virt-{}", std::process::id()));
        let dev = root.join("eth0");
        let drvdir = root.join("drivers/virtio_net");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::create_dir_all(&drvdir).unwrap();
        // device/ is a dir; device/driver is the symlink to the driver.
        std::fs::create_dir_all(dev.join("device")).unwrap();
        std::os::unix::fs::symlink(&drvdir, dev.join("device/driver")).unwrap();
        assert_eq!(driver_of(&dev), "virtio_net");
        // no symlink → empty string (non-virtio).
        assert_eq!(driver_of(&root.join("nope")), "");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kvm_via_product_name() {
        assert_eq!(classify("KVM", "Red Hat", "", true, true), "kvm");
    }

    #[test]
    fn kvm_via_qemu_vendor_with_virtio() {
        assert_eq!(
            classify("Standard PC (Q35 + ICH9, 2009)", "QEMU", "", true, true),
            "kvm"
        );
    }

    #[test]
    fn qemu_tcg_without_virtio() {
        assert_eq!(classify("Standard PC", "QEMU", "", true, false), "qemu");
    }

    #[test]
    fn vmware_and_vbox_and_hyperv() {
        assert_eq!(
            classify("VMware Virtual Platform", "VMware, Inc.", "", true, false),
            "vmware"
        );
        assert_eq!(
            classify("VirtualBox", "innotek GmbH", "", true, false),
            "virtualbox"
        );
        assert_eq!(
            classify("Virtual Machine", "Microsoft Corporation", "", true, false),
            "hyper-v"
        );
    }

    #[test]
    fn xen_takes_priority() {
        assert_eq!(classify("HVM domU", "Xen", "xen", true, false), "xen");
    }

    #[test]
    fn bare_metal_is_none() {
        assert_eq!(
            classify("HP ZBook Power 15.6", "HP", "", false, false),
            "none"
        );
    }

    #[test]
    fn unknown_hypervisor_flag_only() {
        assert_eq!(
            classify("Some Cloud Instance", "Cloud Co", "", true, false),
            "unknown"
        );
    }
}

//! Live proof of ADR-0068 Phase 1's CPU/memory hotplug ADD against a REAL
//! Cloud Hypervisor VMM — `#[ignore]`, opt-in:
//!
//! ```text
//! cargo test -p delonix-provider-cloud-hypervisor --test live -- --ignored --nocapture
//! ```
//!
//! **No guest OS, and that is deliberate.** An EMPTY qcow2 is enough — the
//! same Annex A spike this ADR is built on
//! (`docs/adr/0068-vm-hotplug.md`) used one, because what Phase 1 proves is
//! the HYPERVISOR side: the ceiling declared at boot takes effect
//! (`--cpus boot=1,max=4`), `vm.resize` is accepted, and the read-back is
//! the VMM's own thread count / `memory_actual_size` (D5), never the
//! request's own answer. Memory ends `partial` ON PURPOSE: with no
//! virtio-mem driver in the guest to claim the blocks, `memory_actual_size`
//! correctly stays at the boot size (D4, Annex A finding 4) — a test that
//! asserted `Complete` here would be asserting the wrong thing, against the
//! very measurement D4 rests on.
//!
//! **No networking either.** This goes straight at the api-socket with
//! `std::process::Command`, exactly as Annex A did, instead of through
//! `CloudHypervisorBackend::boot` (which would attach a tap on this host's
//! shared SDN holder). The two trait methods under test only ever read
//! `vm.api_socket`/`vm.pid`/`vm.vcpus_max`/`vm.memory_max_mib`, so a
//! constructed [`Vm`] with no real network reaches them just as faithfully,
//! and far more safely on a host that runs other people's VMs too.
//!
//! Needs `cloud-hypervisor`, `/dev/kvm` and a known firmware present; skips
//! (does not fail) when any is missing — the same shape
//! `delonix-provider-libvirt`'s own `tests/live.rs` uses.
//!
//! Cleans up unconditionally via `Drop`: the VMM is killed even if an
//! assertion panics, so a failed run never leaves a process behind.

use delonix_compute::vm_backend::{HotplugOutcome, VmBackend};
use delonix_compute::Vm;
use delonix_provider_cloud_hypervisor::{CloudHypervisorBackend, DEFAULT_CH_FIRMWARES};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn tool_missing(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
}

/// The first firmware `DEFAULT_CH_FIRMWARES` names that is actually on disk —
/// same order the backend itself prefers (EDK2 before `rust-hypervisor-fw`).
fn firmware() -> Option<&'static str> {
    DEFAULT_CH_FIRMWARES
        .iter()
        .find(|p| Path::new(p).is_file())
        .copied()
}

/// Kills the VMM on drop, unconditionally — a panicking assertion must not
/// leave a `cloud-hypervisor` process running on this (shared, real) host.
struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore]
fn hotplug_add_reads_back_the_real_vmm_adr_0068() {
    if tool_missing("cloud-hypervisor") || !Path::new("/dev/kvm").exists() {
        eprintln!("skip: no cloud-hypervisor and/or /dev/kvm on this host");
        return;
    }
    let Some(fw) = firmware() else {
        eprintln!("skip: no Cloud Hypervisor firmware installed");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let disk = dir.path().join("boot.qcow2");
    let made = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", &disk.to_string_lossy(), "64M"])
        .output()
        .expect("spawn qemu-img create");
    assert!(made.status.success(), "qemu-img create: {made:?}");

    let sock = dir.path().join("api.sock");
    let serial = dir.path().join("serial.log");
    // Byte for byte Annex A's own A.1/virtio-mem shape: boot=1,max=4 and a
    // 512 MiB virtio-mem region above a 256 MiB boot size. Without `max=`/
    // `hotplug_size=` the VMM refuses ANY resize past the boot values
    // (ADR-0068 D3, measured "Requested vCPUs exceed maximum").
    let child = Command::new("cloud-hypervisor")
        .args([
            "--api-socket",
            &sock.to_string_lossy(),
            "--firmware",
            fw,
            "--disk",
            &format!("path={},image_type=qcow2", disk.display()),
            "--cpus",
            "boot=1,max=4",
            "--memory",
            "size=256M,hotplug_method=virtio-mem,hotplug_size=512M",
            "--serial",
            &format!("file={}", serial.display()),
            "--console",
            "off",
        ])
        .spawn()
        .expect("spawn cloud-hypervisor");
    let pid = child.id() as i32;
    let _guard = Guard(child);

    let deadline = Instant::now() + Duration::from_secs(5);
    while !sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(sock.exists(), "api-socket never appeared");
    // The socket existing is not the VMM answering yet (`wait_vmm_ready`'s
    // own reasoning) — a short grace window before the first real request.
    std::thread::sleep(Duration::from_millis(300));

    let mut vm = Vm::new(
        "dlxhp-live".into(),
        disk.to_string_lossy().into_owned(),
        disk.to_string_lossy().into_owned(),
        1,
        "256M".into(),
        "ingress".into(),
        "n/a".into(),
        "00:00:00:00:00:00".into(),
        sock.to_string_lossy().into_owned(),
    );
    vm.pid = Some(pid);
    vm.vcpus_max = Some(4);
    vm.memory_max_mib = Some(768); // boot 256 + the 512 MiB hotplug region.

    let backend = CloudHypervisorBackend;

    // CPU add: the hypervisor side always works (Annex A finding 2) — the
    // read-back is the VMM's own `vcpuN` thread count (D5), not the
    // request's echo — and Phase 1 always reports it `partial` (D4/Q5: no
    // channel to confirm the GUEST onlined it), never `Complete`, no matter
    // how long `wait` runs.
    let (assigned, outcome) = backend
        .hotplug_add_cpu(dir.path(), &vm, 3, Duration::from_secs(1))
        .expect("hotplug_add_cpu");
    assert_eq!(
        assigned, 3,
        "the VMM's own vcpuN thread count did not reach 3"
    );
    assert!(
        matches!(outcome, HotplugOutcome::Partial { .. }),
        "CH CPU add must never report Complete (ADR-0068 D4/Q5): {outcome:?}"
    );
    vm.vcpus = assigned;

    // A target above the declared ceiling is refused by THIS BACKEND, before
    // it ever reaches the api-socket (D3 — "rather than trust the caller's
    // bookkeeping").
    let over = backend.hotplug_add_cpu(dir.path(), &vm, 5, Duration::from_secs(1));
    assert!(over.is_err(), "5 vCPUs exceeds the declared ceiling of 4");

    // Memory add: the hypervisor accepts it, but with no guest virtio-mem
    // driver to claim the region, `memory_actual_size` stays at the boot
    // size (Annex A finding 4) — this IS the measurement, not a flaky one.
    let (actual_mib, outcome) = backend
        .hotplug_add_memory(dir.path(), &vm, 512, Duration::from_millis(800))
        .expect("hotplug_add_memory");
    assert_eq!(
        actual_mib, 256,
        "memory_actual_size moved with no guest driver — re-check Annex A finding 4"
    );
    assert!(
        matches!(outcome, HotplugOutcome::Partial { .. }),
        "memory add with no guest driver must be partial: {outcome:?}"
    );

    // Same ceiling refusal, for memory: 1024 MiB > the declared 768 MiB max.
    let over = backend.hotplug_add_memory(dir.path(), &vm, 1024, Duration::from_millis(200));
    assert!(
        over.is_err(),
        "1024 MiB exceeds the declared ceiling of 768"
    );
}

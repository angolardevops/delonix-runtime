//! Live proof of `vm.guest-agent` (ADR-0050) against a REAL libvirt domain,
//! booted from a real cloud-init base disk, with `qemu-guest-agent` actually
//! running inside the guest — `#[ignore]`, run with `--ignored`, because this is
//! exactly the gap the matrix's own evidence-gate exists to close: a
//! `supported` claim nobody can re-run is the same as an unverified one.
//!
//! ```text
//! DELONIX_LIBVIRT_LIVE_DISK=/path/to/debian-genericcloud.qcow2 \
//!   cargo test -p delonix-provider-libvirt --test live -- --ignored --nocapture
//! ```
//!
//! Needs a reachable `qemu:///system` (the invoking user in the `libvirt`
//! group — `system_libvirt_usable()`), and `qemu-img`/`cloud-localds` on the
//! `PATH`; run with `--ignored`, a missing one FAILS the test naming it (a
//! silent return would read as `passed`). The base disk has to be a real
//! cloud-init image (NoCloud-capable);
//! this test asks cloud-init itself to install and enable
//! `qemu-guest-agent` on first boot (`packages:`/`runcmd:`) — the published
//! `delonix-vm-base` tags do not ship it yet, which is exactly the gap this
//! audit found (see `docs/discovery/67_VMAAS_READINESS_AUDIT.md` §3) — so
//! this test cannot assume it is already there.
//!
//! No SSH key, no SSH client: the only thing under test is the
//! `virtio-serial` channel and `LibvirtBackend::guest_info`, so the seed ISO
//! is built directly here (not through `delonix-vm::cloudinit`, which this
//! PROVIDER crate may not depend on — ADR-0044 P4b.4b) and `qemu-guest-agent`
//! arrives through cloud-init's own package module, not a login.
//!
//! Cleans up unconditionally: the domain is destroyed/undefined and the
//! working directory removed even if an assertion panics (`Drop` on
//! [`Cleanup`]), so a failed run never leaves a VM running on this host.

use delonix_compute::vm_backend::{CreateStage, VmBackend, VmConfig};
use delonix_compute::vm_registry::mac_for;
use delonix_compute::Vm;
use delonix_provider_libvirt::{libvirt_cleanup, system_libvirt_usable, LibvirtBackend};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// `--help` and not `--version`: `cloud-localds` has no `--version` and
/// exits 1 for an unrecognized option, which `--version` IS to it — this
/// would have reported it missing on every host that actually has it.
fn tool_missing(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--help")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
}

/// Guard that destroys the domain and wipes the working directory on drop,
/// unconditionally — a panicking assertion must not leave a VM running on
/// this (shared, real) host.
struct Cleanup {
    name: String,
    _dir: tempfile::TempDir,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = libvirt_cleanup(&self.name);
    }
}

fn write_seed(dir: &Path, name: &str) -> PathBuf {
    let mac = mac_for(name);
    let user_data = dir.join("user-data");
    let meta_data = dir.join("meta-data");
    let network_config = dir.join("network-config");
    // Deliberately no `users:`/`ssh_authorized_keys:` — this test proves the
    // guest-agent CHANNEL, not SSH reachability. `package_update: true` is
    // required: the published base images do not ship the agent (the audit's
    // own finding), so this run has to be able to install it for real.
    std::fs::write(
        &user_data,
        format!(
            "#cloud-config\nhostname: {name}\npackage_update: true\npackage_upgrade: false\npackages:\n  - qemu-guest-agent\nruncmd:\n  - [ systemctl, enable, --now, qemu-guest-agent ]\n"
        ),
    )
    .expect("write user-data");
    std::fs::write(
        &meta_data,
        format!("instance-id: {name}\nlocal-hostname: {name}\n"),
    )
    .expect("write meta-data");
    std::fs::write(
        &network_config,
        format!("version: 2\nethernets:\n  nic0:\n    match:\n      macaddress: \"{mac}\"\n    dhcp4: true\n"),
    )
    .expect("write network-config");
    let seed = dir.join(format!("{name}-seed.iso"));
    let status = Command::new("cloud-localds")
        .arg(format!("--network-config={}", network_config.display()))
        .arg(&seed)
        .arg(&user_data)
        .arg(&meta_data)
        .status()
        .expect("spawn cloud-localds");
    assert!(status.success(), "cloud-localds failed");
    seed
}

#[test]
#[ignore = "needs DELONIX_LIBVIRT_LIVE_DISK (a cloud-init qcow2), qemu-img, cloud-localds and a usable qemu:///system: cargo test -p delonix-provider-libvirt --test live -- --ignored"]
fn libvirt_guest_agent_answers_on_a_real_cloud_init_boot() {
    let disk = std::env::var("DELONIX_LIBVIRT_LIVE_DISK")
        .ok()
        .filter(|s| !s.is_empty())
        .expect("DELONIX_LIBVIRT_LIVE_DISK must name a real cloud-init base qcow2 to run this --ignored test");
    for tool in ["qemu-img", "cloud-localds"] {
        assert!(
            !tool_missing(tool),
            "{tool} must be on PATH to run this --ignored test"
        );
    }
    assert!(
        system_libvirt_usable(),
        "qemu:///system must be usable to run this --ignored test (join the `libvirt` group?)"
    );

    let name = format!("dlx-live-guestagent-{}", std::process::id());
    let dir = tempfile::tempdir().expect("tempdir");
    let dir_path = dir.path().to_path_buf();

    let overlay = dir_path.join(format!("{name}.qcow2"));
    let status = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-b"])
        .arg(&disk)
        .args(["-F", "qcow2"])
        .arg(&overlay)
        .status()
        .expect("spawn qemu-img create");
    assert!(status.success(), "qemu-img create -b failed");

    let seed = write_seed(&dir_path, &name);

    let cleanup = Cleanup {
        name: name.clone(),
        _dir: dir,
    };

    let backend = LibvirtBackend;
    let cfg = VmConfig {
        name: name.clone(),
        disk: disk.clone(),
        vcpus: 1,
        memory: "1G".into(),
        network: "ingress".into(),
        seed: Some(seed.to_string_lossy().into_owned()),
        cloud_init: Some(true),
        ..Default::default()
    };

    let stages = std::cell::RefCell::new(Vec::new());
    let boot = backend
        .boot(
            &dir_path,
            &cfg,
            &overlay.to_string_lossy(),
            &|s: CreateStage| {
                stages.borrow_mut().push(format!("{s:?}"));
            },
        )
        .expect("boot failed");
    eprintln!("boot stages: {:?}", stages.borrow());

    let mut vm = Vm::new(
        name.clone(),
        disk,
        overlay.to_string_lossy().into_owned(),
        cfg.vcpus,
        cfg.memory.clone(),
        cfg.network.clone(),
        boot.tap.clone(),
        boot.mac.clone(),
        boot.api_socket.clone(),
    );
    vm.ip = boot.ip.clone();
    vm.dhcp_lease_floor = boot.lease_floor.clone();
    vm.backend = "libvirt".into();

    assert!(backend.is_running(&vm), "domain did not start running");

    // The agent needs the guest to finish boot, DHCP, apt, install, and
    // enable the unit — generous on purpose; a real cloud-init + apt cycle
    // on a cold cache can take well over a minute.
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut info = None;
    while Instant::now() < deadline {
        match backend.guest_info(&vm) {
            Ok(Some(i)) => {
                info = Some(i);
                break;
            }
            Ok(None) => {}
            Err(e) => eprintln!("guest_info transient error: {e}"),
        }
        std::thread::sleep(Duration::from_secs(3));
    }

    let info = info.expect("guest-agent never answered within 300s");
    eprintln!("guest info: {info:?}");
    assert!(info.os.is_some(), "guest did not report an OS name");
    assert!(info.hostname.is_some(), "guest did not report a hostname");
    assert!(
        info.agent_version.is_some(),
        "guest did not report an agent version"
    );
    assert!(
        !info.filesystems.is_empty(),
        "guest did not report any filesystem"
    );

    drop(cleanup);
}

/// `vm clone` at the backend level: a clone boots as a DISTINCT, INDEPENDENT
/// domain off a flattened copy of the source's disk. The engine's `clone_vm`
/// is what the CLI drives (the live independence proof — a marker written to
/// the source that the clone never gains — is in the session report); this
/// test pins the two invariants a clone must hold whatever the caller: a
/// different identity (the MAC, which libvirt derives from the name), and a
/// disk that shares NO backing file with the source, so one cannot write
/// through to the other.
#[test]
#[ignore = "needs DELONIX_LIBVIRT_LIVE_DISK (a cloud-init qcow2), qemu-img, cloud-localds and a usable qemu:///system: cargo test -p delonix-provider-libvirt --test live -- --ignored"]
fn vm_clone_boots_an_independent_domain_from_a_copied_disk() {
    let disk = std::env::var("DELONIX_LIBVIRT_LIVE_DISK")
        .ok()
        .filter(|s| !s.is_empty())
        .expect("DELONIX_LIBVIRT_LIVE_DISK must name a real cloud-init base qcow2 to run this --ignored test");
    for tool in ["qemu-img", "cloud-localds"] {
        assert!(
            !tool_missing(tool),
            "{tool} must be on PATH to run this --ignored test"
        );
    }
    assert!(
        system_libvirt_usable(),
        "qemu:///system must be usable to run this --ignored test (join the `libvirt` group?)"
    );

    let src_name = format!("dlx-live-clone-src-{}", std::process::id());
    let clone_name = format!("dlx-live-clone-dst-{}", std::process::id());
    let dir = tempfile::tempdir().expect("tempdir");
    let dir_path = dir.path().to_path_buf();

    // The source: an overlay over the base, booted as a normal VM.
    let src_overlay = dir_path.join(format!("{src_name}.qcow2"));
    let status = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-b"])
        .arg(&disk)
        .args(["-F", "qcow2"])
        .arg(&src_overlay)
        .status()
        .expect("spawn qemu-img create");
    assert!(status.success(), "qemu-img create -b failed");
    let src_seed = write_seed(&dir_path, &src_name);

    let _src_cleanup = Cleanup {
        name: src_name.clone(),
        _dir: tempfile::tempdir().expect("tempdir"),
    };
    let _clone_cleanup = Cleanup {
        name: clone_name.clone(),
        _dir: tempfile::tempdir().expect("tempdir"),
    };

    let backend = LibvirtBackend;
    let src_cfg = VmConfig {
        name: src_name.clone(),
        disk: disk.clone(),
        vcpus: 1,
        memory: "1G".into(),
        network: "ingress".into(),
        seed: Some(src_seed.to_string_lossy().into_owned()),
        cloud_init: Some(true),
        ..Default::default()
    };
    let noop = |_s: CreateStage| {};
    let src_boot = backend
        .boot(&dir_path, &src_cfg, &src_overlay.to_string_lossy(), &noop)
        .expect("source boot failed");

    // The clone's disk: `qemu-img convert` flattens the whole chain into a
    // STANDALONE qcow2 — the same primitive `LocalDiskImages::clone_disk` runs.
    let clone_disk = dir_path.join(format!("{clone_name}.qcow2"));
    let status = Command::new("qemu-img")
        .args(["convert", "-U", "-O", "qcow2"])
        .arg(&src_overlay)
        .arg(&clone_disk)
        .status()
        .expect("spawn qemu-img convert");
    assert!(status.success(), "qemu-img convert failed");

    // Independence of the disk: the copy must carry no backing file at all.
    let info = Command::new("qemu-img")
        .args(["info", "--output=json"])
        .arg(&clone_disk)
        .output()
        .expect("spawn qemu-img info");
    let info = String::from_utf8_lossy(&info.stdout);
    assert!(
        !info.contains("backing-filename"),
        "clone disk still has a backing file — not independent:\n{info}"
    );

    // The clone boots as its own domain. A fresh seed gives it its own
    // cloud-init identity; the different NAME is what makes its MAC differ.
    let clone_seed = write_seed(&dir_path, &clone_name);
    let clone_cfg = VmConfig {
        name: clone_name.clone(),
        disk: clone_disk.to_string_lossy().into_owned(),
        vcpus: 1,
        memory: "1G".into(),
        network: "ingress".into(),
        seed: Some(clone_seed.to_string_lossy().into_owned()),
        cloud_init: Some(true),
        ..Default::default()
    };
    let clone_boot = backend
        .boot(&dir_path, &clone_cfg, &clone_disk.to_string_lossy(), &noop)
        .expect("clone boot failed");

    assert_ne!(
        src_boot.mac, clone_boot.mac,
        "clone shares the source's MAC — not a distinct identity"
    );
    assert_eq!(
        clone_boot.mac,
        mac_for(&clone_name),
        "clone MAC is not the one derived from its name"
    );

    let mut src_vm = Vm::new(
        src_name.clone(),
        disk.clone(),
        src_overlay.to_string_lossy().into_owned(),
        1,
        "1G".into(),
        "ingress".into(),
        src_boot.tap.clone(),
        src_boot.mac.clone(),
        src_boot.api_socket.clone(),
    );
    src_vm.backend = "libvirt".into();
    let mut clone_vm = Vm::new(
        clone_name.clone(),
        clone_disk.to_string_lossy().into_owned(),
        clone_disk.to_string_lossy().into_owned(),
        1,
        "1G".into(),
        "ingress".into(),
        clone_boot.tap.clone(),
        clone_boot.mac.clone(),
        clone_boot.api_socket.clone(),
    );
    clone_vm.backend = "libvirt".into();

    assert!(backend.is_running(&src_vm), "source did not stay running");
    assert!(backend.is_running(&clone_vm), "clone did not start running");
}

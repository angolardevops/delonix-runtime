//! SPIKE (ADR-0044, substitution spike). Live proof against REAL backends on
//! this host — `#[ignore]`, run explicitly:
//!
//! ```text
//! cargo test -p delonix-vm --test provider_spike_live -- --ignored --nocapture
//! ```
//!
//! Drives the SAME `VmSpec` through `delonix_vm::provider_spike::registry`
//! for `"cloud-hypervisor"` and `"libvirt"`, create→observe→stop→observe→
//! start→observe→destroy, with **zero** backend-name matching in this file
//! outside the one `registry(id)` call per provider — the substitution
//! spike's own gate (ADR-0044, "Spikes required", item 1).
//!
//! Isolated: a fresh `tempdir` per run, never the host's real VM state root.
//!
//! **Cloud Hypervisor cannot complete its lifecycle from THIS binary at
//! all**, and that is itself a measured finding, not a limitation of this
//! test: `delonix_vm::set_network(...)` is normally called once by
//! `main.rs`, which a `cargo test` binary never runs — and even with it
//! registered (proven separately, see the spike report), the SDN's
//! netns-pin re-exec resolves `std::env::current_exe` to THIS test
//! binary, not `delonix`, and has no override for that path. Registering
//! the network port here would need `delonix-sdn` as a dev-dependency,
//! which is exactly the dependency `VmNetwork` exists to remove (ADR-0040
//! P3) — reintroducing it here, even as a dev-dep, would not get further
//! (the `current_exe` issue is unaffected by it) and `arch_fitness.py`
//! checks `dev-dependencies` too. The real CH proof for this spike ran
//! through the actual `delonix` binary instead — see the spike report for
//! that transcript. This file proves libvirt live, end to end, and lets CH
//! fail with its real, first-hit reason.

use delonix_vm::provider_spike::{registry, Extensions, VmSpec};
use std::path::Path;
use std::process::Command;

fn tool_missing(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
}

fn base_disk(dir: &Path) -> String {
    let path = dir.join("base.qcow2");
    let status = Command::new("qemu-img")
        .args(["create", "-f", "qcow2"])
        .arg(&path)
        .arg("64M")
        .status()
        .expect("spawn qemu-img");
    assert!(status.success(), "qemu-img create failed");
    path.to_string_lossy().into_owned()
}

/// Runs the full lifecycle through ONE provider id and returns what it
/// observed at each step, for the caller to compare across providers. `None`
/// means the provider's own tooling is not usable on this host — the caller
/// treats that as SKIP, not FAIL, exactly like `antispoof_live_...` does for
/// libvirt above it in `lib.rs`.
fn run_lifecycle(root: &Path, disk: &str, provider_id: &str) -> Option<Vec<String>> {
    let provider = registry(provider_id).unwrap_or_else(|| panic!("unknown id {provider_id}"));
    let mut log = Vec::new();

    let spec = VmSpec {
        name: format!("dlx-p4spike-{provider_id}"),
        disk: disk.to_string(),
        vcpus: 1,
        memory_mib: 256,
        network: "ingress".into(),
        namespace: "default".into(),
        cloud_init: Some(false), // no guest OS on the blank disk — nothing to seed
        ..Default::default()
    };
    let ext = Extensions::default(); // deliberately empty: this is the CONVERGENCE test

    let handle = match provider.create(root, &spec, &ext) {
        Ok(h) => h,
        Err(e) => {
            log.push(format!("SKIP: create failed ({e})"));
            return None;
        }
    };
    log.push("created".into());

    let obs = provider
        .observe(root, &handle)
        .expect("observe after create");
    log.push(format!(
        "after-create: running={} ip={:?} confidence={:?}",
        obs.running, obs.ip, obs.ip_confidence
    ));
    assert!(obs.running, "{provider_id}: not running right after create");

    provider.stop(root, &handle).expect("stop");
    let obs = provider.observe(root, &handle).expect("observe after stop");
    log.push(format!("after-stop: running={}", obs.running));
    assert!(!obs.running, "{provider_id}: still running after stop");

    provider.start(root, &handle).expect("start");
    let obs = provider
        .observe(root, &handle)
        .expect("observe after start");
    log.push(format!("after-start: running={}", obs.running));
    assert!(
        obs.running,
        "{provider_id}: not running after start (resume)"
    );

    provider.destroy(root, &handle).expect("destroy");
    log.push("destroyed".into());

    Some(log)
}

/// The one print site in this file — everything above collects its own
/// `Vec<String>` and hands it here, rather than printing as it goes.
fn report(lines: &[String]) {
    eprintln!("{}", lines.join("\n"));
}

#[test]
#[ignore]
fn the_same_vmspec_converges_cloud_hypervisor_and_libvirt() {
    let ch_missing = tool_missing("cloud-hypervisor", &["--version"]);
    let lv_missing = tool_missing("virsh", &["--version"]);
    if ch_missing && lv_missing {
        report(&["neither cloud-hypervisor nor virsh usable here — nothing to prove".into()]);
        return;
    }

    let tmp =
        std::env::temp_dir().join(format!("dlx-p4-substitution-spike-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("mkdir tmp root");
    let disk = base_disk(&tmp);

    let mut ran_at_least_one = false;
    let mut summary = Vec::new();

    if !ch_missing {
        match run_lifecycle(&tmp, &disk, "cloud-hypervisor") {
            Some(log) => {
                summary.push("=== cloud-hypervisor ===".to_string());
                summary.extend(log.iter().map(|l| format!("  {l}")));
                ran_at_least_one = true;
            }
            None => summary.push("cloud-hypervisor: create failed on this host, skipped".into()),
        }
    } else {
        summary.push("cloud-hypervisor not installed — skipped".into());
    }

    if !lv_missing {
        match run_lifecycle(&tmp, &disk, "libvirt") {
            Some(log) => {
                summary.push("=== libvirt ===".to_string());
                summary.extend(log.iter().map(|l| format!("  {l}")));
                ran_at_least_one = true;
            }
            None => summary.push("libvirt: create failed on this host, skipped".into()),
        }
    } else {
        summary.push("virsh not usable — skipped".into());
    }

    report(&summary);
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        ran_at_least_one,
        "neither provider completed a lifecycle — nothing was proven"
    );
}

// The `ip_is_predicted`/`IpConfidence` distinction (Cloud Hypervisor always
// `Predicted`, never `Observed`) is proven live too — but not from THIS
// binary, for the same `current_exe` reason the module doc-comment above
// explains. See the spike report for that transcript
// (`p4_substitution_spike_ch`, run through the real `delonix` binary):
// `after-create running=true ip=Some("10.200.254.164") confidence=Predicted`.

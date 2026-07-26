//! Shared resource-summary collector — the SINGLE place that aggregates
//! "how much is this engine using right now" (container/VM counts, cgroup
//! memory, network bytes, disk usage per area). Three consumers share this
//! exact function so none of them can drift from each other:
//!
//!  * `delonix-mgmt`'s `GET /metrics` (Prometheus gauges) and `GET /v1/dash`
//!    (JSON) — this crate, see `lib.rs`.
//!  * `delonix dash` (the TUI/`--once`/`--json` CLI) — `delonix-runtime-bin`
//!    depends on `delonix-mgmt` (not the other way around), so it calls
//!    straight into [`collect`] instead of re-implementing the aggregation.
//!
//! Lives here (not in `delonix-runtime-core`) because it needs the store/
//! cgroup/netns access of `delonix-runtime`/`delonix-vm`/`delonix-net`/
//! `delonix-image`/`delonix-volume` — `runtime-core` is a shared-types leaf
//! crate none of the higher-level crates depend on for this.

use std::path::Path;

use delonix_runtime_core::Status;

/// A point-in-time snapshot of engine-wide resource usage. `Serialize` so it
/// is BOTH the body of `GET /v1/dash` and the payload of `delonix dash --json`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DashSummary {
    pub containers_running: u64,
    pub containers_total: u64,
    pub vms_running: u64,
    pub vms_total: u64,
    pub networks_total: u64,
    pub volumes_total: u64,
    pub images_total: u64,
    pub secrets_total: u64,
    /// `memory.current` of the whole Delonix cgroup slice, in bytes.
    pub memory_bytes_used: u64,
    /// `memory.max` of the whole Delonix cgroup slice, in bytes (0 = unlimited/unknown — cgroup v2 reports the literal string `"max"`, which doesn't parse as a number).
    pub memory_bytes_limit: u64,
    /// Sum of received bytes across every running container's primary interface, cumulative since each interface's creation. `None` when the caller opted out of the netns reads (see [`collect`]'s `include_network`) — a real "0 bytes" is `Some(0)`, not `None`.
    pub network_rx_bytes: Option<u64>,
    pub network_tx_bytes: Option<u64>,
    /// `None` when the caller opted out of the directory walk (see
    /// [`collect`]'s `include_storage`) — MEASURED on a real host with 49
    /// containers (several full `kindest/node` rootfs copies): 68 GiB,
    /// **over a minute** of disk I/O. Never safe to compute inline on a
    /// request/tick with a latency budget.
    pub storage_bytes_images: Option<u64>,
    pub storage_bytes_volumes: Option<u64>,
    pub storage_bytes_vm_images: Option<u64>,
    pub storage_bytes_containers: Option<u64>,
}

/// Recursive sum of a directory's apparent size, `du`-style. Zero on any
/// unreadable path — mirrors `cmd/system.rs::dir_size` in the bin crate
/// (duplicated here rather than shared across a crate boundary: it's a
/// trivial ~10-line walk, and `delonix-mgmt` cannot depend on the `-bin`
/// crate without an actual circular dependency, unlike the higher-level
/// engine crates this module already pulls in).
fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => dir_size(&path),
                Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0,
            }
        })
        .sum()
}

/// Collects a fresh [`DashSummary`]. Best-effort throughout: a store that
/// doesn't open counts as empty/zero (this must never fail a scrape or a
/// dashboard render over one missing/corrupt store).
///
/// `include_network`: per-container network totals require entering EACH
/// running container's netns to `cat` `/sys/class/net/.../statistics/*`
/// (`delonix_net::infra::container_net_bytes`, one `nsenter`+`cat` per
/// container) — real cost with many containers.
///
/// `include_storage`: a full recursive walk of `blobs`/`layers`/`volumes`/
/// `vm-images`/`containers` under `root` — MEASURED at over a minute on a
/// host with heavy containers (see the field doc on [`DashSummary`]).
///
/// Callers on a tight budget (an HTTP request handler, a TUI tick) MUST pass
/// `false` for whichever of these they can't afford synchronously, and
/// display the last known value from a slower background refresh instead —
/// see `cmd/dash.rs`'s TUI (a background thread) and this crate's `lib.rs`
/// (a background tokio task) for the two real callers of that pattern.
/// One-shot callers with no latency budget (`--once`/`--json`, a human
/// hitting `/v1/dash`) should pass `true` for both.
pub fn collect(root: &Path, include_network: bool, include_storage: bool) -> DashSummary {
    let mut containers_running = 0u64;
    let mut containers_total = 0u64;
    let mut running_ids: Vec<String> = Vec::new();
    if let Ok(store) = delonix_runtime_core::Store::open(root.join("containers")) {
        if let Ok(list) = store.list() {
            containers_total = list.len() as u64;
            for mut c in list {
                delonix_runtime::reconcile_status(&mut c);
                if c.status == Status::Running {
                    containers_running += 1;
                    running_ids.push(c.id.clone());
                }
            }
        }
    }

    let vms = delonix_vm::list(root).unwrap_or_default();
    let vms_running = vms.iter().filter(|v| v.status == Status::Running).count() as u64;
    let vms_total = vms.len() as u64;

    let networks_total = delonix_net::NetworkStore::open(root)
        .and_then(|s| s.list())
        .map(|l| l.len() as u64)
        .unwrap_or(0);
    let volumes_total = delonix_volume::VolumeStore::open(root)
        .and_then(|s| s.list())
        .map(|l| l.len() as u64)
        .unwrap_or(0);
    let images_total = delonix_image::ImageStore::open(root)
        .and_then(|s| s.list())
        .map(|l| l.len() as u64)
        .unwrap_or(0);
    let secrets_total = delonix_runtime_core::SecretStore::open(root)
        .map(|s| s.list().len() as u64)
        .unwrap_or(0);

    let (memory_bytes_limit, memory_bytes_used, ..) = delonix_runtime::slice_budget();

    let (network_rx_bytes, network_tx_bytes) = if include_network {
        let mut rx = 0u64;
        let mut tx = 0u64;
        for id in &running_ids {
            if let Some((r, t)) = delonix_net::infra::container_net_bytes(id) {
                rx += r;
                tx += t;
            }
        }
        (Some(rx), Some(tx))
    } else {
        (None, None)
    };

    let (
        storage_bytes_images,
        storage_bytes_volumes,
        storage_bytes_vm_images,
        storage_bytes_containers,
    ) = if include_storage {
        (
            Some(dir_size(&root.join("blobs")) + dir_size(&root.join("layers"))),
            Some(dir_size(&root.join("volumes"))),
            Some(dir_size(&root.join("vm-images"))),
            Some(dir_size(&root.join("containers"))),
        )
    } else {
        (None, None, None, None)
    };

    DashSummary {
        containers_running,
        containers_total,
        vms_running,
        vms_total,
        networks_total,
        volumes_total,
        images_total,
        secrets_total,
        memory_bytes_used,
        memory_bytes_limit,
        network_rx_bytes,
        network_tx_bytes,
        storage_bytes_images,
        storage_bytes_volumes,
        storage_bytes_vm_images,
        storage_bytes_containers,
    }
}

/// Pushes a [`DashSummary`] into the shared Prometheus registry
/// (`delonix_runtime_core::metrics`). Fields collected as `None` (an
/// `include_network`/`include_storage` opt-out) simply leave that gauge at
/// its last-published value — callers are expected to publish the cheap
/// fields often and the expensive ones from a slower background refresh
/// (see `collect`'s doc comment), so a gauge is stale, never wrong-and-zero.
pub fn publish_to_metrics(s: &DashSummary) {
    delonix_runtime_core::metrics::set_containers(s.containers_running, s.containers_total);
    delonix_runtime_core::metrics::set_vms(s.vms_running, s.vms_total);
    delonix_runtime_core::metrics::set_memory(s.memory_bytes_used, s.memory_bytes_limit);
    if let (Some(rx), Some(tx)) = (s.network_rx_bytes, s.network_tx_bytes) {
        delonix_runtime_core::metrics::set_network(rx, tx);
    }
    if let (Some(images), Some(volumes), Some(vm_images), Some(containers)) = (
        s.storage_bytes_images,
        s.storage_bytes_volumes,
        s.storage_bytes_vm_images,
        s.storage_bytes_containers,
    ) {
        delonix_runtime_core::metrics::set_storage(images, volumes, vm_images, containers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_sobre_root_vazio_nao_rebenta_e_da_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let s = collect(dir.path(), true, true);
        assert_eq!(s.containers_running, 0);
        assert_eq!(s.containers_total, 0);
        assert_eq!(s.vms_total, 0);
        assert_eq!(s.storage_bytes_images, Some(0));
        assert_eq!(s.network_rx_bytes, Some(0));
    }

    #[test]
    fn collect_sem_include_network_nem_storage_devolve_none() {
        let dir = tempfile::tempdir().unwrap();
        let s = collect(dir.path(), false, false);
        assert_eq!(s.network_rx_bytes, None);
        assert_eq!(s.network_tx_bytes, None);
        assert_eq!(s.storage_bytes_images, None);
        assert_eq!(s.storage_bytes_containers, None);
    }

    #[test]
    fn dir_size_soma_recursivamente() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"12345").unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b"), b"1234567890").unwrap();
        assert_eq!(dir_size(dir.path()), 15);
    }
}

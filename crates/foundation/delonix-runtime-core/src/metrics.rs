//! SHARED Prometheus metrics of the runtime (C1, slice 2). A single definition
//! per metric lives here; the servers expose them where they are needed — the
//! `delonix-cri` on a dedicated HTTP `/metrics` (runtime scrape on the k8s node, like
//! containerd/CRI-O), and optionally the `delonix-mgmt` (control-plane). Avoids
//! duplicating metric definitions across surfaces.
//!
//! Consumers do NOT touch `prometheus-client`: they increment via the
//! `inc_*` functions and render via [`encode`]. This keeps the dependency contained in this crate.

use std::sync::LazyLock;

use prometheus_client::encoding::text::encode as encode_registry;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;

struct Metrics {
    registry: Registry,
    pod_sandboxes_created: Counter,
    containers_created: Counter,
    images_pulled: Counter,
    // Resource-summary GAUGES (C1, slice 3) — unlike the counters above (app
    // events, incremented as they happen), these are a snapshot RE-COMPUTED by
    // the caller right before a scrape (see `dash`/`dashstats` in the
    // higher-level crates that have store/cgroup/netns access this crate
    // deliberately does not). `Gauge`, not `Counter`, even for the
    // cumulative-looking byte counters: the value read from the kernel is
    // already a running total, but it is SUMMED across a dynamic set of
    // containers that can disappear between scrapes (shrinking the sum) —
    // `Counter`'s API only allows monotonic `inc`/`inc_by`, which doesn't fit
    // a value that can legitimately go down when a container exits.
    containers_running: Gauge,
    containers_total: Gauge,
    vms_running: Gauge,
    vms_total: Gauge,
    memory_bytes_used: Gauge,
    memory_bytes_limit: Gauge,
    network_rx_bytes: Gauge,
    network_tx_bytes: Gauge,
    /// How many running containers did NOT contribute to the two gauges
    /// above (`--net host`/`--net none` have no netns to read) — makes the
    /// traffic total's incompleteness visible to a scrape instead of it
    /// silently looking like an exhaustive, authoritative sum.
    network_unmeasured_containers: Gauge,
    storage_bytes_images: Gauge,
    storage_bytes_volumes: Gauge,
    storage_bytes_vm_images: Gauge,
    storage_bytes_containers: Gauge,
}

static METRICS: LazyLock<Metrics> = LazyLock::new(|| {
    let mut registry = Registry::with_prefix("delonix");

    // `delonix_build_info{version="…"} 1` — always present, gives the scrape a stable
    // series to correlate the runtime version (`*_build_info` pattern).
    let build = Info::new(vec![("version", env!("CARGO_PKG_VERSION"))]);
    registry.register("build", "Runtime build information", build);

    let pod_sandboxes_created = Counter::default();
    registry.register(
        "cri_pod_sandboxes_created",
        "CRI pod sandboxes created (total)",
        pod_sandboxes_created.clone(),
    );
    let containers_created = Counter::default();
    registry.register(
        "cri_containers_created",
        "CRI containers created (total)",
        containers_created.clone(),
    );
    let images_pulled = Counter::default();
    registry.register(
        "cri_images_pulled",
        "Images pulled via CRI (total)",
        images_pulled.clone(),
    );

    let containers_running = Gauge::default();
    registry.register(
        "containers_running",
        "Containers currently in the Running state",
        containers_running.clone(),
    );
    let containers_total = Gauge::default();
    registry.register(
        "containers_total",
        "Containers in any state (the whole store)",
        containers_total.clone(),
    );
    let vms_running = Gauge::default();
    registry.register(
        "vms_running",
        "VMs currently in the Running state",
        vms_running.clone(),
    );
    let vms_total = Gauge::default();
    registry.register("vms_total", "VMs in any state", vms_total.clone());
    let memory_bytes_used = Gauge::default();
    registry.register(
        "memory_bytes_used",
        "Current memory.current of the whole Delonix cgroup slice, in bytes",
        memory_bytes_used.clone(),
    );
    let memory_bytes_limit = Gauge::default();
    registry.register(
        "memory_bytes_limit",
        "Configured memory.max of the whole Delonix cgroup slice, in bytes (0 = unlimited/unknown)",
        memory_bytes_limit.clone(),
    );
    let network_rx_bytes = Gauge::default();
    registry.register(
        "network_rx_bytes",
        "Sum of received bytes across every running container's primary interface",
        network_rx_bytes.clone(),
    );
    let network_tx_bytes = Gauge::default();
    registry.register(
        "network_tx_bytes",
        "Sum of transmitted bytes across every running container's primary interface",
        network_tx_bytes.clone(),
    );
    let network_unmeasured_containers = Gauge::default();
    registry.register(
        "network_unmeasured_containers",
        "Running containers NOT included in network_rx_bytes/network_tx_bytes (--net host/none have no netns to read)",
        network_unmeasured_containers.clone(),
    );
    let storage_bytes_images = Gauge::default();
    registry.register(
        "storage_bytes_images",
        "Disk space used by the image store (blobs + layers), in bytes",
        storage_bytes_images.clone(),
    );
    let storage_bytes_volumes = Gauge::default();
    registry.register(
        "storage_bytes_volumes",
        "Disk space used by named volumes, in bytes",
        storage_bytes_volumes.clone(),
    );
    let storage_bytes_vm_images = Gauge::default();
    registry.register(
        "storage_bytes_vm_images",
        "Disk space used by golden/VM images (qcow2 bases), in bytes",
        storage_bytes_vm_images.clone(),
    );
    let storage_bytes_containers = Gauge::default();
    registry.register(
        "storage_bytes_containers",
        "Disk space used by container rootfs directories, in bytes",
        storage_bytes_containers.clone(),
    );

    Metrics {
        registry,
        pod_sandboxes_created,
        containers_created,
        images_pulled,
        containers_running,
        containers_total,
        vms_running,
        vms_total,
        memory_bytes_used,
        memory_bytes_limit,
        network_rx_bytes,
        network_tx_bytes,
        network_unmeasured_containers,
        storage_bytes_images,
        storage_bytes_volumes,
        storage_bytes_vm_images,
        storage_bytes_containers,
    }
});

/// Renders all metrics in Prometheus text format (the body of a
/// `GET /metrics`). Never fails in practice — `encode` only errors on `fmt::Error`.
pub fn encode() -> String {
    let mut buf = String::new();
    let _ = encode_registry(&mut buf, &METRICS.registry);
    buf
}

/// +1 CRI pod sandbox created.
pub fn inc_pod_sandbox_created() {
    METRICS.pod_sandboxes_created.inc();
}
/// +1 CRI container created.
pub fn inc_container_created() {
    METRICS.containers_created.inc();
}
/// +1 image pulled via CRI.
pub fn inc_image_pulled() {
    METRICS.images_pulled.inc();
}

/// Overwrites the container-count gauges with a freshly-collected snapshot.
pub fn set_containers(running: u64, total: u64) {
    METRICS.containers_running.set(running as i64);
    METRICS.containers_total.set(total as i64);
}
/// Overwrites the VM-count gauges with a freshly-collected snapshot.
pub fn set_vms(running: u64, total: u64) {
    METRICS.vms_running.set(running as i64);
    METRICS.vms_total.set(total as i64);
}
/// Overwrites the cgroup-slice memory gauges (bytes) with a freshly-collected snapshot.
pub fn set_memory(used: u64, limit: u64) {
    METRICS.memory_bytes_used.set(used as i64);
    METRICS.memory_bytes_limit.set(limit as i64);
}
/// Overwrites the aggregate network byte-counter gauges (see the `Metrics`
/// struct doc comment for why these are gauges, not counters).
pub fn set_network(rx_bytes: u64, tx_bytes: u64, unmeasured_containers: u64) {
    METRICS.network_rx_bytes.set(rx_bytes as i64);
    METRICS.network_tx_bytes.set(tx_bytes as i64);
    METRICS
        .network_unmeasured_containers
        .set(unmeasured_containers as i64);
}
/// Overwrites the disk-usage gauges (bytes) with a freshly-collected snapshot.
pub fn set_storage(images: u64, volumes: u64, vm_images: u64, containers: u64) {
    METRICS.storage_bytes_images.set(images as i64);
    METRICS.storage_bytes_volumes.set(volumes as i64);
    METRICS.storage_bytes_vm_images.set(vm_images as i64);
    METRICS.storage_bytes_containers.set(containers as i64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_rende_formato_prometheus_e_conta() {
        inc_container_created();
        let out = encode();
        // build_info always present + the container metric (TYPE without `_total`, the
        // sample WITH `_total`, as OpenMetrics requires).
        assert!(
            out.contains("delonix_build_info{version="),
            "build_info em falta:\n{out}"
        );
        assert!(out.contains("# TYPE delonix_cri_containers_created counter"));
        assert!(
            out.contains("delonix_cri_containers_created_total"),
            "contador em falta:\n{out}"
        );
        // Prometheus format ends with `# EOF`.
        assert!(out.trim_end().ends_with("# EOF"));
    }
}

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
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;

struct Metrics {
    registry: Registry,
    pod_sandboxes_created: Counter,
    containers_created: Counter,
    images_pulled: Counter,
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

    Metrics {
        registry,
        pod_sandboxes_created,
        containers_created,
        images_pulled,
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

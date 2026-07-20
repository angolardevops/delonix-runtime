//! Métricas Prometheus PARTILHADAS do runtime (C1, fatia 2). Uma única definição
//! por métrica vive aqui; os servidores expõem-nas onde fazem falta — o
//! `delonix-cri` num `/metrics` HTTP dedicado (scrape do runtime no nó k8s, como
//! containerd/CRI-O), e opcionalmente o `delonix-mgmt` (control-plane). Evita
//! duplicar definições de métrica entre superfícies.
//!
//! Os consumidores NÃO tocam no `prometheus-client`: incrementam via as funções
//! `inc_*` e renderizam via [`encode`]. Assim a dependência fica contida neste crate.

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

    // `delonix_build_info{version="…"} 1` — sempre presente, dá ao scrape uma série
    // estável para correlacionar versão do runtime (padrão `*_build_info`).
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

/// Renderiza todas as métricas no formato-texto do Prometheus (o corpo de um
/// `GET /metrics`). Nunca falha na prática — o `encode` só erra em `fmt::Error`.
pub fn encode() -> String {
    let mut buf = String::new();
    let _ = encode_registry(&mut buf, &METRICS.registry);
    buf
}

/// +1 pod sandbox CRI criado.
pub fn inc_pod_sandbox_created() {
    METRICS.pod_sandboxes_created.inc();
}
/// +1 container CRI criado.
pub fn inc_container_created() {
    METRICS.containers_created.inc();
}
/// +1 imagem puxada via CRI.
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
        // build_info sempre presente + a métrica de container (TYPE sem `_total`, a
        // amostra COM `_total`, como o OpenMetrics manda).
        assert!(
            out.contains("delonix_build_info{version="),
            "build_info em falta:\n{out}"
        );
        assert!(out.contains("# TYPE delonix_cri_containers_created counter"));
        assert!(
            out.contains("delonix_cri_containers_created_total"),
            "contador em falta:\n{out}"
        );
        // Formato Prometheus termina com `# EOF`.
        assert!(out.trim_end().ends_with("# EOF"));
    }
}

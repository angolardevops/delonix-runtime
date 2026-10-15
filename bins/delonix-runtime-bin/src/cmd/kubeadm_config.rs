//! Renders the `kubeadm init --config=...` YAML for the cases the flat flags
//! cannot express.
//!
//! `kubeadm init`'s flat CLI flags (`--pod-network-cidr`, `--service-cidr`, ...)
//! cannot express three things this engine needs:
//!
//! 1. `ClusterConfiguration.etcd.external` — the original reason this module
//!    exists (`etcd.mode: external`).
//! 2. `InitConfiguration.skipPhases: [addon/kube-proxy]` — needed when the
//!    chosen pod network REPLACES kube-proxy (`spec.network.kubeProxy: none`).
//!    Installing kube-proxy and then letting a CNI replace it leaves two
//!    things programming the same service dataplane, which is why this is a
//!    creation-time decision and not a later toggle.
//! 3. `KubeProxyConfiguration.mode: ipvs` — needed for
//!    `spec.network.kubeProxy: ipvs`.
//!
//! The `stacked` + `kubeProxy: iptables` path (the default) keeps using the
//! flat flags unchanged, so the default behaviour carries zero regression
//! risk. Hand-built via typed structs + `serde_yaml` (already a dependency,
//! used elsewhere for manifest round-tripping) — no `kubeadm-types` crate
//! exists or is added for this.

use delonix_model::{Error, Result};
use serde::Serialize;

const KUBEADM_API_VERSION: &str = "kubeadm.k8s.io/v1beta3";
/// The kube-proxy component config group — its own API, not kubeadm's.
const KUBE_PROXY_API_VERSION: &str = "kubeproxy.config.k8s.io/v1alpha1";

/// kubeadm's own conventional PKI paths for the STACKED etcd case — reused
/// deliberately for the EXTERNAL case too, so anything downstream that
/// expects kubeadm's normal layout still finds these files there.
pub(crate) const KUBEADM_ETCD_CA_PATH: &str = "/etc/kubernetes/pki/etcd/ca.crt";
pub(crate) const KUBEADM_ETCD_CLIENT_CERT_PATH: &str =
    "/etc/kubernetes/pki/apiserver-etcd-client.crt";
pub(crate) const KUBEADM_ETCD_CLIENT_KEY_PATH: &str =
    "/etc/kubernetes/pki/apiserver-etcd-client.key";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitConfigurationYaml {
    api_version: &'static str,
    kind: &'static str,
    node_registration: NodeRegistration,
    /// Omitted entirely when empty — an empty `skipPhases: []` is accepted by
    /// kubeadm but says something this engine does not mean.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skip_phases: Vec<String>,
}

/// `kubeproxy.config.k8s.io/v1alpha1` — only rendered for `mode: ipvs`.
/// `iptables` is kubeadm's own default and is left unsaid rather than restated,
/// so the generated document never claims to configure what it did not change.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KubeProxyConfigurationYaml {
    api_version: &'static str,
    kind: &'static str,
    mode: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeRegistration {
    cri_socket: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClusterConfigurationYaml {
    api_version: &'static str,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kubernetes_version: Option<String>,
    control_plane_endpoint: String,
    networking: Networking,
    #[serde(skip_serializing_if = "Option::is_none")]
    etcd: Option<EtcdBlock>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Networking {
    pod_subnet: String,
    service_subnet: String,
}

#[derive(Serialize)]
struct EtcdBlock {
    external: ExternalEtcd,
}

/// Everything the generated config needs to know. A struct and not eight
/// positional arguments: the two network fields were added after the etcd
/// ones, and a call site that silently passed `service_subnet` where
/// `pod_subnet` was expected would compile.
pub(crate) struct InitConfig<'a> {
    pub k8s_version: Option<&'a str>,
    pub control_plane_endpoint: &'a str,
    pub pod_subnet: &'a str,
    pub service_subnet: &'a str,
    pub cri_socket: &'a str,
    /// Empty = stacked etcd (no `etcd.external` block at all).
    pub etcd_endpoints: &'a [String],
    /// `iptables` (nothing rendered), `ipvs` (a `KubeProxyConfiguration`), or
    /// `none` (the `addon/kube-proxy` phase is skipped).
    pub kube_proxy: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExternalEtcd {
    endpoints: Vec<String>,
    ca_file: String,
    cert_file: String,
    key_file: String,
}

/// Does this configuration need a generated `--config` at all?
///
/// Asked here, next to what the config can express, rather than at the call
/// site: the flat-flag path has to stay the default, and the condition for
/// leaving it must live with the reason.
pub(crate) fn needed(cfg: &InitConfig) -> bool {
    !cfg.etcd_endpoints.is_empty() || cfg.kube_proxy != "iptables"
}

/// Renders the `kubeadm init --config=` YAML: an `InitConfiguration`, a
/// `ClusterConfiguration`, and — only for `kubeProxy: ipvs` — a
/// `KubeProxyConfiguration`.
///
/// Refuses to render a config that would say nothing new (the caller should
/// have used the flat flags), because a `--config` path taken by accident is a
/// second way to configure a cluster and the two drift.
pub(crate) fn render_init_config(cfg: &InitConfig) -> Result<String> {
    if !needed(cfg) {
        return Err(Error::Invalid(
            "render_init_config: nothing here needs a --config (stacked etcd and \
             kubeProxy: iptables are the flat-flag path) — the caller has to check `needed()`"
                .into(),
        ));
    }
    let init = InitConfigurationYaml {
        api_version: KUBEADM_API_VERSION,
        kind: "InitConfiguration",
        node_registration: NodeRegistration {
            cri_socket: cfg.cri_socket.to_string(),
        },
        // The pod network replaces kube-proxy: do not install it. Installing
        // it anyway and letting the CNI take over leaves two owners of the
        // service dataplane, and which one wins depends on which wrote last.
        skip_phases: if cfg.kube_proxy == "none" {
            vec!["addon/kube-proxy".to_string()]
        } else {
            Vec::new()
        },
    };
    let cluster = ClusterConfigurationYaml {
        api_version: KUBEADM_API_VERSION,
        kind: "ClusterConfiguration",
        kubernetes_version: cfg.k8s_version.map(|v| format!("v{v}")),
        control_plane_endpoint: cfg.control_plane_endpoint.to_string(),
        networking: Networking {
            pod_subnet: cfg.pod_subnet.to_string(),
            service_subnet: cfg.service_subnet.to_string(),
        },
        // Omitted for stacked etcd, which is kubeadm's own default: an
        // `etcd: {}` block would be a claim about something not configured.
        etcd: (!cfg.etcd_endpoints.is_empty()).then(|| EtcdBlock {
            external: ExternalEtcd {
                endpoints: cfg.etcd_endpoints.to_vec(),
                ca_file: KUBEADM_ETCD_CA_PATH.to_string(),
                cert_file: KUBEADM_ETCD_CLIENT_CERT_PATH.to_string(),
                key_file: KUBEADM_ETCD_CLIENT_KEY_PATH.to_string(),
            },
        }),
    };
    let init_yaml = serde_yaml::to_string(&init)
        .map_err(|e| Error::Invalid(format!("a gerar InitConfiguration: {e}")))?;
    let cluster_yaml = serde_yaml::to_string(&cluster)
        .map_err(|e| Error::Invalid(format!("a gerar ClusterConfiguration: {e}")))?;
    let mut out = format!("{init_yaml}---\n{cluster_yaml}");
    if cfg.kube_proxy == "ipvs" {
        let kp = KubeProxyConfigurationYaml {
            api_version: KUBE_PROXY_API_VERSION,
            kind: "KubeProxyConfiguration",
            mode: "ipvs".to_string(),
        };
        let kp_yaml = serde_yaml::to_string(&kp)
            .map_err(|e| Error::Invalid(format!("a gerar KubeProxyConfiguration: {e}")))?;
        out.push_str("---\n");
        out.push_str(&kp_yaml);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg<'a>(etcd: &'a [String], kube_proxy: &'a str) -> InitConfig<'a> {
        InitConfig {
            k8s_version: Some("1.34.0"),
            control_plane_endpoint: "10.0.0.1:6443",
            pod_subnet: "10.244.0.0/16",
            service_subnet: "10.96.0.0/12",
            cri_socket: "unix:///run/delonix-cri.sock",
            etcd_endpoints: etcd,
            kube_proxy,
        }
    }

    /// The flat-flag path must stay the default: a `--config` that says
    /// nothing the flags already say is a second way to configure a cluster.
    #[test]
    fn render_init_config_recusa_uma_config_que_nao_diz_nada_de_novo() {
        assert!(!needed(&cfg(&[], "iptables")));
        assert!(render_init_config(&cfg(&[], "iptables")).is_err());
        // Either reason on its own is enough to need one.
        assert!(needed(&cfg(
            &["https://10.0.1.1:2379".to_string()],
            "iptables"
        )));
        assert!(needed(&cfg(&[], "ipvs")));
        assert!(needed(&cfg(&[], "none")));
    }

    #[test]
    fn render_init_config_gera_2_documentos_separados_por_tracos() {
        let etcd = [
            "https://10.0.1.1:2379".to_string(),
            "https://10.0.1.2:2379".to_string(),
        ];
        let yaml = render_init_config(&cfg(&etcd, "iptables")).unwrap();
        let docs: Vec<&str> = yaml.split("---\n").collect();
        assert_eq!(docs.len(), 2, "{yaml}");
        assert!(docs[0].contains("kind: InitConfiguration"));
        assert!(docs[0].contains("criSocket: unix:///run/delonix-cri.sock"));
        // No phase is skipped unless a profile replaces kube-proxy.
        assert!(!docs[0].contains("skipPhases"), "{yaml}");
        assert!(docs[1].contains("kind: ClusterConfiguration"));
        assert!(docs[1].contains("kubernetesVersion: v1.34.0"));
        assert!(docs[1].contains("controlPlaneEndpoint: 10.0.0.1:6443"));
        assert!(docs[1].contains("podSubnet: 10.244.0.0/16"));
        assert!(docs[1].contains("serviceSubnet: 10.96.0.0/12"));
        assert!(docs[1].contains("https://10.0.1.1:2379"));
        assert!(docs[1].contains("https://10.0.1.2:2379"));
        assert!(docs[1].contains(&format!("caFile: {KUBEADM_ETCD_CA_PATH}")));
        assert!(docs[1].contains(&format!("certFile: {KUBEADM_ETCD_CLIENT_CERT_PATH}")));
        assert!(docs[1].contains(&format!("keyFile: {KUBEADM_ETCD_CLIENT_KEY_PATH}")));
    }

    #[test]
    fn render_init_config_sem_k8s_version_omite_o_campo() {
        let etcd = ["https://10.0.1.1:2379".to_string()];
        let mut c = cfg(&etcd, "iptables");
        c.k8s_version = None;
        let yaml = render_init_config(&c).unwrap();
        assert!(!yaml.contains("kubernetesVersion"), "{yaml}");
    }

    /// Stacked etcd (the default) must not grow an `etcd:` block just because
    /// a different reason brought us onto the `--config` path. A block that
    /// claims to configure etcd and configures nothing is worse than none.
    #[test]
    fn a_config_reached_for_kube_proxy_does_not_claim_to_configure_etcd() {
        let yaml = render_init_config(&cfg(&[], "ipvs")).unwrap();
        assert!(!yaml.contains("etcd"), "{yaml}");
        assert!(!yaml.contains("caFile"), "{yaml}");
    }

    /// `kubeProxy: none` means the CNI replaces it, so kubeadm must not
    /// install it at all — two owners of the service dataplane is a cluster
    /// whose behaviour depends on who wrote a rule last.
    #[test]
    fn kube_proxy_none_skips_the_addon_phase_and_renders_no_kube_proxy_config() {
        let yaml = render_init_config(&cfg(&[], "none")).unwrap();
        assert!(yaml.contains("skipPhases:"), "{yaml}");
        assert!(yaml.contains("addon/kube-proxy"), "{yaml}");
        // Nothing to configure if it is never installed.
        assert!(!yaml.contains("KubeProxyConfiguration"), "{yaml}");
        let docs: Vec<&str> = yaml.split("---\n").collect();
        assert_eq!(docs.len(), 2, "{yaml}");
    }

    /// `ipvs` is the opposite: kube-proxy IS installed, and told its mode.
    #[test]
    fn kube_proxy_ipvs_renders_a_third_document_and_skips_nothing() {
        let yaml = render_init_config(&cfg(&[], "ipvs")).unwrap();
        let docs: Vec<&str> = yaml.split("---\n").collect();
        assert_eq!(docs.len(), 3, "{yaml}");
        assert!(!docs[0].contains("skipPhases"), "{yaml}");
        assert!(docs[2].contains("kind: KubeProxyConfiguration"), "{yaml}");
        assert!(docs[2].contains(KUBE_PROXY_API_VERSION), "{yaml}");
        assert!(docs[2].contains("mode: ipvs"), "{yaml}");
    }

    /// Both reasons at once — external etcd AND a replaced kube-proxy.
    #[test]
    fn both_reasons_at_once_render_one_coherent_document_set() {
        let etcd = ["https://10.0.1.1:2379".to_string()];
        let yaml = render_init_config(&cfg(&etcd, "none")).unwrap();
        assert!(yaml.contains("addon/kube-proxy"), "{yaml}");
        assert!(
            yaml.contains(&format!("caFile: {KUBEADM_ETCD_CA_PATH}")),
            "{yaml}"
        );
        assert_eq!(yaml.split("---\n").count(), 2, "{yaml}");
    }
}

//! Renders the `kubeadm init --config=...` YAML needed for external etcd.
//!
//! `kubeadm init`'s flat CLI flags (`--pod-network-cidr`, `--service-cidr`, ...)
//! cannot express `ClusterConfiguration.etcd.external` — that field only exists
//! in the `--config` document. The `stacked` (default) path in `cmd::cluster`
//! keeps using the flat flags unchanged; this module is only reached on the
//! `etcd.mode: external` branch, so it carries zero regression risk to the
//! default behavior. Hand-built via typed structs + `serde_yaml` (already a
//! dependency, used elsewhere for manifest round-tripping) — no
//! `kubeadm-types` crate exists or is added for this.

use delonix_runtime_core::{Error, Result};
use serde::Serialize;

const KUBEADM_API_VERSION: &str = "kubeadm.k8s.io/v1beta3";

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
    etcd: EtcdBlock,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExternalEtcd {
    endpoints: Vec<String>,
    ca_file: String,
    cert_file: String,
    key_file: String,
}

/// Renders the 2-document `kubeadm init --config=` YAML for the external-etcd
/// branch. `etcd_endpoints` must be non-empty (callers only reach this
/// function when `spec.etcd.mode == "external"`, which `validate()` already
/// guarantees has at least 1 host).
pub(crate) fn render_init_config(
    k8s_version: Option<&str>,
    control_plane_endpoint: &str,
    pod_subnet: &str,
    service_subnet: &str,
    cri_socket: &str,
    etcd_endpoints: &[String],
) -> Result<String> {
    if etcd_endpoints.is_empty() {
        return Err(Error::Invalid(
            "render_init_config: etcd_endpoints vazio — chamador tinha de garantir isto antes"
                .into(),
        ));
    }
    let init = InitConfigurationYaml {
        api_version: KUBEADM_API_VERSION,
        kind: "InitConfiguration",
        node_registration: NodeRegistration {
            cri_socket: cri_socket.to_string(),
        },
    };
    let cluster = ClusterConfigurationYaml {
        api_version: KUBEADM_API_VERSION,
        kind: "ClusterConfiguration",
        kubernetes_version: k8s_version.map(|v| format!("v{v}")),
        control_plane_endpoint: control_plane_endpoint.to_string(),
        networking: Networking {
            pod_subnet: pod_subnet.to_string(),
            service_subnet: service_subnet.to_string(),
        },
        etcd: EtcdBlock {
            external: ExternalEtcd {
                endpoints: etcd_endpoints.to_vec(),
                ca_file: KUBEADM_ETCD_CA_PATH.to_string(),
                cert_file: KUBEADM_ETCD_CLIENT_CERT_PATH.to_string(),
                key_file: KUBEADM_ETCD_CLIENT_KEY_PATH.to_string(),
            },
        },
    };
    let init_yaml = serde_yaml::to_string(&init)
        .map_err(|e| Error::Invalid(format!("a gerar InitConfiguration: {e}")))?;
    let cluster_yaml = serde_yaml::to_string(&cluster)
        .map_err(|e| Error::Invalid(format!("a gerar ClusterConfiguration: {e}")))?;
    Ok(format!("{init_yaml}---\n{cluster_yaml}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_init_config_recusa_endpoints_vazios() {
        assert!(render_init_config(
            None,
            "10.0.0.1:6443",
            "10.244.0.0/16",
            "10.96.0.0/12",
            "unix:///run/delonix-cri.sock",
            &[]
        )
        .is_err());
    }

    #[test]
    fn render_init_config_gera_2_documentos_separados_por_tracos() {
        let yaml = render_init_config(
            Some("1.34.0"),
            "10.0.0.1:6443",
            "10.244.0.0/16",
            "10.96.0.0/12",
            "unix:///run/delonix-cri.sock",
            &[
                "https://10.0.1.1:2379".to_string(),
                "https://10.0.1.2:2379".to_string(),
            ],
        )
        .unwrap();
        let docs: Vec<&str> = yaml.split("---\n").collect();
        assert_eq!(docs.len(), 2, "{yaml}");
        assert!(docs[0].contains("kind: InitConfiguration"));
        assert!(docs[0].contains("criSocket: unix:///run/delonix-cri.sock"));
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
        let yaml = render_init_config(
            None,
            "10.0.0.1:6443",
            "10.244.0.0/16",
            "10.96.0.0/12",
            "unix:///run/delonix-cri.sock",
            &["https://10.0.1.1:2379".to_string()],
        )
        .unwrap();
        assert!(!yaml.contains("kubernetesVersion"), "{yaml}");
    }
}

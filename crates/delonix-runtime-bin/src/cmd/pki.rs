//! Generic CA + leaf certificate issuance — used by `cmd::etcd` to build the
//! mTLS PKI a dedicated (`etcd.mode: external`) etcd cluster needs (member
//! peer/server certs + one `apiserver-etcd-client` leaf for kube-apiserver).
//!
//! The only prior cert-generation code in this codebase, `ingress_proxy::
//! self_signed_pem`, wraps `rcgen`'s ONE-SHOT `generate_simple_self_signed`
//! convenience function — a single self-signed leaf, no issuer parameter at
//! all, sufficient for a lone HTTPS listener but not for a cluster where
//! every member's cert has to validate against a COMMON root. This module
//! uses `rcgen`'s lower-level API (`CertificateParams::self_signed`/
//! `.signed_by()`, both already present in the vendored `rcgen 0.13.2` — no
//! version bump) to build a real CA that signs N leaves.

use delonix_runtime_core::{Error, Result};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};

/// A CA's cert + key, kept LIVE (not just PEM) so multiple leaves can be
/// signed in the same process run. v1 never reloads a CA key from disk (no
/// post-bootstrap member add — see CLAUDE.md), so PEM round-tripping of the
/// CA key back into `rcgen` types is out of scope.
pub(crate) struct CaMaterial {
    pub cert_pem: String,
    pub key_pem: String,
    cert: rcgen::Certificate,
    key: KeyPair,
}

pub(crate) struct LeafMaterial {
    pub cert_pem: String,
    pub key_pem: String,
}

fn rcgen_err(context: &'static str) -> impl Fn(rcgen::Error) -> Error {
    move |e| Error::Runtime {
        context,
        message: e.to_string(),
    }
}

/// Generates a self-signed CA (`CA:TRUE`, unconstrained path length).
pub(crate) fn generate_ca(common_name: &str) -> Result<CaMaterial> {
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(rcgen_err("etcd CA"))?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    let key = KeyPair::generate().map_err(rcgen_err("etcd CA"))?;
    let cert = params.self_signed(&key).map_err(rcgen_err("etcd CA"))?;
    Ok(CaMaterial {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        cert,
        key,
    })
}

/// Issues a leaf cert signed by `ca`, with the given SANs. `CertificateParams::new`
/// sniffs each string and picks an IP or a DNS SAN automatically — callers pass a
/// mix of IPs and hostnames freely.
pub(crate) fn issue_leaf(
    ca: &CaMaterial,
    common_name: &str,
    sans: &[String],
) -> Result<LeafMaterial> {
    let mut params = CertificateParams::new(sans.to_vec()).map_err(rcgen_err("etcd leaf cert"))?;
    params.is_ca = IsCa::NoCa;
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    let key = KeyPair::generate().map_err(rcgen_err("etcd leaf cert"))?;
    let cert = params
        .signed_by(&key, &ca.cert, &ca.key)
        .map_err(rcgen_err("etcd leaf cert"))?;
    Ok(LeafMaterial {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_ca_produz_pem_valido() {
        let ca = generate_ca("test-etcd-ca").unwrap();
        assert!(ca.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(ca.key_pem.contains("PRIVATE KEY"));
    }

    /// The real regression risk for a CA-signs-N-leaves helper: accidental
    /// key reuse across members, which would let any member impersonate any
    /// other over etcd's peer mTLS.
    #[test]
    fn issue_leaf_gera_chaves_e_certificados_distintos_por_folha() {
        let ca = generate_ca("test-etcd-ca").unwrap();
        let leaf1 = issue_leaf(&ca, "etcd-1", &["10.0.0.1".to_string()]).unwrap();
        let leaf2 = issue_leaf(&ca, "etcd-2", &["10.0.0.2".to_string()]).unwrap();
        assert_ne!(leaf1.key_pem, leaf2.key_pem);
        assert_ne!(leaf1.cert_pem, leaf2.cert_pem);
        assert!(leaf1.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(leaf1.key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn issue_leaf_nunca_e_igual_a_ca_que_o_assinou() {
        let ca = generate_ca("test-etcd-ca").unwrap();
        let leaf = issue_leaf(&ca, "etcd-1", &["10.0.0.1".to_string()]).unwrap();
        assert_ne!(leaf.cert_pem, ca.cert_pem);
        assert_ne!(leaf.key_pem, ca.key_pem);
    }

    #[test]
    fn issue_leaf_aceita_mistura_de_ip_e_hostname_como_sans() {
        let ca = generate_ca("test-etcd-ca").unwrap();
        let leaf = issue_leaf(
            &ca,
            "etcd-1",
            &[
                "10.0.0.1".to_string(),
                "etcd-1".to_string(),
                "localhost".to_string(),
                "127.0.0.1".to_string(),
            ],
        );
        assert!(leaf.is_ok());
    }
}

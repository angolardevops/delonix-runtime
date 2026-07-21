//! Image signature verification (B8) — compatible with **cosign/sigstore**.
//!
//! Cosign model (`cosign sign --key`): the signature of an image with manifest
//! digest `sha256:<D>` is stored as a **separate OCI artifact**, in the
//! same repository, with the tag `sha256-<D>.sig`. That artifact is a manifest
//! whose *layers* have:
//! - `mediaType: application/vnd.dev.cosign.simplesigning.v1+json`;
//! - the layer BLOB = the *payload* (JSON with `critical.image.docker-manifest-digest`);
//! - the annotation `dev.cosignproject.cosign/signature` = the ECDSA signature (DER,
//!   base64) over the payload bytes.
//!
//! Verifying = (1) resolve the digest of the image manifest, (2) fetch the
//! `.sig` artifact, (3) confirm the ECDSA-P256 signature over the payload with the
//! **trusted public key**, and (4) confirm that the payload points to the
//! image digest (prevents reusing a signature on another image).

use crate::cas::{sha256_hex, strip};
use crate::registry::{registry_client, RegistryClient};
use crate::ImageStore;
use base64::Engine;
use delonix_runtime_core::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

const COSIGN_SIG_ANNOTATION: &str = "dev.cosignproject.cosign/signature";

#[derive(Deserialize)]
struct SigManifest {
    #[serde(default)]
    layers: Vec<SigLayer>,
}
#[derive(Deserialize)]
struct SigLayer {
    digest: String,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

/// The cosign simple-signing payload (only the fields we validate).
#[derive(Deserialize)]
struct Payload {
    critical: Critical,
}
#[derive(Deserialize)]
struct Critical {
    image: ImageRef,
}
#[derive(Deserialize)]
struct ImageRef {
    #[serde(rename = "docker-manifest-digest")]
    docker_manifest_digest: String,
}

/// Extracts the P-256 public point (`04 || X || Y`, 65 bytes) from a PEM
/// public key (SPKI `BEGIN PUBLIC KEY`). For P-256, the point is the last 65 bytes of
/// the SPKI DER (the final `BIT STRING`). `ring` expects the point, not the SPKI.
fn p256_point_from_pem(pem: &str) -> Result<Vec<u8>> {
    let mut b64 = String::new();
    let mut inside = false;
    for line in pem.lines() {
        let l = line.trim();
        if l.starts_with("-----BEGIN ") && l.contains("PUBLIC KEY") {
            inside = true;
        } else if l.starts_with("-----END ") {
            break;
        } else if inside {
            b64.push_str(l);
        }
    }
    if b64.is_empty() {
        return Err(Error::Invalid(
            "invalid PEM public key (no PUBLIC KEY block)".into(),
        ));
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .map_err(|e| Error::Invalid(format!("invalid base64 public key: {e}")))?;
    if der.len() < 65 || der[der.len() - 65] != 0x04 {
        return Err(Error::Invalid(
            "public key does not look like an uncompressed P-256 point".into(),
        ));
    }
    Ok(der[der.len() - 65..].to_vec())
}

/// Verifies an ECDSA-P256-SHA256 signature (DER) over `msg` with the `point`.
fn verify_ecdsa_p256(point: &[u8], msg: &[u8], sig_der: &[u8]) -> bool {
    let key =
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, point);
    key.verify(msg, sig_der).is_ok()
}

/// Verifies the cosign signature of `reference` with the public key `pubkey_pem`.
/// Returns the digest of the verified manifest, or an instructive error if the image
/// is not signed or the signature does not check out.
pub fn verify_signature(store: &ImageStore, reference: &str, pubkey_pem: &str) -> Result<String> {
    let point = p256_point_from_pem(pubkey_pem)?;
    let mut c: RegistryClient = registry_client(store, reference)?;

    // 1) digest of the image manifest (what cosign signs for the tag).
    let manifest_bytes = c.get_manifest(&c.reference())?;
    let hex = sha256_hex(&manifest_bytes);
    let digest = format!("sha256:{hex}");

    // 2) signature artifact: tag `sha256-<hex>.sig`.
    let sig_tag = format!("sha256-{hex}.sig");
    let sig_bytes = c.get_manifest(&sig_tag).map_err(|_| {
        Error::Invalid(format!(
            "image not signed: no cosign signature for {reference} ({digest})"
        ))
    })?;
    let sig_manifest: SigManifest = serde_json::from_slice(&sig_bytes)
        .map_err(|e| Error::Invalid(format!("invalid signature manifest: {e}")))?;

    // 3) + 4) for each layer: payload + signature in the annotation.
    for layer in &sig_manifest.layers {
        let Some(sig_b64) = layer.annotations.get(COSIGN_SIG_ANNOTATION) else {
            continue;
        };
        let Ok(sig) = base64::engine::general_purpose::STANDARD.decode(sig_b64.trim()) else {
            continue;
        };
        let payload = c.get_blob(&layer.digest)?;
        if !verify_ecdsa_p256(&point, &payload, &sig) {
            continue; // signature does not check out with this key
        }
        // bind the signature to THIS image (anti-reuse).
        let parsed: Payload = serde_json::from_slice(&payload)
            .map_err(|e| Error::Invalid(format!("invalid signature payload: {e}")))?;
        if strip(&parsed.critical.image.docker_manifest_digest) == hex {
            return Ok(digest);
        }
    }
    Err(Error::Invalid(format!(
        "invalid signature: no signature for {reference} matches the given key"
    )))
}

#[cfg(test)]
mod tests {
    use super::{p256_point_from_pem, verify_ecdsa_p256};

    #[test]
    fn ecdsa_p256_roundtrip_and_tamper() {
        use ring::rand::SystemRandom;
        use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let kp = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let msg = b"delonix container image signature payload";
        let sig = kp.sign(&rng, msg).unwrap();
        let point = kp.public_key().as_ref(); // uncompressed point 04||X||Y
                                              // genuine signature checks out; tampered message does not.
        assert!(verify_ecdsa_p256(point, msg, sig.as_ref()));
        assert!(!verify_ecdsa_p256(point, b"tampered", sig.as_ref()));
    }

    #[test]
    fn rejects_non_pem_pubkey() {
        assert!(p256_point_from_pem("not a pem").is_err());
    }
}

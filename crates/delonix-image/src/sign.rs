//! Verificação de assinaturas de imagens (B8) — compatível com **cosign/sigstore**.
//!
//! Modelo do cosign (`cosign sign --key`): a assinatura de uma imagem com digest
//! de manifesto `sha256:<D>` é guardada como um **artefacto OCI separado**, no
//! mesmo repositório, com a tag `sha256-<D>.sig`. Esse artefacto é um manifesto
//! cujos *layers* têm:
//! - `mediaType: application/vnd.dev.cosign.simplesigning.v1+json`;
//! - o BLOB do layer = o *payload* (JSON com `critical.image.docker-manifest-digest`);
//! - a anotação `dev.cosignproject.cosign/signature` = a assinatura ECDSA (DER,
//!   base64) sobre os bytes do payload.
//!
//! Verificar = (1) resolver o digest do manifesto da imagem, (2) buscar o
//! artefacto `.sig`, (3) confirmar a assinatura ECDSA-P256 sobre o payload com a
//! **chave pública de confiança**, e (4) confirmar que o payload aponta para o
//! digest da imagem (impede reutilizar uma assinatura noutra imagem).

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

/// O payload simple-signing do cosign (só os campos que validamos).
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

/// Extrai o ponto público P-256 (`04 || X || Y`, 65 bytes) de uma chave pública
/// PEM (SPKI `BEGIN PUBLIC KEY`). Para P-256, o ponto são os últimos 65 bytes do
/// SPKI DER (a `BIT STRING` final). `ring` espera o ponto, não o SPKI.
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

/// Verifica uma assinatura ECDSA-P256-SHA256 (DER) sobre `msg` com o `ponto`.
fn verify_ecdsa_p256(point: &[u8], msg: &[u8], sig_der: &[u8]) -> bool {
    let key =
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, point);
    key.verify(msg, sig_der).is_ok()
}

/// Verifica a assinatura cosign de `reference` com a chave pública `pubkey_pem`.
/// Devolve o digest do manifesto verificado, ou um erro didáctico se a imagem
/// não estiver assinada ou a assinatura não conferir.
pub fn verify_signature(store: &ImageStore, reference: &str, pubkey_pem: &str) -> Result<String> {
    let point = p256_point_from_pem(pubkey_pem)?;
    let mut c: RegistryClient = registry_client(store, reference)?;

    // 1) digest do manifesto da imagem (o que o cosign assina para a tag).
    let manifest_bytes = c.get_manifest(&c.reference())?;
    let hex = sha256_hex(&manifest_bytes);
    let digest = format!("sha256:{hex}");

    // 2) artefacto de assinatura: tag `sha256-<hex>.sig`.
    let sig_tag = format!("sha256-{hex}.sig");
    let sig_bytes = c.get_manifest(&sig_tag).map_err(|_| {
        Error::Invalid(format!(
            "image not signed: no cosign signature for {reference} ({digest})"
        ))
    })?;
    let sig_manifest: SigManifest = serde_json::from_slice(&sig_bytes)
        .map_err(|e| Error::Invalid(format!("invalid signature manifest: {e}")))?;

    // 3) + 4) para cada layer: payload + assinatura na anotação.
    for layer in &sig_manifest.layers {
        let Some(sig_b64) = layer.annotations.get(COSIGN_SIG_ANNOTATION) else {
            continue;
        };
        let Ok(sig) = base64::engine::general_purpose::STANDARD.decode(sig_b64.trim()) else {
            continue;
        };
        let payload = c.get_blob(&layer.digest)?;
        if !verify_ecdsa_p256(&point, &payload, &sig) {
            continue; // assinatura não confere com esta chave
        }
        // liga a assinatura A ESTA imagem (anti-reutilização).
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
        let point = kp.public_key().as_ref(); // ponto não-comprimido 04||X||Y
                                              // assinatura genuína confere; mensagem adulterada não.
        assert!(verify_ecdsa_p256(point, msg, sig.as_ref()));
        assert!(!verify_ecdsa_p256(point, b"tampered", sig.as_ref()));
    }

    #[test]
    fn rejects_non_pem_pubkey() {
        assert!(p256_point_from_pem("not a pem").is_err());
    }
}

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
use crate::registry::{
    parse_reference, push_oci_artifact_with_layer_annotations, registry_client, RegistryClient,
};
use crate::ImageStore;
use base64::Engine;
use delonix_runtime_core::{write_atomic_mode, Error, Result};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const COSIGN_SIG_ANNOTATION: &str = "dev.cosignproject.cosign/signature";
const COSIGN_SIG_MEDIA_TYPE: &str = "application/vnd.dev.cosign.simplesigning.v1+json";
/// The fixed SPKI (`SubjectPublicKeyInfo`) prefix for an uncompressed P-256
/// point: `SEQUENCE { SEQUENCE { OID id-ecPublicKey, OID prime256v1 },
/// BIT STRING { 0 unused bits, <point> } }` (RFC 5480). Only the trailing
/// 65-byte point varies between keys, so the prefix is a compile-time
/// constant rather than something re-derived per call.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

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
    // The discarded cause used to make every failure read "image not signed".
    // A registry that answers 500, a token that expired, a network that is down
    // — all of them came out as a VERDICT about the image. That is the worst
    // possible confusion on a signature check: the operator concludes the
    // artifact is unsigned and goes to re-sign it, when the registry was simply
    // unreachable. Absence of the `.sig` tag is a verdict; anything else is an
    // "I could not tell", and the two must not wear the same sentence.
    let sig_bytes = c.get_manifest(&sig_tag).map_err(|e| match e {
        // A 404 on the `.sig` tag is the VERDICT: the artifact is not there.
        Error::NotFound(_) => Error::Invalid(format!(
            "image not signed: no cosign signature for {reference} ({digest})"
        )),
        // Everything else is an "I could not tell", and it used to wear the
        // same sentence as the verdict. A registry answering 500, an expired
        // token, a network that is down — all of them read as a statement ABOUT
        // THE IMAGE, so the operator concludes the artifact is unsigned and
        // goes to re-sign it. On a signature check that is the worst confusion
        // available: absence of proof presented as proof of absence.
        other => Error::Invalid(format!(
            "could not determine whether {reference} ({digest}) is signed — the \
registry did not answer for {sig_tag}: {other}"
        )),
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

/// Wraps a PEM body (base64, 64 chars/line) between the given `label`'s
/// `-----BEGIN`/`-----END` markers.
fn pem_wrap(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut body = String::new();
    for chunk in b64.as_bytes().chunks(64) {
        // Each chunk is a slice of an ASCII base64 string, always valid UTF-8.
        body.push_str(std::str::from_utf8(chunk).unwrap());
        body.push('\n');
    }
    format!("-----BEGIN {label}-----\n{body}-----END {label}-----\n")
}

/// The base64 body between a PEM's `-----BEGIN <label>-----`/`-----END-----`
/// markers, decoded to raw DER. `label` is matched as a substring (so
/// `"PRIVATE KEY"` also matches `"EC PRIVATE KEY"`), the same tolerance
/// [`p256_point_from_pem`] already has for `"PUBLIC KEY"`.
fn pem_unwrap(pem: &str, label: &str) -> Result<Vec<u8>> {
    let mut b64 = String::new();
    let mut inside = false;
    for line in pem.lines() {
        let l = line.trim();
        if l.starts_with("-----BEGIN ") && l.contains(label) {
            inside = true;
        } else if l.starts_with("-----END ") {
            break;
        } else if inside {
            b64.push_str(l);
        }
    }
    if b64.is_empty() {
        return Err(Error::Invalid(format!("invalid PEM (no {label} block)")));
    }
    base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .map_err(|e| Error::Invalid(format!("invalid base64 in PEM: {e}")))
}

/// The inverse of [`p256_point_from_pem`]: wraps a 65-byte uncompressed P-256
/// point (`04 || X || Y`, as `EcdsaKeyPair::public_key()` returns it) in the
/// SPKI PEM form a real X.509/TLS tool expects. Round-trip tested against
/// [`p256_point_from_pem`] rather than by inspecting the PEM text — the DER
/// framing is exactly the part a visual check would not catch a mistake in.
fn p256_point_to_pem(point: &[u8]) -> Result<String> {
    if point.len() != 65 || point[0] != 0x04 {
        return Err(Error::Invalid(
            "not an uncompressed P-256 point (expected 65 bytes starting with 0x04)".into(),
        ));
    }
    let mut der = Vec::with_capacity(P256_SPKI_PREFIX.len() + point.len());
    der.extend_from_slice(&P256_SPKI_PREFIX);
    der.extend_from_slice(point);
    Ok(pem_wrap("PUBLIC KEY", &der))
}

/// Where a node's image-signing private key lives by default — one key per
/// `DELONIX_ROOT`, generated on first use. Plain PEM, `0600`, NOT wrapped by
/// the `CredVault` (same treatment already accepted for `cluster kubeadm`'s
/// `id_ed25519`: private key material that is not the vault's master key,
/// stored in clear under `DELONIX_ROOT` — encrypting it would be
/// inconsistent with that precedent for no new reason).
pub fn default_signing_key_path(root: &Path) -> PathBuf {
    root.join("keys").join("image-signing.key")
}

/// Loads the signing key at `path`, generating and persisting a fresh
/// ECDSA-P256 key there if it does not exist yet. `write_atomic_mode` gives
/// the file its `0600` mode ATOMICALLY at creation — the same TOCTOU class
/// this repo's own secret store closed (a widen-then-narrow `chmod` leaves a
/// window where another local user can read the key before it is private).
fn ensure_signing_key(path: &Path) -> Result<EcdsaKeyPair> {
    let rng = SystemRandom::new();
    if let Ok(pem) = std::fs::read_to_string(path) {
        let pkcs8 = pem_unwrap(&pem, "PRIVATE KEY")?;
        return EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8, &rng).map_err(
            |e| Error::Invalid(format!("invalid signing key at {}: {e}", path.display())),
        );
    }
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .map_err(|e| Error::Invalid(format!("could not generate a signing key: {e}")))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic_mode(
        path,
        pem_wrap("PRIVATE KEY", pkcs8.as_ref()).as_bytes(),
        Some(0o600),
    )?;
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
        .map_err(|e| Error::Invalid(format!("just-generated signing key rejected itself: {e}")))
}

/// Builds the cosign-compatible signature target for `reference`: the SAME
/// host/repo, with the tag replaced by `sha256-<hex>.sig`. String
/// reconstruction (not a second `Client`/store) because [`parse_reference`]
/// is pure and total — round-tripping `host/repo:tag` through it always
/// re-derives the same host/repo, including the Docker Hub default
/// (`registry-1.docker.io` contains a `.`, so it is never re-collapsed to a
/// bare name) and the `library/` prefix (already baked into `repo` from the
/// first parse, and a repo containing `/` is never re-prefixed).
fn signature_target(reference: &str, hex: &str) -> String {
    let (host, repo, _) = parse_reference(reference);
    format!("{host}/{repo}:sha256-{hex}.sig")
}

/// Signs `reference` with the key at `key_path` (generated there on first
/// use if absent) and publishes the signature as a separate cosign-format
/// OCI artifact in the SAME repository — the only place a later
/// `image verify` knows to look. Refuses to overwrite an existing signature
/// unless `force`, distinguishing "already signed" (a real 404 was absent)
/// from "could not tell" (the registry did not answer), same discipline
/// [`verify_signature`] already applies to the read side.
///
/// Returns `(digest, public_key_path)` — the public key is written NEXT TO
/// the private one (`.pub`, world-readable, non-secret) so the caller can
/// print where to find it; the private key itself is never returned or
/// printed.
pub fn sign_image(
    store: &ImageStore,
    reference: &str,
    key_path: &Path,
    force: bool,
) -> Result<(String, PathBuf)> {
    let mut c: RegistryClient = registry_client(store, reference)?;
    let manifest_bytes = c.get_manifest(&c.reference())?;
    let hex = sha256_hex(&manifest_bytes);
    let digest = format!("sha256:{hex}");
    let sig_tag = format!("sha256-{hex}.sig");

    if !force {
        match c.get_manifest(&sig_tag) {
            // The tag exists — a signature is already there.
            Ok(_) => {
                return Err(Error::Invalid(format!(
                    "{reference} ({digest}) is already signed — pass --force to sign it again"
                )))
            }
            // A real 404 on the `.sig` tag is the verdict: nothing signed it yet.
            Err(Error::NotFound(_)) => {}
            // Anything else is "could not tell", and must not read as either verdict —
            // the same distinction `verify_signature` makes on this same read.
            Err(other) => {
                return Err(Error::Invalid(format!(
                    "could not determine whether {reference} ({digest}) is already signed — \
the registry did not answer for {sig_tag}: {other}"
                )))
            }
        }
    }

    let key_pair = ensure_signing_key(key_path)?;
    let pubkey_path = key_path.with_extension("pub");
    let pubkey_pem = p256_point_to_pem(key_pair.public_key().as_ref())?;
    // The public key is not secret — 0644 is correct, and it must be
    // (re)written on every sign so it always matches whichever private key
    // just signed, including the very first time this key is used.
    write_atomic_mode(&pubkey_path, pubkey_pem.as_bytes(), Some(0o644))?;

    // Cosign's "simple signing" payload — only `critical.image.docker-manifest-digest`
    // is validated by `verify_signature`, but the rest is filled in for
    // compatibility with real cosign/sigstore tooling reading the same artifact.
    let (_, repo, _) = parse_reference(reference);
    let payload = serde_json::json!({
        "critical": {
            "identity": {"docker-reference": repo},
            "image": {"docker-manifest-digest": digest},
            "type": "cosign container image signature",
        },
        "optional": null,
    });
    let payload_bytes = serde_json::to_vec(&payload)?;

    let rng = SystemRandom::new();
    let sig = key_pair
        .sign(&rng, &payload_bytes)
        .map_err(|e| Error::Invalid(format!("could not sign: {e}")))?;
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.as_ref());

    let mut annotations = BTreeMap::new();
    annotations.insert(COSIGN_SIG_ANNOTATION.to_string(), sig_b64);
    let target = signature_target(reference, &hex);
    push_oci_artifact_with_layer_annotations(
        store.root(),
        &target,
        COSIGN_SIG_MEDIA_TYPE,
        &payload_bytes,
        &annotations,
    )?;

    Ok((digest, pubkey_path))
}

#[cfg(test)]
mod tests {
    use super::{
        default_signing_key_path, ensure_signing_key, p256_point_from_pem, p256_point_to_pem,
        sign_image, signature_target, verify_ecdsa_p256, verify_signature,
    };

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

    /// The roundtrip the plan calls for: generate → wrap → the EXISTING
    /// parser reads back the SAME point. A visual inspection of the PEM
    /// text would not catch a mistake in the DER framing; only feeding it
    /// back to the reader that has to consume it in production would.
    #[test]
    fn p256_point_to_pem_roundtrips_through_the_existing_parser() {
        use ring::rand::SystemRandom;
        use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let kp = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let point = kp.public_key().as_ref().to_vec();
        let pem = p256_point_to_pem(&point).unwrap();
        assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----\n"));
        let parsed = p256_point_from_pem(&pem).unwrap();
        assert_eq!(parsed, point);
    }

    #[test]
    fn p256_point_to_pem_rejects_the_wrong_shape() {
        assert!(p256_point_to_pem(&[0u8; 64]).is_err()); // wrong length
        assert!(p256_point_to_pem(&[0u8; 65]).is_err()); // right length, wrong tag byte
    }

    /// `ensure_signing_key` generates on first use, persists at `0600`, and
    /// a second call against the SAME path loads the identical key back —
    /// never silently regenerating (which would invalidate every signature
    /// already made with it).
    #[test]
    fn ensure_signing_key_generates_once_then_persists() {
        use ring::signature::KeyPair;
        let dir = std::env::temp_dir().join(format!(
            "dlx-signkey-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("nested").join("image-signing.key");

        let first = ensure_signing_key(&path).unwrap();
        let point1 = first.public_key().as_ref().to_vec();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "signing key must be 0600, got {mode:o}");
        }

        let second = ensure_signing_key(&path).unwrap();
        let point2 = second.public_key().as_ref().to_vec();
        assert_eq!(
            point1, point2,
            "loading an existing key must not regenerate it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_signing_key_path_lives_under_keys() {
        let root = std::path::Path::new("/tmp/dlx-root-example");
        assert_eq!(
            default_signing_key_path(root),
            root.join("keys").join("image-signing.key")
        );
    }

    /// `parse_reference` round-trips host/repo through `host/repo:tag` —
    /// this is the property that makes rebuilding a string (instead of
    /// reusing the open client) safe for the Docker Hub default too.
    #[test]
    fn signature_target_keeps_the_same_repo_docker_hub_default() {
        assert_eq!(
            signature_target("alpine:latest", "deadbeef"),
            "registry-1.docker.io/library/alpine:sha256-deadbeef.sig"
        );
    }

    #[test]
    fn signature_target_keeps_the_same_repo_explicit_host() {
        assert_eq!(
            signature_target("ghcr.io/angolardevops/delonix-vm-k8s:1.34", "cafef00d"),
            "ghcr.io/angolardevops/delonix-vm-k8s:sha256-cafef00d.sig"
        );
    }

    /// End-to-end against a local OCI registry mock (the SAME one
    /// `registry.rs`'s own push/pull round-trip test uses, not a second
    /// copy — see the doc-comment on `serve_anon_registry`): sign a real
    /// artifact, then verify it back with the key `sign_image` just wrote.
    /// No network, no real registry.
    #[test]
    fn sign_then_verify_round_trip_against_a_local_registry() {
        use crate::registry::{push_oci_artifact, serve_anon_registry};
        use ring::signature::KeyPair;
        let (port, _blob_gets, _handle) = serve_anon_registry();
        let tmp = std::env::temp_dir().join(format!(
            "dlx-image-sign-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = crate::ImageStore::open(&tmp).unwrap();
        let reference = format!("127.0.0.1:{port}/repo:tag");
        let key_path = tmp.join("keys").join("image-signing.key");

        // an artifact has to exist before it can be signed.
        push_oci_artifact(&tmp, &reference, "application/octet-stream", b"hello").unwrap();

        let (digest, pubkey_path) = sign_image(&store, &reference, &key_path, false).unwrap();
        assert!(pubkey_path.ends_with("image-signing.pub"));
        let pubkey_pem = std::fs::read_to_string(&pubkey_path).unwrap();

        let verified = verify_signature(&store, &reference, &pubkey_pem).unwrap();
        assert_eq!(verified, digest);

        // signing again without --force is a refusal, not a silent overwrite.
        let err = sign_image(&store, &reference, &key_path, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already signed"), "{err}");

        // --force signs again and verification still checks out.
        let (digest2, _) = sign_image(&store, &reference, &key_path, true).unwrap();
        assert_eq!(digest2, digest);
        assert_eq!(
            verify_signature(&store, &reference, &pubkey_pem).unwrap(),
            digest
        );

        // a DIFFERENT key must not verify this signature.
        let other_key_path = tmp.join("keys").join("other.key");
        let other_kp = ensure_signing_key(&other_key_path).unwrap();
        let other_pubkey_pem = p256_point_to_pem(other_kp.public_key().as_ref()).unwrap();
        assert!(verify_signature(&store, &reference, &other_pubkey_pem).is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

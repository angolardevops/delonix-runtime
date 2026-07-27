//! Writing an image OUT of the store as an **OCI image layout archive** — the
//! inverse of [`crate::load::load_docker_archive`], and the local-only
//! counterpart of [`crate::registry::push_to_registry`].
//!
//! # Why this exists
//!
//! `delonix cluster load` (the equivalent of `kind load docker-image`) has to
//! hand a locally-built image to the containerd running INSIDE a `kindest/node`,
//! with no registry in between. `ctr images import` reads exactly this format,
//! so the store's blobs are re-packed as-is — nothing is re-compressed, re-hashed
//! or rebuilt, and the digests the node ends up with are byte-identical to ours.
//!
//! Not to be confused with `ImageStore::export_rootfs`/`delonix image export`,
//! which produce an OCI **runtime bundle** (an unpacked rootfs + `config.json`
//! for `runc`/`crun`) — a different artifact for a different consumer.

use crate::cas::strip;
use crate::image::{Image, ImageStore};
use crate::registry::build_manifest;
use delonix_runtime_core::Result;
use std::collections::HashMap;
use std::path::Path;

/// Annotation containerd reads to NAME the imported image. Without it, `ctr
/// images import` still ingests the blobs but registers no reference, and the
/// kubelet can never resolve `image: <repo>:<tag>` — the import would look like
/// it worked and the pod would still land in `ErrImagePull`.
const CONTAINERD_IMAGE_NAME: &str = "io.containerd.image.name";
/// The standard OCI equivalent, written alongside so the archive is also
/// meaningful to readers that are not containerd (`skopeo`, `crane`, ...).
const OCI_REF_NAME: &str = "org.opencontainers.image.ref.name";

/// Writes `image` to `dest` as an OCI image layout archive, named `ref_name`
/// (a full `repo:tag`) for the importer.
///
/// The layout is the minimal legal one: `oci-layout`, `index.json`, and the
/// `blobs/sha256/<hex>` referenced by the manifest — config first, then the
/// layers in order. The manifest itself is the SAME one
/// [`crate::registry::build_manifest`] publishes to a registry, so a registry
/// pull and an archive import of the same local image produce identical content.
pub fn write_oci_archive(
    store: &ImageStore,
    image: &Image,
    ref_name: &str,
    dest: &Path,
) -> Result<()> {
    let (manifest_bytes, manifest_digest) = build_manifest(store, image)?;

    let file = std::fs::File::create(dest)?;
    let mut tar = tar::Builder::new(file);

    // `oci-layout` marker — an importer that does not find it treats the tar as
    // the legacy `docker save` format and looks for a `manifest.json` we do not write.
    append_file(&mut tar, "oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": crate::registry::DOCKER_MANIFEST_MEDIA_TYPE,
            "digest": manifest_digest,
            "size": manifest_bytes.len(),
            "annotations": annotations(ref_name),
        }],
    });
    append_file(&mut tar, "index.json", &serde_json::to_vec(&index)?)?;

    append_blob(&mut tar, &manifest_digest, &manifest_bytes)?;
    let config = store.cas().read(&image.id)?;
    append_blob(&mut tar, &image.id, &config)?;
    for digest in &image.layers {
        let data = store.cas().read(digest)?;
        append_blob(&mut tar, digest, &data)?;
    }

    tar.finish()?;
    Ok(())
}

/// The name annotations, as a map — PURE, so the (easy to get wrong, silent when
/// wrong) key names are covered by a test instead of only by a live import.
pub(crate) fn annotations(ref_name: &str) -> HashMap<String, String> {
    HashMap::from([
        (CONTAINERD_IMAGE_NAME.to_string(), ref_name.to_string()),
        (OCI_REF_NAME.to_string(), ref_name.to_string()),
    ])
}

/// `blobs/sha256/<hex>` for a `sha256:<hex>` digest — the layout's blob path.
pub(crate) fn blob_path(digest: &str) -> String {
    format!("blobs/sha256/{}", strip(digest))
}

fn append_blob<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    digest: &str,
    data: &[u8],
) -> Result<()> {
    append_file(tar, &blob_path(digest), data)
}

fn append_file<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    // Fixed mtime: two archives of the SAME image are then byte-identical, which
    // keeps an import idempotent to inspect and a `sha256sum` of the artifact
    // meaningful. Nothing downstream reads it.
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, path, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_path_usa_o_hex_sem_o_prefixo_do_algoritmo() {
        assert_eq!(
            blob_path("sha256:abc123"),
            "blobs/sha256/abc123",
            "the layout addresses blobs by hex; leaving `sha256:` in the path \
             produces a tar containerd silently cannot resolve"
        );
        // Already-stripped input must not be mangled either.
        assert_eq!(blob_path("abc123"), "blobs/sha256/abc123");
    }

    #[test]
    fn annotations_incluem_a_chave_que_o_containerd_le() {
        let a = annotations("delonix-web:v1.2.3");
        // The containerd-specific key is the one that NAMES the image on import;
        // dropping it makes the import succeed and the pod still fail to pull.
        assert_eq!(a.get(CONTAINERD_IMAGE_NAME).unwrap(), "delonix-web:v1.2.3");
        assert_eq!(a.get(OCI_REF_NAME).unwrap(), "delonix-web:v1.2.3");
    }
}

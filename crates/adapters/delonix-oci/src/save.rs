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
use crate::registry::{build_manifest, build_oci_manifest};
use crate::Result;
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
    write_archive(
        store,
        image,
        ref_name,
        dest,
        crate::registry::DOCKER_MANIFEST_MEDIA_TYPE,
        &manifest_bytes,
        &manifest_digest,
    )
}

/// What an archive written by [`write_oci_media_archive`] is, by digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciArchive {
    /// The image id (the config digest). The conversion does not touch the
    /// config, so this is the identity of the image whatever its manifest says.
    pub image_id: String,
    /// The digest of the manifest inside THIS archive, with OCI media types. It
    /// differs from the one a registry serves for a Docker v2 image, and names
    /// the file that was produced rather than the image.
    pub manifest_digest: String,
}

/// [`write_oci_archive`] with a manifest that carries **OCI media types**
/// (`application/vnd.oci.image.manifest.v1+json`, OCI config, OCI layers).
///
/// The config and the layers are the store's blobs, byte for byte; only the
/// manifest is rewritten. It exists for consumers that read nothing else: a
/// Proxmox VE node refuses the Docker v2 archive and accepts this one
/// (ADR-0058). `delonix image save` keeps [`write_oci_archive`], which `docker
/// load` and `ctr import` read, and nothing measured asks it to change.
///
/// A layer with no plain OCI equivalent (zstd, foreign) is refused, see
/// [`crate::registry::build_oci_manifest`].
pub fn write_oci_media_archive(
    store: &ImageStore,
    image: &Image,
    ref_name: &str,
    dest: &Path,
) -> Result<OciArchive> {
    let (manifest_bytes, manifest_digest) = build_oci_manifest(store, image)?;
    write_archive(
        store,
        image,
        ref_name,
        dest,
        OCI_MANIFEST_MEDIA_TYPE,
        &manifest_bytes,
        &manifest_digest,
    )?;
    Ok(OciArchive {
        image_id: image.id.clone(),
        manifest_digest,
    })
}

const OCI_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// The archive itself: the layout, the legacy `manifest.json`, and the blobs,
/// with `manifest_bytes` as the manifest the index points at.
fn write_archive(
    store: &ImageStore,
    image: &Image,
    ref_name: &str,
    dest: &Path,
    manifest_media_type: &str,
    manifest_bytes: &[u8],
    manifest_digest: &str,
) -> Result<()> {
    let file = std::fs::File::create(dest)?;
    let mut tar = tar::Builder::new(file);

    // `oci-layout` marker — an importer that does not find it treats the tar as
    // the legacy `docker save` format and looks for a `manifest.json` we do not write.
    append_file(&mut tar, "oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": manifest_media_type,
            "digest": manifest_digest,
            "size": manifest_bytes.len(),
            "annotations": annotations(ref_name),
        }],
    });
    append_file(&mut tar, "index.json", &serde_json::to_vec(&index)?)?;

    // Legacy `manifest.json`, exactly as `docker save` still emits alongside the
    // OCI layout. It is what makes ONE archive readable by every consumer that
    // matters here: `ctr images import` and `podman load` read the OCI layout,
    // while `docker load` and our own [`crate::load::load_docker_archive`] read
    // this. Writing only the layout would make `delonix image save` |
    // `delonix image load` fail to round-trip through our own loader.
    let legacy = serde_json::json!([{
        "Config": blob_path(&image.id),
        "RepoTags": [ref_name],
        "Layers": image.layers.iter().map(|d| blob_path(d)).collect::<Vec<_>>(),
    }]);
    append_file(&mut tar, "manifest.json", &serde_json::to_vec(&legacy)?)?;

    append_blob(&mut tar, manifest_digest, manifest_bytes)?;
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

    /// A store holding one image whose layer starts with the gzip magic (the
    /// media type is read from the first bytes; nothing decompresses it here).
    fn store_with_one_image(root: &Path) -> (ImageStore, Image, Vec<u8>, Vec<u8>) {
        let store = ImageStore::open(root).unwrap();
        let config =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#
                .to_vec();
        let layer = [&[0x1f, 0x8b][..], b"not really gzip, only its magic"].concat();
        let id = store.cas().write(&config).unwrap();
        let layer_digest = store.cas().write(&layer).unwrap();
        let image = Image {
            id,
            repo_tags: vec!["img:1".into()],
            layers: vec![layer_digest],
            config: Default::default(),
            created_unix: 0,
        };
        (store, image, config, layer)
    }

    fn read_tar(path: &Path) -> HashMap<String, Vec<u8>> {
        let mut out = HashMap::new();
        let mut ar = tar::Archive::new(std::fs::File::open(path).unwrap());
        for entry in ar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut data).unwrap();
            out.insert(name, data);
        }
        out
    }

    #[test]
    fn the_oci_media_archive_rewrites_only_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, image, config, layer) = store_with_one_image(&tmp.path().join("store"));
        let dest = tmp.path().join("img.tar");

        let archive = write_oci_media_archive(&store, &image, "img:1", &dest).unwrap();
        let files = read_tar(&dest);

        let index: serde_json::Value = serde_json::from_slice(&files["index.json"]).unwrap();
        let entry = &index["manifests"][0];
        assert_eq!(
            entry["mediaType"],
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(entry["digest"], archive.manifest_digest.as_str());

        let manifest_bytes = &files[&blob_path(&archive.manifest_digest)];
        assert_eq!(
            format!("sha256:{}", crate::cas::sha256_hex(manifest_bytes)),
            archive.manifest_digest,
            "the recorded digest is the digest of the manifest in the file"
        );
        let manifest: serde_json::Value = serde_json::from_slice(manifest_bytes).unwrap();
        assert_eq!(
            manifest["mediaType"],
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(
            manifest["config"]["mediaType"],
            "application/vnd.oci.image.config.v1+json"
        );
        assert_eq!(
            manifest["layers"][0]["mediaType"],
            "application/vnd.oci.image.layer.v1.tar+gzip"
        );

        // The blobs are the store's, byte for byte, and the image id is unchanged.
        assert_eq!(archive.image_id, image.id);
        assert_eq!(files[&blob_path(&image.id)], config);
        assert_eq!(files[&blob_path(&image.layers[0])], layer);
    }

    #[test]
    fn image_save_keeps_the_docker_v2_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, image, _, _) = store_with_one_image(&tmp.path().join("store"));
        let docker = tmp.path().join("docker.tar");
        let oci = tmp.path().join("oci.tar");
        write_oci_archive(&store, &image, "img:1", &docker).unwrap();
        let archive = write_oci_media_archive(&store, &image, "img:1", &oci).unwrap();

        let files = read_tar(&docker);
        let index: serde_json::Value = serde_json::from_slice(&files["index.json"]).unwrap();
        assert_eq!(
            index["manifests"][0]["mediaType"],
            "application/vnd.docker.distribution.manifest.v2+json"
        );
        // Same image, two manifests: the digests must differ, or one of the two
        // functions is not writing what it says.
        assert_ne!(
            index["manifests"][0]["digest"],
            archive.manifest_digest.as_str()
        );
    }

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

//! Loading images from a `docker save` archive (OCI/legacy format).

use crate::cas::sha256_hex;
use crate::image::{now_unix, Image, ImageConfig, ImageStore};
use delonix_runtime_core::{Error, Result};
use oci_spec::image::ImageConfiguration;
use serde::Deserialize;
use std::io::Read;
use std::path::Path;

/// An entry of the legacy `manifest.json` from `docker save`. It is NOT an OCI type —
/// the `manifest.json` of `docker save` is a legacy Docker format (array of
/// `{Config, RepoTags, Layers}` with `blobs/sha256/...` paths), which
/// `oci-spec` does not model; hence it is hand-rolled on purpose. The config
/// blob it points to is already an OCI `ImageConfiguration` (see below).
#[derive(Deserialize)]
struct DockerManifest {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// Converts a `blobs/sha256/<hex>` path into the `sha256:<hex>` digest.
fn path_to_digest(blob_path: &str) -> String {
    let hex = blob_path.rsplit('/').next().unwrap_or(blob_path);
    format!("sha256:{hex}")
}

/// Imports a `docker save` archive into the store, returning the image.
pub fn load_docker_archive(store: &ImageStore, tar_path: &Path) -> Result<Image> {
    let file = std::fs::File::open(tar_path)?;
    let mut archive = tar::Archive::new(file);
    let mut manifest_bytes: Option<Vec<u8>> = None;

    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry.path()?.to_string_lossy().into_owned();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;

        if name == "manifest.json" {
            manifest_bytes = Some(buf);
        } else if name.starts_with("blobs/sha256/") && name.len() > "blobs/sha256/".len() {
            let digest = store.cas().write(&buf)?;
            let expected = path_to_digest(&name);
            if digest != expected {
                return Err(Error::Invalid(format!(
                    "corrupted blob: {name} has sha256 {digest}"
                )));
            }
        }
    }

    let manifest_bytes = manifest_bytes
        .ok_or_else(|| Error::Invalid("manifest.json missing from the archive".into()))?;
    let manifests: Vec<DockerManifest> = serde_json::from_slice(&manifest_bytes)?;
    let manifest = manifests
        .into_iter()
        .next()
        .ok_or_else(|| Error::Invalid("empty manifest.json".into()))?;

    let config_digest = path_to_digest(&manifest.config);
    let config_bytes = store.cas().read(&config_digest)?;
    if sha256_hex(&config_bytes) != crate::cas::strip(&config_digest) {
        return Err(Error::Invalid("config digest mismatch".into()));
    }
    // Read the runtime config from the OCI config blob (`ImageConfiguration`).
    let oci_config: ImageConfiguration = serde_json::from_slice(&config_bytes)?;
    let inner = oci_config.config().clone().unwrap_or_default();

    let image = Image {
        id: config_digest,
        repo_tags: manifest.repo_tags.unwrap_or_default(),
        layers: manifest.layers.iter().map(|l| path_to_digest(l)).collect(),
        config: ImageConfig {
            cmd: inner.cmd().clone().unwrap_or_default(),
            entrypoint: inner.entrypoint().clone().unwrap_or_default(),
            env: inner.env().clone().unwrap_or_default(),
            user: inner.user().clone().unwrap_or_default(),
            working_dir: inner.working_dir().clone().unwrap_or_default(),
            cpus: None,
            memory: None,
            security: Vec::new(),
            healthcheck: None,
        },
        created_unix: now_unix(),
    };
    store.enforce_tag_uniqueness(&image)?;
    store.save(&image)?;
    Ok(image)
}

//! Carregar imagens a partir de um arquivo `docker save` (formato OCI/legacy).

use crate::cas::sha256_hex;
use crate::image::{now_unix, Image, ImageConfig, ImageStore};
use delonix_runtime_core::{Error, Result};
use oci_spec::image::ImageConfiguration;
use serde::Deserialize;
use std::io::Read;
use std::path::Path;

/// Uma entrada do `manifest.json` legacy do `docker save`. NÃO é um tipo OCI —
/// o `manifest.json` do `docker save` é um formato Docker legacy (array de
/// `{Config, RepoTags, Layers}` com caminhos `blobs/sha256/...`), que o
/// `oci-spec` não modela; por isso fica hand-rolled de propósito. O blob de
/// config apontado por ele já é um `ImageConfiguration` OCI (ver abaixo).
#[derive(Deserialize)]
struct DockerManifest {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// Converte um caminho `blobs/sha256/<hex>` no digest `sha256:<hex>`.
fn path_to_digest(blob_path: &str) -> String {
    let hex = blob_path.rsplit('/').next().unwrap_or(blob_path);
    format!("sha256:{hex}")
}

/// Importa um arquivo `docker save` para o armazém, devolvendo a imagem.
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
    // Lê a config de execução do blob de config OCI (`ImageConfiguration`).
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

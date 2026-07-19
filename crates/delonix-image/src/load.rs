//! Carregar imagens a partir de um arquivo `docker save` (formato OCI/legacy).

use crate::cas::sha256_hex;
use crate::image::{now_unix, Image, ImageConfig, ImageStore};
use delonix_runtime_core::{Error, Result};
use serde::Deserialize;
use std::io::Read;
use std::path::Path;

/// Uma entrada do `manifest.json` legacy do `docker save`.
#[derive(Deserialize)]
struct DockerManifest {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// O blob de config OCI (só os campos que usamos).
#[derive(Deserialize)]
struct RawConfig {
    config: Option<RawConfigInner>,
}

#[derive(Deserialize)]
struct RawConfigInner {
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "User")]
    user: Option<String>,
    #[serde(rename = "WorkingDir")]
    working_dir: Option<String>,
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
                    "blob corrompido: {name} tem sha256 {digest}"
                )));
            }
        }
    }

    let manifest_bytes =
        manifest_bytes.ok_or_else(|| Error::Invalid("manifest.json em falta no arquivo".into()))?;
    let manifests: Vec<DockerManifest> = serde_json::from_slice(&manifest_bytes)?;
    let manifest = manifests
        .into_iter()
        .next()
        .ok_or_else(|| Error::Invalid("manifest.json vazio".into()))?;

    let config_digest = path_to_digest(&manifest.config);
    let config_bytes = store.cas().read(&config_digest)?;
    if sha256_hex(&config_bytes) != crate::cas::strip(&config_digest) {
        return Err(Error::Invalid("digest do config não confere".into()));
    }
    let raw: RawConfig = serde_json::from_slice(&config_bytes)?;
    let inner = raw.config.unwrap_or(RawConfigInner {
        cmd: None,
        entrypoint: None,
        env: None,
        user: None,
        working_dir: None,
    });

    let image = Image {
        id: config_digest,
        repo_tags: manifest.repo_tags.unwrap_or_default(),
        layers: manifest.layers.iter().map(|l| path_to_digest(l)).collect(),
        config: ImageConfig {
            cmd: inner.cmd.unwrap_or_default(),
            entrypoint: inner.entrypoint.unwrap_or_default(),
            env: inner.env.unwrap_or_default(),
            user: inner.user.unwrap_or_default(),
            working_dir: inner.working_dir.unwrap_or_default(),
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

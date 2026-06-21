//! `delonix-volume` — volumes nomeados e *bind mounts* do Delonix Engine.
//!
//! Dois tipos de montagem, ambos **zero-copy** (o kernel partilha os blocos via
//! `MS_BIND`, não há cópia de dados):
//! - **volume nomeado**: um directório gerido pelo Delonix em
//!   `<root>/volumes/<nome>/_data`, que **sobrevive** ao container;
//! - **bind mount**: um caminho arbitrário do host, montado no container.
//!
//! A sintaxe `-v` segue o Docker: `nome:/destino` (volume) ou
//! `/caminho/host:/destino` (bind), com `:ro` opcional para só-leitura.

use delonix_core::{Error, Mount, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Metadados de um volume nomeado.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Volume {
    /// O nome do volume.
    pub name: String,
    /// O directório de dados no host (`.../_data`).
    pub mountpoint: String,
    /// Instante de criação (segundos Unix).
    pub created_unix: u64,
    /// Driver: `local` (omissão) ou `nfs` (TrueNAS/NFS externo).
    #[serde(default = "default_driver")]
    pub driver: String,
    /// Para `nfs`: o *export* (`servidor:/caminho`).
    #[serde(default)]
    pub device: Option<String>,
    /// Opções de montagem (`mount -o ...`), ex.: `vers=4,ro`.
    #[serde(default)]
    pub options: Option<String>,
}

fn default_driver() -> String {
    "local".to_string()
}

/// O armazém de volumes, sob `<root>/volumes`.
pub struct VolumeStore {
    root: PathBuf,
}

impl VolumeStore {
    /// Abre (criando) o armazém de volumes.
    pub fn open(base: impl Into<PathBuf>) -> Result<Self> {
        let root = base.into().join("volumes");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
    fn data_dir(&self, name: &str) -> PathBuf {
        self.dir(name).join("_data")
    }
    fn meta_path(&self, name: &str) -> PathBuf {
        self.dir(name).join("meta.json")
    }

    fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    }

    /// Cria um volume `local` (idempotente: devolve o existente se já existir).
    pub fn create(&self, name: &str) -> Result<Volume> {
        if self.meta_path(name).exists() {
            return self.inspect(name); // preserva o driver/device de um volume já criado
        }
        self.create_with(name, "local", None, None)
    }

    /// Cria um volume com um driver (`local`/`nfs`). Para `nfs`, monta já o
    /// *export* (`servidor:/caminho`) no directório de dados — útil para ligar a
    /// um TrueNAS ou outro servidor NFS. Idempotente.
    pub fn create_with(
        &self,
        name: &str,
        driver: &str,
        device: Option<String>,
        options: Option<String>,
    ) -> Result<Volume> {
        if !Self::valid_name(name) {
            return Err(Error::Invalid(format!("nome de volume inválido: {name:?}")));
        }
        if self.meta_path(name).exists() {
            let v = self.inspect(name)?;
            self.ensure_mounted(&v)?;
            return Ok(v);
        }
        if driver == "nfs" && device.as_deref().unwrap_or("").is_empty() {
            return Err(Error::Invalid("volume nfs requer um device (servidor:/export)".into()));
        }
        let data = self.data_dir(name);
        fs::create_dir_all(&data)?;
        let vol = Volume {
            name: name.to_string(),
            mountpoint: data.to_string_lossy().into_owned(),
            created_unix: now_unix(),
            driver: driver.to_string(),
            device,
            options,
        };
        // Monta ANTES de persistir: se o NFS falhar, não deixamos um volume órfão.
        if let Err(e) = self.ensure_mounted(&vol) {
            let _ = fs::remove_dir_all(self.dir(name));
            return Err(e);
        }
        fs::write(self.meta_path(name), serde_json::to_vec_pretty(&vol)?)?;
        Ok(vol)
    }

    /// Garante que um volume `nfs` está montado (via `mount -t nfs`). No-op para
    /// volumes locais ou se já estiver montado. Best-effort: requer `mount.nfs`.
    pub fn ensure_mounted(&self, vol: &Volume) -> Result<()> {
        if vol.driver != "nfs" || is_mounted(&vol.mountpoint) {
            return Ok(());
        }
        let device = vol
            .device
            .as_ref()
            .ok_or_else(|| Error::Invalid(format!("volume nfs '{}' sem device", vol.name)))?;
        let mut args = vec!["-t", "nfs", device.as_str(), vol.mountpoint.as_str()];
        if let Some(o) = &vol.options {
            args.push("-o");
            args.push(o);
        }
        let out = std::process::Command::new("mount")
            .args(&args)
            .output()
            .map_err(|e| Error::Runtime { context: "mount nfs", message: e.to_string() })?;
        if !out.status.success() {
            return Err(Error::Runtime {
                context: "mount nfs",
                message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    /// Lista os volumes existentes.
    pub fn list(&self) -> Result<Vec<Volume>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let meta = path.join("meta.json");
            if meta.exists() {
                if let Ok(bytes) = fs::read(&meta) {
                    if let Ok(v) = serde_json::from_slice::<Volume>(&bytes) {
                        out.push(v);
                    }
                }
            }
        }
        out.sort_by_key(|v| std::cmp::Reverse(v.created_unix));
        Ok(out)
    }

    /// Inspecciona um volume pelo nome.
    pub fn inspect(&self, name: &str) -> Result<Volume> {
        let meta = self.meta_path(name);
        if !meta.exists() {
            return Err(Error::NotFound(format!("volume {name}")));
        }
        Ok(serde_json::from_slice(&fs::read(meta)?)?)
    }

    /// Remove um volume (e os seus dados). Desmonta primeiro se for `nfs`.
    pub fn remove(&self, name: &str) -> Result<()> {
        let dir = self.dir(name);
        if !dir.exists() {
            return Err(Error::NotFound(format!("volume {name}")));
        }
        if let Ok(v) = self.inspect(name) {
            if v.driver == "nfs" && is_mounted(&v.mountpoint) {
                let _ = std::process::Command::new("umount").arg(&v.mountpoint).output();
            }
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Traduz uma especificação `-v` num [`Mount`].
    ///
    /// - `nome:/destino[:ro]` → volume nomeado (criado se não existir);
    /// - `/host:/destino[:ro]` (ou `./rel`) → *bind mount* de um caminho do host.
    pub fn resolve_spec(&self, spec: &str) -> Result<Mount> {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return Err(Error::Invalid(format!(
                "spec de volume inválida: {spec:?} (usa origem:/destino[:ro])"
            )));
        }
        let src = parts[0];
        let target = parts[1];
        let readonly = parts.get(2).map(|o| *o == "ro").unwrap_or(false);
        if !target.starts_with('/') {
            return Err(Error::Invalid(format!("destino deve ser absoluto: {target:?}")));
        }

        let source = if src.starts_with('/') || src.starts_with('.') {
            // bind mount de um caminho do host
            let p = fs::canonicalize(src)
                .map_err(|_| Error::Invalid(format!("caminho de bind inexistente: {src}")))?;
            p.to_string_lossy().into_owned()
        } else {
            // volume nomeado (cria a pedido, como o Docker; monta o NFS se for o caso)
            let vol = self.create(src)?;
            self.ensure_mounted(&vol)?;
            vol.mountpoint
        };

        Ok(Mount {
            source,
            target: target.to_string(),
            readonly,
        })
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `true` se `path` é um ponto de montagem activo (consulta `/proc/mounts`).
fn is_mounted(path: &str) -> bool {
    fs::read_to_string("/proc/mounts")
        .map(|s| s.lines().any(|l| l.split_whitespace().nth(1) == Some(path)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (VolumeStore, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "delonix-vol-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (VolumeStore::open(&base).unwrap(), base)
    }

    #[test]
    fn create_list_inspect_remove() {
        let (vs, base) = store();
        let v = vs.create("data").unwrap();
        assert!(v.mountpoint.ends_with("/data/_data"));
        assert_eq!(vs.list().unwrap().len(), 1);
        assert_eq!(vs.inspect("data").unwrap().name, "data");
        vs.remove("data").unwrap();
        assert!(vs.inspect("data").is_err());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_named_volume_creates_it() {
        let (vs, base) = store();
        let m = vs.resolve_spec("cache:/var/cache").unwrap();
        assert!(m.source.ends_with("/cache/_data"));
        assert_eq!(m.target, "/var/cache");
        assert!(!m.readonly);
        assert_eq!(vs.inspect("cache").unwrap().name, "cache");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_bind_readonly() {
        let (vs, base) = store();
        let host = base.join("hostdir");
        fs::create_dir_all(&host).unwrap();
        let spec = format!("{}:/mnt:ro", host.display());
        let m = vs.resolve_spec(&spec).unwrap();
        assert_eq!(m.target, "/mnt");
        assert!(m.readonly);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn rejects_relative_target_and_bad_spec() {
        let (vs, base) = store();
        assert!(vs.resolve_spec("data:relative").is_err());
        assert!(vs.resolve_spec("oneword").is_err());
        fs::remove_dir_all(&base).ok();
    }
}

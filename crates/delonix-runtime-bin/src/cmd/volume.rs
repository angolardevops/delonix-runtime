//! `delonix volumes` — volumes nomeados (create/ls/rm/inspect).

use std::path::PathBuf;

use clap::Subcommand;
use delonix_runtime_core::Result;
use delonix_volume::{parse_size_bytes, VolumeStore};
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::util::state_root;

/// `spec` de `kind: Volume` — espelha os campos de `VolumeCmd::Create`.
#[derive(Debug, Deserialize)]
struct VolumeSpec {
    #[serde(default = "default_driver")]
    driver: String,
    device: Option<String>,
    options: Option<String>,
    quota: Option<String>,
}

fn default_driver() -> String {
    "local".to_string()
}

#[derive(Subcommand)]
pub enum VolumeCmd {
    /// Cria um volume nomeado.
    Create {
        name: String,
        /// `local` (default) ou `nfs`.
        #[arg(long, default_value = "local")]
        driver: String,
        /// Dispositivo/export (driver `nfs`).
        #[arg(long)]
        device: Option<String>,
        /// Opções de montagem adicionais (driver `nfs`).
        #[arg(long)]
        options: Option<String>,
        /// Quota (ex.: `2g`) — só é aplicada se `--quota` for dado.
        #[arg(long)]
        quota: Option<String>,
    },
    /// Lista os volumes.
    Ls,
    /// Detalhe de um volume (inclui uso real em disco).
    Inspect { name: String },
    /// Remove um volume.
    Rm { name: String },
    /// Aplica os documentos `kind: Volume` de um manifesto (idempotente por nome).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

pub fn run(action: VolumeCmd) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    match action {
        VolumeCmd::Create { name, driver, device, options, quota } => {
            let vol = create_volume(&store, &name, &driver, device, options, quota)?;
            println!("{}", vol.name);
            Ok(())
        }
        VolumeCmd::Ls => cmd_ls(&store),
        VolumeCmd::Inspect { name } => cmd_inspect(&store, &name),
        VolumeCmd::Rm { name } => cmd_rm(&store, &name),
        VolumeCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
    }
}

/// Aplica os documentos `kind: Volume` (`create`/`create_with` já são
/// idempotentes por nome — não precisa de um check de existência à parte).
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    for doc in manifest::of_kind(docs, "Volume") {
        let name = &doc.metadata.name;
        let spec: VolumeSpec = manifest::spec_of(doc)?;
        create_volume(&store, name, &spec.driver, spec.device, spec.options, spec.quota)?;
        println!("volume/{name}: garantido");
    }
    Ok(())
}

fn create_volume(
    store: &VolumeStore,
    name: &str,
    driver: &str,
    device: Option<String>,
    options: Option<String>,
    quota: Option<String>,
) -> Result<delonix_volume::Volume> {
    let vol = if driver == "local" && device.is_none() && options.is_none() {
        store.create(name)?
    } else {
        store.create_with(name, driver, device, options)?
    };
    if let Some(q) = quota {
        let bytes = parse_size_bytes(&q)
            .ok_or_else(|| delonix_runtime_core::Error::Invalid(format!("quota inválida: {q}")))?;
        store.set_quota(name, Some(bytes), None, false)?;
    }
    Ok(vol)
}

fn cmd_ls(store: &VolumeStore) -> Result<()> {
    println!("{:<24}  {:<8}  MOUNTPOINT", "NOME", "DRIVER");
    for v in store.list()? {
        println!("{:<24}  {:<8}  {}", v.name, v.driver, v.mountpoint);
    }
    Ok(())
}

fn cmd_inspect(store: &VolumeStore, name: &str) -> Result<()> {
    let v = store.inspect(name)?;
    let usage = store.usage(name);
    println!("nome:        {}", v.name);
    println!("driver:      {}", v.driver);
    println!("mountpoint:  {}", v.mountpoint);
    println!("criado:      unix={}", v.created_unix);
    println!("uso:         {usage} bytes");
    if let Some(q) = v.quota_bytes {
        println!("quota:       {q} bytes");
    }
    Ok(())
}

fn cmd_rm(store: &VolumeStore, name: &str) -> Result<()> {
    store.remove(name)?;
    println!("{name}");
    Ok(())
}

//! `delonix volumes` — volumes nomeados (create/ls/rm/inspect).

use std::path::PathBuf;

use clap::Subcommand;
use delonix_runtime_core::Result;
use delonix_volume::{parse_size_bytes, VolumeStore};
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::output;
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
    /// Detalhe legível de um ou mais volumes, ao estilo `kubectl describe`
    /// (para humanos; use `inspect` para a vista compacta de sempre).
    Describe {
        #[arg(required = true)]
        names: Vec<String>,
    },
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
        VolumeCmd::Describe { names } => cmd_describe(&store, &names),
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
    let mut t = output::Table::new(&["NAME", "DRIVER", "MOUNTPOINT"]);
    for v in store.list()? {
        t.row(vec![v.name, v.driver, v.mountpoint]);
    }
    t.print();
    Ok(())
}

/// Uso em disco, com o denominador da quota quando existe: `"1.5 KiB"` ou
/// `"1.5 KiB / 2.0 GiB (0%)"`. Função **pura** (o `usage`/`quota_bytes` reais
/// vêm do store) para a aritmética da percentagem ser testável — incluindo a
/// quota 0, que não pode dividir por zero.
fn fmt_usage(used: u64, quota: Option<u64>) -> String {
    match quota {
        Some(q) if q > 0 => {
            let pct = (used as f64 / q as f64 * 100.0).round() as u64;
            format!("{} / {} ({pct}%)", output::fmt_size(used), output::fmt_size(q))
        }
        // Quota 0 = sem espaço nenhum; imprimir "(inf%)" seria pior que só o uso.
        Some(_) => format!("{} / 0 B", output::fmt_size(used)),
        None => output::fmt_size(used),
    }
}

/// `volumes describe` — detalhe legível ao estilo `kubectl describe`.
/// Complementa o `inspect` (vista compacta de sempre, estável para scripts).
fn cmd_describe(store: &VolumeStore, names: &[String]) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        let v = store.inspect(name)?;
        if i > 0 {
            println!();
        }
        describe_one(store, &v);
    }
    Ok(())
}

fn describe_one(store: &VolumeStore, v: &delonix_volume::Volume) {
    let mut d = output::Describe::new();
    d.field("Name", &v.name);
    d.field("Driver", &v.driver);
    d.field("Mountpoint", &v.mountpoint);
    d.field("Created", output::fmt_local(v.created_unix));
    d.field("Age", output::fmt_age(v.created_unix));
    d.field("Usage", fmt_usage(store.usage(&v.name), v.quota_bytes));
    d.field("Quota", v.quota_bytes.map(output::fmt_size).unwrap_or_else(|| "<none>".into()));
    d.field_opt("Alert at", v.alert_pct.map(|p| format!("{p}%")));
    // Só existem no driver `nfs` — omitidos por inteiro no `local`.
    d.field_opt("Device", v.device.as_deref());
    d.field_opt("Options", v.options.as_deref());
    d.print();
}

#[cfg(test)]
mod tests {
    use super::fmt_usage;

    #[test]
    fn usage_sem_quota_mostra_so_o_uso() {
        assert_eq!(fmt_usage(1536, None), "1.5 KiB");
    }

    #[test]
    fn usage_com_quota_mostra_percentagem() {
        assert_eq!(fmt_usage(512 * 1024 * 1024, Some(1024 * 1024 * 1024)), "512.0 MiB / 1.00 GiB (50%)");
    }

    #[test]
    fn usage_com_quota_zero_nao_divide_por_zero() {
        // Uma quota 0 daria `inf%`/NaN na percentagem — degrada para o uso cru.
        assert_eq!(fmt_usage(100, Some(0)), "100 B / 0 B");
    }
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

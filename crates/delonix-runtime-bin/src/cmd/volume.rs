//! `delonix volumes` — named volumes (create/ls/rm/inspect).

use std::path::PathBuf;

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_runtime_core::{Error, Result};
use delonix_volume::{parse_size_bytes, VolumeStore};
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::state_root;

/// `spec` of `kind: Volume` — mirrors the fields of `VolumeCmd::Create`.
#[derive(Debug, Deserialize)]
struct VolumeSpec {
    #[serde(default = "default_driver")]
    driver: String,
    device: Option<String>,
    /// Canonical `mountOptions` (uniform with `kind: Storage`); `options`
    /// is still accepted (backward-compat).
    #[serde(rename = "mountOptions", alias = "options")]
    options: Option<String>,
    quota: Option<String>,
}

fn default_driver() -> String {
    "local".to_string()
}

/// Names accepted in the `kind: Volume` `spec` (canonical + aliases), for the
/// unknown-field warning.
pub(crate) const VOLUME_SPEC_FIELDS: &[&str] =
    &["driver", "device", "mountOptions", "options", "quota"];

#[derive(Subcommand)]
pub enum VolumeCmd {
    /// Create a named volume.
    Create {
        name: String,
        /// `local` (default) or `nfs`.
        #[arg(long, default_value = "local")]
        driver: String,
        /// Device/export (`nfs` driver).
        #[arg(long)]
        device: Option<String>,
        /// Additional mount options (`nfs` driver).
        #[arg(long)]
        options: Option<String>,
        /// Quota (e.g. `2g`) — only applied if `--quota` is given.
        #[arg(long)]
        quota: Option<String>,
    },
    /// List the volumes.
    Ls,
    /// Details of a volume (includes real on-disk usage).
    Inspect {
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        name: String,
    },
    /// Readable detail of one or more volumes, `kubectl describe` style
    /// (for humans; use `inspect` for the usual compact view).
    Describe {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Remove a volume.
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        name: String,
    },
    /// Apply the `kind: Volume` documents from a manifest (idempotent by name).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Point-in-time snapshots of a volume (tar.gz under the volume; safe in rootless).
    Snapshot {
        #[command(subcommand)]
        action: SnapshotCmd,
    },
}

/// `delonix volumes snapshot` — crash-consistent (taken with the workload
/// running). For application consistency (e.g. a DB), stop/dump the consumer
/// first. In rootless the tar runs in a mapped userns (effective owner of the
/// subuid files) — see `runtime::reexec_mapped`/`__volsnap`.
#[derive(clap::Subcommand)]
pub enum SnapshotCmd {
    /// Create a snapshot NOW (default name: UTC timestamp).
    Create {
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        volume: String,
        /// Snapshot name (default: `YYYYMMDD-HHMMSS`).
        #[arg(long)]
        name: Option<String>,
    },
    /// List the snapshots of a volume.
    Ls {
        /// Volume to query (omit for the snapshots of ALL).
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        volume: Option<String>,
    },
    /// Restore a snapshot INTO the volume (replaces the current data — stop the
    /// consumers first).
    Restore {
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        volume: String,
        /// Snapshot name (see `snapshot ls`).
        snap: String,
    },
    /// Delete a snapshot.
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::volumes))]
        volume: String,
        snap: String,
    },
}

pub fn run(action: VolumeCmd) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    match action {
        VolumeCmd::Create {
            name,
            driver,
            device,
            options,
            quota,
        } => {
            let vol = create_volume(&store, &name, &driver, device, options, quota)?;
            println!("{}", vol.name);
            Ok(())
        }
        VolumeCmd::Ls => cmd_ls(&store),
        VolumeCmd::Inspect { name } => cmd_inspect(&store, &name),
        VolumeCmd::Describe { names } => cmd_describe(&store, &names),
        VolumeCmd::Rm { name } => cmd_rm(&store, &name),
        VolumeCmd::Snapshot { action } => cmd_snapshot(&store, action),
        VolumeCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
    }
}

/// Applies the `kind: Volume` documents (`create`/`create_with` are already
/// idempotent by name — no separate existence check needed).
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let store = VolumeStore::open(state_root())?;
    for doc in manifest::of_kind(docs, "Volume") {
        let name = &doc.metadata.name;
        manifest::warn_unknown_fields(doc, VOLUME_SPEC_FIELDS);
        let spec: VolumeSpec = manifest::spec_of(doc)?;
        create_volume(
            &store,
            name,
            &spec.driver,
            spec.device,
            spec.options,
            spec.quota,
        )?;
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

/// On-disk usage, with the quota denominator when present: `"1.5 KiB"` or
/// `"1.5 KiB / 2.0 GiB (0%)"`. **Pure** function (the real `usage`/`quota_bytes`
/// come from the store) so the percentage arithmetic is testable — including
/// quota 0, which cannot divide by zero.
fn fmt_usage(used: u64, quota: Option<u64>) -> String {
    match quota {
        Some(q) if q > 0 => {
            let pct = (used as f64 / q as f64 * 100.0).round() as u64;
            format!(
                "{} / {} ({pct}%)",
                output::fmt_size(used),
                output::fmt_size(q)
            )
        }
        // Quota 0 = no space at all; printing "(inf%)" would be worse than just usage.
        Some(_) => format!("{} / 0 B", output::fmt_size(used)),
        None => output::fmt_size(used),
    }
}

/// `volumes describe` — readable detail in `kubectl describe` style.
/// Complements `inspect` (the usual compact view, stable for scripts).
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
    d.field(
        "Quota",
        v.quota_bytes
            .map(output::fmt_size)
            .unwrap_or_else(|| "<none>".into()),
    );
    d.field_opt("Alert at", v.alert_pct.map(|p| format!("{p}%")));
    // Only exist in the `nfs` driver — omitted entirely for `local`.
    d.field_opt("Device", v.device.as_deref());
    d.field_opt("Options", v.options.as_deref());
    d.print();
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

/// Default snapshot name: UTC timestamp `YYYYMMDD-HHMMSS` (no `chrono` — the
/// runtime does not bring it in; uses `libc::gmtime_r`).
fn default_snap_name() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` is valid; `gmtime_r` writes into `tm` (our buffer).
    unsafe { libc::gmtime_r(&t, &mut tm) };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Runs a snapshot operation via the right path: rootless → re-exec
/// `__volsnap` in a mapped userns (owner of the subuids); rootful/no-helpers →
/// direct. The `__volsnap` handler lives in `cmd::mapped` (see the note there on
/// the re-exec contract that the public engine was missing).
fn volsnap_run(mode: &str, data: &std::path::Path, tarball: &std::path::Path) -> Result<()> {
    let d = data.to_string_lossy().to_string();
    let t = tarball.to_string_lossy().to_string();
    match delonix_runtime::reexec_mapped(&["__volsnap", mode, &d, &t]) {
        Some(true) => Ok(()),
        Some(false) => Err(Error::Runtime {
            context: "volume snapshot",
            message: format!("__volsnap {mode} falhou no userns mapeado (vê /etc/subuid)"),
        }),
        // No rootless/helpers: run direct (already owner of the files).
        None => super::mapped::volsnap(mode, data, tarball),
    }
}

fn cmd_snapshot(store: &VolumeStore, action: SnapshotCmd) -> Result<()> {
    match action {
        SnapshotCmd::Create { volume, name } => {
            let v = store.inspect(&volume)?;
            let snap = name.unwrap_or_else(default_snap_name);
            let tarball = store.snapshot_path(&volume, &snap)?;
            if tarball.exists() {
                return Err(Error::Invalid(super::po::tf(
                    "snapshot '{snap}' already exists",
                    &[("snap", &snap)],
                )));
            }
            volsnap_run("create", std::path::Path::new(&v.mountpoint), &tarball)?;
            let size = std::fs::metadata(&tarball).map(|m| m.len()).unwrap_or(0);
            println!(
                "snapshot '{snap}' do volume '{volume}' criado ({})",
                super::output::fmt_size(size)
            );
            println!(
                "{}",
                super::output::dim(
                    "(crash-consistente: para consistência de BD, pára/dump o consumidor primeiro)"
                )
            );
        }
        SnapshotCmd::Ls { volume } => {
            // No argument: snapshots of ALL volumes, with a VOLUME column.
            let vols: Vec<String> = match volume {
                Some(v) => {
                    store.inspect(&v)?; // validates that the volume exists
                    vec![v]
                }
                None => store.list()?.into_iter().map(|v| v.name).collect(),
            };
            let mut t = super::output::Table::new(&["VOLUME", "SNAPSHOT", "SIZE", "CREATED"])
                .right_align(2);
            for v in vols {
                for (n, size, ts) in store.list_snapshots(&v)? {
                    t.row(vec![
                        v.clone(),
                        n,
                        super::output::fmt_size(size),
                        super::output::fmt_local(ts.max(0) as u64),
                    ]);
                }
            }
            t.print();
        }
        SnapshotCmd::Restore { volume, snap } => {
            let v = store.inspect(&volume)?;
            let tarball = store.snapshot_path(&volume, &snap)?;
            if !tarball.exists() {
                return Err(Error::NotFound(format!(
                    "snapshot {snap} do volume {volume}"
                )));
            }
            super::output::warn(&format!(
                "a repor '{volume}' a partir de '{snap}' — pára os consumidores do volume primeiro"
            ));
            volsnap_run("restore", std::path::Path::new(&v.mountpoint), &tarball)?;
            println!("volume '{volume}' reposto do snapshot '{snap}'");
        }
        SnapshotCmd::Rm { volume, snap } => {
            store.remove_snapshot(&volume, &snap)?;
            println!("snapshot '{snap}' do volume '{volume}' apagado");
        }
    }
    Ok(())
}

fn cmd_rm(store: &VolumeStore, name: &str) -> Result<()> {
    store.remove(name)?;
    println!("{name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{fmt_usage, VolumeSpec};

    #[test]
    fn volumespec_aceita_options_legado_e_mountoptions_canonico() {
        let legado: VolumeSpec = serde_yaml::from_str("driver: nfs\noptions: vers=4,ro\n").unwrap();
        assert_eq!(legado.options.as_deref(), Some("vers=4,ro"));
        let canon: VolumeSpec =
            serde_yaml::from_str("driver: nfs\nmountOptions: vers=4,ro\n").unwrap();
        assert_eq!(canon.options.as_deref(), Some("vers=4,ro"));
    }

    #[test]
    fn usage_sem_quota_mostra_so_o_uso() {
        assert_eq!(fmt_usage(1536, None), "1.5 KiB");
    }

    #[test]
    fn usage_com_quota_mostra_percentagem() {
        assert_eq!(
            fmt_usage(512 * 1024 * 1024, Some(1024 * 1024 * 1024)),
            "512.0 MiB / 1.00 GiB (50%)"
        );
    }

    #[test]
    fn usage_com_quota_zero_nao_divide_por_zero() {
        // A quota of 0 would give `inf%`/NaN in the percentage — degrades to raw usage.
        assert_eq!(fmt_usage(100, Some(0)), "100 B / 0 B");
    }
}

//! `delonix sharevolume` (`kind: ShareVolume`) — carves an ISOLATED,
//! individually-quota'd subdirectory out of an already-mounted `kind:
//! Storage` (NFS/CIFS/WebDAV — see `cmd::storage`), so N containers/VMs/pods
//! can share ONE NAS export without seeing each other's data or exhausting
//! each other's quota.
//!
//! **Mechanism (deliberately no new mount machinery)**: `spec.storageRef`
//! names an existing `kind: Storage` (== a `delonix-volume` volume with a
//! network driver, already mounted once at `<root>/volumes/<storageRef>/_data`
//! by `cmd::storage`). A `ShareVolume` is just a REAL subdirectory of that
//! same tree (`<storage-mountpoint>/shares/<name>`), registered as its OWN
//! named `delonix-volume` volume via `VolumeStore::register_external` — a
//! volume whose `mountpoint` points OUTSIDE the store's usual `_data`
//! convention. Two consequences fall out for free, with zero new code:
//! - **Isolation** is plain path confinement: a container that bind-mounts
//!   `-v <sharevolume>:/data` only ever sees ITS subdirectory — it cannot
//!   reach a sibling's without traversing `..`, which no mount here allows.
//! - **Consumption needs nothing new**: `container run -v <name>:/target`
//!   (and the `Vm`/`Pod` equivalents) already resolve a named volume purely
//!   by reading its `Volume.mountpoint` (`VolumeStore::resolve_spec`) — a
//!   `ShareVolume`-registered volume is indistinguishable to that code from
//!   any other named volume.
//!
//! **Quota is SOFT only** (measured usage + alert threshold, via
//! `VolumeStore::usage_at`/`quota_state_at`) — the HARD quota path
//! (`delonix-volume`'s ext4-loopback-image) needs local block storage and
//! doesn't compose with a subdirectory of an NFS/CIFS/WebDAV mount; this is
//! stated up front rather than silently downgraded.
//!
//! `rm` is non-destructive by default: `VolumeStore::remove` only ever
//! deletes ITS OWN per-name bookkeeping directory, never an external
//! `mountpoint` — removing a `ShareVolume` un-registers it but the actual
//! shared data (a subdirectory of the parent Storage) survives unless
//! `--purge-data` is passed explicitly.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use delonix_runtime_core::{Error, JsonStore, Result};
use delonix_volume::VolumeStore;
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::state_root;

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ShareVolumeSpec {
    /// Name of an existing `kind: Storage` (a network-backed `delonix-volume`).
    #[serde(rename = "storageRef")]
    storage_ref: String,
    /// Human size (`5G`, `500M`, ...). Omit = unlimited (still measured/shown).
    #[serde(default)]
    quota: Option<String>,
    /// Usage percentage above which `ls`/`describe` flag a WARN (default 90).
    #[serde(default, rename = "alertPct")]
    alert_pct: Option<u8>,
}

pub const SHAREVOLUME_SPEC_FIELDS: &[&str] = &["storageRef", "quota", "alertPct"];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShareRecord {
    name: String,
    storage_ref: String,
    mountpoint: String,
    quota_bytes: Option<u64>,
    alert_pct: Option<u8>,
    created_unix: u64,
}

#[derive(Subcommand)]
pub enum ShareVolumeCmd {
    /// Apply the `kind: ShareVolume` documents of a manifest (idempotent).
    Apply {
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// List share volumes (parent storage, quota, live usage).
    Ls {
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: output::OutputFormat,
    },
    /// Human-readable detail of one share volume.
    Describe { name: String },
    /// Un-register a share volume. The underlying data (a subdirectory of
    /// the parent Storage) is PRESERVED unless `--purge-data` is passed.
    Rm {
        name: String,
        #[arg(long = "purge-data")]
        purge_data: bool,
    },
}

pub fn run(action: ShareVolumeCmd) -> Result<()> {
    let vstore = VolumeStore::open(state_root())?;
    let sstore = shares_store()?;
    match action {
        ShareVolumeCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply_with(&vstore, &sstore, &docs)
        }
        ShareVolumeCmd::Ls { output } => cmd_ls(&vstore, &sstore, output),
        ShareVolumeCmd::Describe { name } => cmd_describe(&vstore, &sstore, &name),
        ShareVolumeCmd::Rm { name, purge_data } => cmd_rm(&vstore, &sstore, &name, purge_data),
    }
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    apply_with(&VolumeStore::open(state_root())?, &shares_store()?, docs)
}

fn apply_with(
    vstore: &VolumeStore,
    sstore: &JsonStore<ShareRecord>,
    docs: &[ManifestDoc],
) -> Result<()> {
    for doc in manifest::of_kind(docs, "ShareVolume") {
        manifest::warn_unknown_fields(doc, SHAREVOLUME_SPEC_FIELDS);
        let spec: ShareVolumeSpec = manifest::spec_of(doc)?;
        apply_one(vstore, sstore, &doc.metadata.name, &spec)?;
    }
    Ok(())
}

fn shares_store() -> Result<JsonStore<ShareRecord>> {
    JsonStore::open(state_root().join("sharevolumes"))
}

fn apply_one(
    vstore: &VolumeStore,
    store: &JsonStore<ShareRecord>,
    name: &str,
    spec: &ShareVolumeSpec,
) -> Result<()> {
    let parent = vstore.inspect(&spec.storage_ref).map_err(|_| {
        Error::Invalid(super::po::tf(
            "ShareVolume '{name}': storageRef '{storage_ref}' does not exist — create it first \
             (`delonix storage create` / `kind: Storage`)",
            &[("name", name), ("storage_ref", &spec.storage_ref)],
        ))
    })?;
    let quota_bytes = spec
        .quota
        .as_deref()
        .map(|q| {
            delonix_volume::parse_size_bytes(q).ok_or_else(|| {
                Error::Invalid(super::po::tf(
                    "invalid quota: {q}",
                    &[("q", &format!("{q:?}"))],
                ))
            })
        })
        .transpose()?;

    // `register_external`'s own name-charset validation runs BEFORE it
    // touches disk — this join can't escape `<parent>/shares/` with a name
    // that will end up being rejected anyway.
    let subdir = Path::new(&parent.mountpoint).join("shares").join(name);
    let vol = vstore.register_external(name, &subdir, quota_bytes, spec.alert_pct)?;

    // Idempotent re-apply preserves the original `created_unix`.
    let created_unix = store
        .load(name)
        .map(|r| r.created_unix)
        .unwrap_or_else(|_| output::now_unix());
    let rec = ShareRecord {
        name: name.to_string(),
        storage_ref: spec.storage_ref.clone(),
        mountpoint: vol.mountpoint.clone(),
        quota_bytes,
        alert_pct: spec.alert_pct,
        created_unix,
    };
    store.save(name, &rec)?;
    println!(
        "sharevolume/{name}: {} ({} -> {})",
        super::po::t("ready"),
        spec.storage_ref,
        vol.mountpoint
    );
    Ok(())
}

fn alert_label(warn: bool, over: bool) -> &'static str {
    if over {
        "OVER"
    } else if warn {
        "WARN"
    } else {
        "-"
    }
}

/// `sharevolume ls -o json` row (ADR-0005): raw bytes + booleans (not the human
/// `">= X"`/`OVER`/`?` strings), with `used_complete`/`measured` so a consumer can
/// tell an incomplete measurement (mapped-userns EACCES) from a real value.
#[derive(serde::Serialize)]
struct ShareVolumeLsRow {
    name: String,
    storage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota_bytes: Option<u64>,
    used_bytes: u64,
    used_complete: bool,
    in_alert: bool,
    above_quota: bool,
    measured: bool,
    mountpoint: String,
}

fn cmd_ls(
    vstore: &VolumeStore,
    sstore: &JsonStore<ShareRecord>,
    format: output::OutputFormat,
) -> Result<()> {
    if format == output::OutputFormat::Json {
        let mut rows = Vec::new();
        for rec in sstore.list()? {
            let path = Path::new(&rec.mountpoint);
            let u = super::volume::measured_usage(path);
            let qs = vstore.quota_state_at_checked(path, rec.quota_bytes, rec.alert_pct);
            rows.push(ShareVolumeLsRow {
                name: rec.name,
                storage: rec.storage_ref,
                quota_bytes: rec.quota_bytes,
                used_bytes: u.bytes,
                used_complete: u.is_complete(),
                in_alert: qs.in_alert,
                above_quota: qs.above_quota,
                measured: qs.measured,
                mountpoint: rec.mountpoint,
            });
        }
        return output::print_json(&rows);
    }
    let mut t = output::Table::new(&["NAME", "STORAGE", "QUOTA", "USED", "ALERT", "MOUNTPOINT"]);
    for rec in sstore.list()? {
        let path = Path::new(&rec.mountpoint);
        // MEASURED usage, with the mapped-userns fallback: a tenant's share is
        // written by a container in a mapped userns, so the direct walk hits
        // EACCES and used to report a flat `0 B` — a per-tenant quota display
        // that could never leave 0%. See `volume::measured_usage`.
        let u = super::volume::measured_usage(path);
        let qs = vstore.quota_state_at_checked(path, rec.quota_bytes, rec.alert_pct);
        t.row(vec![
            rec.name,
            rec.storage_ref,
            rec.quota_bytes
                .map(output::fmt_size)
                .unwrap_or_else(|| "-".to_string()),
            if u.is_complete() {
                output::fmt_size(u.bytes)
            } else {
                format!(">= {}", output::fmt_size(u.bytes))
            },
            if !u.is_complete() && !qs.measured {
                "?".to_string()
            } else {
                alert_label(qs.in_alert, qs.above_quota).to_string()
            },
            rec.mountpoint,
        ]);
    }
    t.print();
    Ok(())
}

fn cmd_describe(vstore: &VolumeStore, sstore: &JsonStore<ShareRecord>, name: &str) -> Result<()> {
    let rec = sstore.load(name).map_err(|e| match e {
        Error::NotFound(n) => Error::Invalid(format!(
            "no such sharevolume: {n} (see `delonix sharevolume ls`)"
        )),
        e => e,
    })?;
    let path = Path::new(&rec.mountpoint);
    let u = super::volume::measured_usage(path);
    let qs = vstore.quota_state_at_checked(path, rec.quota_bytes, rec.alert_pct);
    let (warn, over) = (qs.in_alert, qs.above_quota);
    let mut d = output::Describe::new();
    d.field("Name", &rec.name);
    d.field("Storage", &rec.storage_ref);
    d.field("Mountpoint", &rec.mountpoint);
    d.field("Used", super::volume::fmt_measured(u, None));
    d.field_opt("Quota", rec.quota_bytes.map(output::fmt_size).as_deref());
    d.field(
        "Alert",
        if over {
            "OVER QUOTA"
        } else if warn {
            "near quota"
        } else {
            "ok"
        },
    );
    d.field("Created", output::fmt_local(rec.created_unix));
    d.field(
        "Consume with",
        format!("-v {}:/path/in/container", rec.name),
    );
    d.print();
    Ok(())
}

fn cmd_rm(
    vstore: &VolumeStore,
    sstore: &JsonStore<ShareRecord>,
    name: &str,
    purge_data: bool,
) -> Result<()> {
    let rec = sstore.load(name).map_err(|e| match e {
        Error::NotFound(n) => Error::Invalid(format!(
            "no such sharevolume: {n} (see `delonix sharevolume ls`)"
        )),
        e => e,
    })?;
    // Best-effort: `remove` only ever deletes THIS store's own bookkeeping
    // dir (see `register_external`'s doc) — the shared data is untouched.
    let _ = vstore.remove(name);
    if purge_data {
        // NEVER claim a deletion that did not happen. This was
        // `let _ = std::fs::remove_dir_all(...)`, and in rootless a tenant's share
        // is written by a container in a mapped userns — so the directory belongs to
        // a SUBUID, `remove_dir_all` fails with EACCES, and the command still
        // printed "removed (data deleted)". An operator offboarding a tenant (or
        // answering an erasure request) was told the data was gone while every byte
        // was still on the NAS. `remove_tree_mapped` is the same helper that makes
        // `volumes rm` work on subuid data; if the tree STILL survives, that is an
        // error, not a footnote.
        //
        // Plain removal FIRST, mapped userns only as the fallback: when we own the
        // data (root, or a share written as our own uid) a single `remove_dir_all`
        // is enough, and `remove_tree_mapped` forks a child that re-execs
        // `current_exe()` — which is the right thing for the real binary but
        // re-enters the TEST harness under a test binary, where the harness reads
        // `__rmtree <path>` as name filters, runs zero tests and exits 0. That
        // "success" would suppress any fallback and leave the tree in place. Trying
        // the cheap path first avoids both the fork and that trap.
        if std::fs::remove_dir_all(&rec.mountpoint).is_err() {
            delonix_runtime::remove_tree_mapped(std::path::Path::new(&rec.mountpoint));
        }
        if std::path::Path::new(&rec.mountpoint).exists() {
            return Err(Error::Runtime {
                context: "sharevolume purge",
                message: super::po::tf(
                    "the data of '{name}' at {path} could NOT be deleted — the share record was \
                     kept so it is not lost track of; delete it as the data's owner",
                    &[("name", name), ("path", &rec.mountpoint)],
                ),
            });
        }
    }
    sstore.remove(name)?;
    delonix_runtime_core::events::emit(
        &state_root(),
        "sharevolume",
        "remove",
        name,
        name,
        if purge_data { Some("purge-data") } else { None },
    );
    println!(
        "sharevolume/{name}: {}{}",
        super::po::t("removed"),
        if purge_data {
            format!(" ({})", super::po::t("data deleted"))
        } else {
            String::new()
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_label_prioriza_over_sobre_warn() {
        assert_eq!(alert_label(false, false), "-");
        assert_eq!(alert_label(true, false), "WARN");
        assert_eq!(alert_label(true, true), "OVER");
        assert_eq!(alert_label(false, true), "OVER");
    }

    fn stores() -> (VolumeStore, JsonStore<ShareRecord>, PathBuf) {
        // A UNIQUE dir per call, not per call SITE: `line!()` here would
        // always be the same line (this helper is shared by every test), so
        // tests running in parallel (the default Rust test runner) raced on
        // the SAME temp dir — one test's `remove_dir_all` cleanup deleted
        // another's still-in-use "nas-shared" mid-run. An atomic counter
        // guarantees a fresh dir even for tests started in the same instant.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!(
            "delonix-sharevolume-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            seq
        ));
        (
            VolumeStore::open(&tmp).unwrap(),
            JsonStore::open(tmp.join("sharevolumes")).unwrap(),
            tmp,
        )
    }

    #[test]
    fn apply_recusa_storage_ref_inexistente() {
        let (vstore, sstore, tmp) = stores();
        let spec = ShareVolumeSpec {
            storage_ref: "nao-existe".to_string(),
            quota: None,
            alert_pct: None,
        };
        let err = apply_one(&vstore, &sstore, "sv1", &spec).unwrap_err();
        assert!(format!("{err}").contains("storageRef"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_e_idempotente_e_isola_por_subdirectorio() {
        let (vstore, sstore, tmp) = stores();
        // The parent "Storage" — a plain local volume stands in for a
        // network one here (register_external doesn't care which).
        vstore.create("nas-shared").unwrap();
        let spec = ShareVolumeSpec {
            storage_ref: "nas-shared".to_string(),
            quota: Some("1M".to_string()),
            alert_pct: Some(80),
        };
        apply_one(&vstore, &sstore, "tenant-a", &spec).unwrap();
        apply_one(&vstore, &sstore, "tenant-b", &spec).unwrap();

        let a = sstore.load("tenant-a").unwrap();
        let b = sstore.load("tenant-b").unwrap();
        assert_ne!(
            a.mountpoint, b.mountpoint,
            "cada tenant tem o SEU subdirectório"
        );
        assert!(a.mountpoint.contains("nas-shared"));
        assert!(a.mountpoint.ends_with("tenant-a"));
        assert_eq!(a.quota_bytes, Some(1024 * 1024));

        // Idempotent re-apply: same name, `created_unix` preserved.
        std::thread::sleep(std::time::Duration::from_millis(5));
        apply_one(&vstore, &sstore, "tenant-a", &spec).unwrap();
        let a2 = sstore.load("tenant-a").unwrap();
        assert_eq!(a.created_unix, a2.created_unix);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rm_sem_purge_preserva_os_dados() {
        let (vstore, sstore, tmp) = stores();
        vstore.create("nas-shared").unwrap();
        let spec = ShareVolumeSpec {
            storage_ref: "nas-shared".to_string(),
            quota: None,
            alert_pct: None,
        };
        apply_one(&vstore, &sstore, "tenant-a", &spec).unwrap();
        let mountpoint = sstore.load("tenant-a").unwrap().mountpoint;
        std::fs::write(Path::new(&mountpoint).join("f"), b"data").unwrap();

        cmd_rm(&vstore, &sstore, "tenant-a", false).unwrap();
        assert!(
            sstore.load("tenant-a").is_err(),
            "o registo devia ter desaparecido"
        );
        assert!(
            Path::new(&mountpoint).join("f").exists(),
            "sem --purge-data os dados devem sobreviver"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rm_com_purge_apaga_os_dados() {
        let (vstore, sstore, tmp) = stores();
        vstore.create("nas-shared").unwrap();
        let spec = ShareVolumeSpec {
            storage_ref: "nas-shared".to_string(),
            quota: None,
            alert_pct: None,
        };
        apply_one(&vstore, &sstore, "tenant-a", &spec).unwrap();
        let mountpoint = sstore.load("tenant-a").unwrap().mountpoint;

        cmd_rm(&vstore, &sstore, "tenant-a", true).unwrap();
        assert!(
            !Path::new(&mountpoint).exists(),
            "--purge-data deve apagar o subdirectório"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

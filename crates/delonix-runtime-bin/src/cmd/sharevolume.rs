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
    /// Namespace that OWNS this share. Legacy records have no field and deserialize as
    /// `default`, which is exactly what they were: before scoping there was one flat
    /// space and everything lived in it.
    #[serde(default = "ns_default")]
    namespace: String,
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
    Describe {
        name: String,
        /// Namespace that owns the share (default `default`).
        #[arg(long, short = 'n')]
        namespace: Option<String>,
    },
    /// Un-register a share volume. The underlying data (a subdirectory of
    /// the parent Storage) is PRESERVED unless `--purge-data` is passed.
    Rm {
        name: String,
        #[arg(long = "purge-data")]
        purge_data: bool,
        /// Namespace that owns the share (default `default`).
        #[arg(long, short = 'n')]
        namespace: Option<String>,
    },
    /// Move the pre-scoping share records into the `default` namespace.
    ///
    /// Records only: the DATA is not moved, because each record carries the path it was
    /// created with and moving a tenant's bytes is not something a migration should do
    /// behind their back. What changes is where the record lives and that its backing
    /// volume becomes `default`-scoped, so `-v <name>` resolves through the namespaced
    /// path like every share created from now on.
    Migrate {
        /// Show what would move, change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run(action: ShareVolumeCmd) -> Result<()> {
    let root = state_root();
    let vstore = VolumeStore::open(&root)?;
    match action {
        ShareVolumeCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply_with(&docs)
        }
        ShareVolumeCmd::Ls { output } => cmd_ls(&root, &vstore, output),
        ShareVolumeCmd::Describe { name, namespace } => {
            cmd_describe(&root, &vstore, &name, &namespace.unwrap_or_else(ns_default))
        }
        ShareVolumeCmd::Rm {
            name,
            purge_data,
            namespace,
        } => cmd_rm(
            &root,
            &name,
            purge_data,
            &namespace.unwrap_or_else(ns_default),
        ),
        ShareVolumeCmd::Migrate { dry_run } => cmd_migrate(&root, dry_run),
    }
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    apply_with(docs)
}

fn apply_with(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, "ShareVolume") {
        manifest::warn_unknown_fields(doc, SHAREVOLUME_SPEC_FIELDS);
        let spec: ShareVolumeSpec = manifest::spec_of(doc)?;
        let ns = doc.metadata.namespace.clone().unwrap_or_else(ns_default);
        apply_one(&state_root(), &doc.metadata.name, &spec, &ns)?;
    }
    Ok(())
}

fn ns_default() -> String {
    "default".to_string()
}

/// The share records of ONE namespace: `sharevolumes/<ns>/`.
///
/// A directory per namespace, not a composed key: `JsonStore`'s `safe_key` maps `/` to `-`,
/// so `<ns>/<name>` would flatten into exactly the kind of ambiguous key
/// (`a/b-c` and `a-b/c` collide) that the compose project names already had to fix.
fn shares_store_ns(root: &Path, namespace: &str) -> Result<JsonStore<ShareRecord>> {
    JsonStore::open(root.join("sharevolumes").join(safe_ns(namespace)))
}

/// The pre-scoping store: records sitting flat in `sharevolumes/`. Still READ so that
/// nothing created before this change stops working — the second of the two read paths.
fn shares_store_legacy(root: &Path) -> Result<JsonStore<ShareRecord>> {
    JsonStore::open(root.join("sharevolumes"))
}

/// A namespace safe as ONE path component. Rejects the traversal and separator cases
/// instead of sanitizing them into something else: a namespace that silently becomes a
/// different namespace is how a tenant reads another tenant's shares.
fn safe_ns(namespace: &str) -> String {
    if namespace.is_empty()
        || namespace == "."
        || namespace == ".."
        || namespace.contains('/')
        || namespace.starts_with('.')
    {
        return "default".to_string();
    }
    namespace.to_string()
}

/// Loads a share, preferring its namespace and falling back to a legacy flat record.
/// The fallback only applies to `default`: a legacy record was, by definition, unscoped.
fn load_record(root: &Path, namespace: &str, name: &str) -> Result<(ShareRecord, bool)> {
    if let Ok(rec) = shares_store_ns(root, namespace)?.load(name) {
        return Ok((rec, false));
    }
    if namespace == "default" {
        if let Ok(rec) = shares_store_legacy(root)?.load(name) {
            return Ok((rec, true));
        }
    }
    Err(Error::Invalid(format!(
        "no such sharevolume: {name} in namespace {namespace} (see `delonix sharevolume ls`)"
    )))
}

/// Every share on the node, namespace by namespace, plus the legacy flat ones.
fn list_all(root: &Path) -> Result<Vec<ShareRecord>> {
    let mut out = Vec::new();
    let base = root.join("sharevolumes");
    // Legacy: records directly under `sharevolumes/` (they report as `default`).
    if let Ok(st) = shares_store_legacy(root) {
        out.extend(st.list().unwrap_or_default());
    }
    if let Ok(rd) = std::fs::read_dir(&base) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let ns = e.file_name().to_string_lossy().to_string();
            if let Ok(st) = shares_store_ns(root, &ns) {
                out.extend(st.list().unwrap_or_default());
            }
        }
    }
    out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(out)
}

/// Moves the pre-scoping share records into the `default` namespace.
///
/// **Records, not data.** Each record carries the path it was created with, so a migrated
/// share keeps reading and writing exactly where it already did; moving a tenant's bytes is
/// not something a migration should do behind their back. What changes is where the record
/// lives and that its backing volume becomes `default`-scoped, so `-v <name>` resolves
/// through the namespaced path like every share created from now on.
///
/// Order is deliberate, and it is the lesson from the volume-removal bug: the new
/// registration is written FIRST and the old bookkeeping removed LAST. If it dies in
/// between, both exist — and `resolve_spec_in` then REFUSES that name as ambiguous instead
/// of silently picking one, which is a stopped workload with a clear message rather than a
/// workload writing into the wrong place.
fn cmd_migrate(root: &Path, dry_run: bool) -> Result<()> {
    let legacy = shares_store_legacy(root)?;
    let already = shares_store_ns(root, "default")?;
    let global = VolumeStore::open(root)?;
    let scoped = VolumeStore::open_scoped(root, "default")?;
    let mut moved = 0usize;
    for rec in legacy.list().unwrap_or_default() {
        if already.load(&rec.name).is_ok() {
            super::output::warn(&super::po::tf(
                "sharevolume '{name}': a namespaced record already exists — left alone, \
                 resolve it by hand",
                &[("name", &rec.name)],
            ));
            continue;
        }
        if dry_run {
            println!(
                "{}",
                super::po::tf(
                    "would migrate sharevolume/{name} to namespace default",
                    &[("name", &rec.name)],
                )
            );
            moved += 1;
            continue;
        }
        let mut next = rec.clone();
        next.namespace = "default".to_string();
        scoped.register_external(
            &rec.name,
            std::path::Path::new(&rec.mountpoint),
            rec.quota_bytes,
            rec.alert_pct,
        )?;
        already.save(&rec.name, &next)?;
        legacy.remove(&rec.name)?;
        // Last: the global bookkeeping dir. `remove` never touches the shared data
        // (see `register_external`'s doc) — only this store's own directory.
        let _ = global.remove(&rec.name);
        println!(
            "{}",
            super::po::tf(
                "sharevolume/{name}: migrated to namespace default",
                &[("name", &rec.name)],
            )
        );
        moved += 1;
    }
    if moved == 0 {
        println!("{}", super::po::t("nothing to migrate"));
    }
    Ok(())
}

fn apply_one(root: &Path, name: &str, spec: &ShareVolumeSpec, namespace: &str) -> Result<()> {
    let namespace = safe_ns(namespace);
    // The parent Storage is NOT namespaced: it is the NAS mount itself, node
    // infrastructure, and scoping it would mean one mount per namespace of the same
    // export. What gets scoped is the SHARE carved out of it.
    let vstore = VolumeStore::open(root)?;
    // An already-existing share keeps the path it was created with. `apply` is
    // "ensure present", so recomputing the path on a re-apply would move a legacy
    // share's data out from under it and orphan every byte already written there —
    // `sharevolume migrate` is the explicit way to move one, on purpose.
    let existing = load_record(root, &namespace, name).ok();
    let legacy = existing.as_ref().map(|(_, l)| *l).unwrap_or(false);
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
    // New shares live under their namespace: `<storage>/shares/<ns>/<name>`. Without the
    // namespace component two tenants that both call their share `db` get the SAME
    // directory — and `rm --purge-data` on one deletes the other's data.
    let subdir = match &existing {
        Some((rec, _)) => std::path::PathBuf::from(&rec.mountpoint),
        None => Path::new(&parent.mountpoint)
            .join("shares")
            .join(&namespace)
            .join(name),
    };
    // The backing volume goes in the namespace's own sub-tree so `-v <name>` resolves to
    // THIS namespace's share (`VolumeStore::resolve_spec_in`). A legacy share keeps its
    // global registration until `migrate` moves it, so nothing consuming it breaks today.
    let reg = if legacy {
        VolumeStore::open(root)?
    } else {
        VolumeStore::open_scoped(root, &namespace)?
    };
    let vol = reg.register_external(name, &subdir, quota_bytes, spec.alert_pct)?;

    // Idempotent re-apply preserves the original `created_unix`.
    let created_unix = existing
        .as_ref()
        .map(|(r, _)| r.created_unix)
        .unwrap_or_else(output::now_unix);
    let rec = ShareRecord {
        name: name.to_string(),
        namespace: namespace.clone(),
        storage_ref: spec.storage_ref.clone(),
        mountpoint: vol.mountpoint.clone(),
        quota_bytes,
        alert_pct: spec.alert_pct,
        created_unix,
    };
    if legacy {
        shares_store_legacy(root)?.save(name, &rec)?;
    } else {
        shares_store_ns(root, &namespace)?.save(name, &rec)?;
    }
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
    namespace: String,
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

fn cmd_ls(root: &Path, vstore: &VolumeStore, format: output::OutputFormat) -> Result<()> {
    if format == output::OutputFormat::Json {
        let mut rows = Vec::new();
        for rec in list_all(root)? {
            let path = Path::new(&rec.mountpoint);
            let u = super::volume::measured_usage(path);
            let qs = vstore.quota_state_at_checked(path, rec.quota_bytes, rec.alert_pct);
            rows.push(ShareVolumeLsRow {
                name: rec.name,
                namespace: rec.namespace,
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
    // Lists EVERY namespace, never just one: a share that exists and is invisible is how
    // an operator deletes a Storage believing nothing hangs off it.
    let mut t = output::Table::new(&[
        "NAMESPACE",
        "NAME",
        "STORAGE",
        "QUOTA",
        "USED",
        "ALERT",
        "MOUNTPOINT",
    ]);
    for rec in list_all(root)? {
        let path = Path::new(&rec.mountpoint);
        // MEASURED usage, with the mapped-userns fallback: a tenant's share is
        // written by a container in a mapped userns, so the direct walk hits
        // EACCES and used to report a flat `0 B` — a per-tenant quota display
        // that could never leave 0%. See `volume::measured_usage`.
        let u = super::volume::measured_usage(path);
        let qs = vstore.quota_state_at_checked(path, rec.quota_bytes, rec.alert_pct);
        t.row(vec![
            rec.namespace,
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

fn cmd_describe(root: &Path, vstore: &VolumeStore, name: &str, namespace: &str) -> Result<()> {
    let (rec, _legacy) = load_record(root, namespace, name)?;
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

fn cmd_rm(root: &Path, name: &str, purge_data: bool, namespace: &str) -> Result<()> {
    let (rec, legacy) = load_record(root, namespace, name)?;
    // The bookkeeping lives where the record lives: a scoped share de-registers from its
    // namespace's volume sub-tree, a legacy one from the global store.
    let vstore = if legacy {
        VolumeStore::open(root)?
    } else {
        VolumeStore::open_scoped(root, &rec.namespace)?
    };
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
    if legacy {
        shares_store_legacy(root)?.remove(name)?;
    } else {
        shares_store_ns(root, &rec.namespace)?.remove(name)?;
    }
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
        // The share store is now the `default` NAMESPACE's, which is where `apply_one`
        // writes; the flat `sharevolumes/` is the legacy path and only `migrate` touches it.
        (
            VolumeStore::open(&tmp).unwrap(),
            JsonStore::open(tmp.join("sharevolumes").join("default")).unwrap(),
            tmp,
        )
    }

    #[test]
    fn apply_recusa_storage_ref_inexistente() {
        let (_vstore, _sstore, tmp) = stores();
        let spec = ShareVolumeSpec {
            storage_ref: "nao-existe".to_string(),
            quota: None,
            alert_pct: None,
        };
        let err = apply_one(&tmp, "sv1", &spec, "default").unwrap_err();
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
        apply_one(&tmp, "tenant-a", &spec, "default").unwrap();
        apply_one(&tmp, "tenant-b", &spec, "default").unwrap();

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
        apply_one(&tmp, "tenant-a", &spec, "default").unwrap();
        let a2 = sstore.load("tenant-a").unwrap();
        assert_eq!(a.created_unix, a2.created_unix);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// O invariante do B2: dois inquilinos, o MESMO nome, e um purge que nao atravessa a
    /// fronteira.
    ///
    /// Antes do escopo, o caminho dos dados era `<storage>/shares/<nome>` — sem namespace —
    /// por isso dois `db` partilhavam o directorio e o `--purge-data` de um levava os dados
    /// do outro. O teste afirma as tres coisas que tem de valer ao mesmo tempo: caminhos
    /// distintos, `-v db` a resolver para o `db` da SUA namespace, e o purge de um a deixar
    /// o outro intacto.
    #[test]
    fn dois_namespaces_com_o_mesmo_share_nao_se_tocam() {
        let (vstore, _sstore, tmp) = stores();
        vstore.create("nas-shared").unwrap();
        let spec = ShareVolumeSpec {
            storage_ref: "nas-shared".to_string(),
            quota: None,
            alert_pct: None,
        };
        apply_one(&tmp, "db", &spec, "teamA").unwrap();
        apply_one(&tmp, "db", &spec, "teamB").unwrap();

        let (a, _) = load_record(&tmp, "teamA", "db").unwrap();
        let (b, _) = load_record(&tmp, "teamB", "db").unwrap();
        assert_ne!(
            a.mountpoint, b.mountpoint,
            "cada namespace tem o SEU caminho"
        );
        assert!(a.mountpoint.contains("teamA"), "{}", a.mountpoint);
        assert!(b.mountpoint.contains("teamB"), "{}", b.mountpoint);

        // Dados reais dos dois lados.
        std::fs::write(Path::new(&a.mountpoint).join("marca"), b"de-A").unwrap();
        std::fs::write(Path::new(&b.mountpoint).join("marca"), b"de-B").unwrap();

        // `-v db:/data` resolve para o db da namespace de quem monta, nao para o outro.
        let m = vstore.resolve_spec_in("db:/data", "teamB").unwrap();
        assert_eq!(m.source, b.mountpoint, "teamB tem de receber o SEU db");

        // O purge de um NAO pode tocar no outro.
        cmd_rm(&tmp, "db", true, "teamA").unwrap();
        assert!(
            !Path::new(&a.mountpoint).exists(),
            "os dados de teamA tinham de desaparecer"
        );
        assert_eq!(
            std::fs::read(Path::new(&b.mountpoint).join("marca")).unwrap(),
            b"de-B",
            "os dados de teamB tinham de ficar INTACTOS"
        );
        assert!(
            load_record(&tmp, "teamB", "db").is_ok(),
            "o registo de teamB sobrevive"
        );

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
        apply_one(&tmp, "tenant-a", &spec, "default").unwrap();
        let mountpoint = sstore.load("tenant-a").unwrap().mountpoint;
        std::fs::write(Path::new(&mountpoint).join("f"), b"data").unwrap();

        cmd_rm(&tmp, "tenant-a", false, "default").unwrap();
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
        apply_one(&tmp, "tenant-a", &spec, "default").unwrap();
        let mountpoint = sstore.load("tenant-a").unwrap().mountpoint;

        cmd_rm(&tmp, "tenant-a", true, "default").unwrap();
        assert!(
            !Path::new(&mountpoint).exists(),
            "--purge-data deve apagar o subdirectório"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

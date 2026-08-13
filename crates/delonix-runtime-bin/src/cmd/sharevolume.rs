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

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct ShareVolumeSpec {
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

// The reconciler adapter that used to live here — `RECONCILED_SHARE_FIELDS`,
// `desired`, `actual`, `presence_of` — is gone with the Kind. A share is a
// `kind: Volume` now, so `volume::desired`/`actual` describe it, and they see it
// through the same record every other volume has instead of through a second one
// beside it. That is what made it ownable: ownership is a label on the volume.

#[derive(Subcommand)]
pub enum ShareVolumeCmd {
    /// Apply the `kind: ShareVolume` documents of a manifest (idempotent).
    Apply {
        #[arg(value_hint = clap::ValueHint::FilePath, short, long)]
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
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::sharevolumes))]
        name: String,
        /// Namespace that owns the share (default `default`).
        #[arg(long, short = 'n', add = clap_complete::engine::ArgValueCandidates::new(super::complete::namespaces))]
        namespace: Option<String>,
    },
    /// Un-register a share volume.
    ///
    /// The underlying data (a subdirectory of the parent Storage) is PRESERVED
    /// unless `--purge-data` is passed.
    Rm {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::sharevolumes))]
        name: String,
        #[arg(long = "purge-data")]
        purge_data: bool,
        /// Namespace that owns the share (default `default`).
        #[arg(long, short = 'n', add = clap_complete::engine::ArgValueCandidates::new(super::complete::namespaces))]
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
            // After `load` there is no `kind: ShareVolume` left — it lowers to a
            // `kind: Volume` with a `share:` block. This command keeps its
            // promise («apply the shares of this manifest, and nothing else») by
            // filtering on the block rather than on a Kind that no longer
            // survives: applying every volume in the file would silently do more
            // than the command's name says.
            let shares: Vec<ManifestDoc> = docs
                .into_iter()
                .filter(|d| d.kind == "Volume" && d.spec.get("share").is_some())
                .collect();
            super::volume::apply(&shares)
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

/// Applies one `kind: Volume` that carries a `share:` block.
///
/// The single entry point from the declarative side since `kind: ShareVolume`
/// became a spelling of `kind: Volume` — there is no per-Kind `apply` here any
/// more, because after `load` no document of that Kind survives.
pub(crate) fn apply_share(
    name: &str,
    from: &str,
    quota: Option<&str>,
    alert_pct: Option<u8>,
    namespace: &str,
) -> Result<()> {
    let spec = ShareVolumeSpec {
        storage_ref: from.to_string(),
        quota: quota.map(str::to_string),
        alert_pct,
    };
    apply_one(&state_root(), name, &spec, namespace)
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

/// A share read back from the volume that IS it.
///
/// `Volume.parent` is what makes a volume a share; everything else the old
/// `ShareRecord` held was already on the volume beside it. Returns `None` for a
/// plain volume, which is what keeps `list_all` from reporting every volume on
/// the node as a share.
fn share_from_volume(v: &delonix_volume::Volume, namespace: &str) -> Option<ShareRecord> {
    Some(ShareRecord {
        name: v.name.clone(),
        namespace: namespace.to_string(),
        storage_ref: v.parent.clone()?,
        mountpoint: v.mountpoint.clone(),
        quota_bytes: v.quota_bytes,
        alert_pct: v.alert_pct,
        created_unix: v.created_unix,
    })
}

/// Loads a share, from the volume first and from a not-yet-absorbed record second.
///
/// Four places, in the order that keeps the CURRENT format authoritative:
/// namespaced volume, namespaced record, and — only for `default`, because a
/// pre-scoping share was by definition unscoped — the global volume and the flat
/// record. The `bool` is what it always was: whether the share lives in the
/// GLOBAL store rather than a namespace's, which decides where `rm` de-registers.
///
/// A record still being read here is not a leftover to clean up on sight: it is a
/// share created before the merge, whose data is exactly where it always was. The
/// absorption happens on the next `apply`, which is the moment there is something
/// to write anyway.
fn load_record(root: &Path, namespace: &str, name: &str) -> Result<(ShareRecord, bool)> {
    let ns = safe_ns(namespace);
    if let Ok(v) = VolumeStore::open_scoped(root, &ns)?.inspect(name) {
        if let Some(rec) = share_from_volume(&v, &ns) {
            return Ok((rec, false));
        }
    }
    if let Ok(rec) = shares_store_ns(root, namespace)?.load(name) {
        return Ok((rec, false));
    }
    if namespace == "default" {
        if let Ok(v) = VolumeStore::open(root)?.inspect(name) {
            if let Some(rec) = share_from_volume(&v, "default") {
                return Ok((rec, true));
            }
        }
        if let Ok(rec) = shares_store_legacy(root)?.load(name) {
            return Ok((rec, true));
        }
    }
    Err(Error::NotFound(format!(
        "no such sharevolume: {name} in namespace {namespace} (see `delonix sharevolume ls`)"
    )))
}

/// Every share on the node: the volumes that name a parent, plus any record not
/// yet absorbed into one.
///
/// De-duplicated by `(namespace, name)` with the VOLUME winning, because during
/// the window between the merge and a share's next `apply` both exist and they
/// describe the same directory — listing it twice would read as two shares, and
/// picking the record would show the older of two answers.
fn list_all(root: &Path) -> Result<Vec<ShareRecord>> {
    let mut out: Vec<ShareRecord> = Vec::new();
    let mut seen: std::collections::BTreeSet<(String, String)> = Default::default();
    let mut push = |rec: ShareRecord, out: &mut Vec<ShareRecord>| {
        if seen.insert((rec.namespace.clone(), rec.name.clone())) {
            out.push(rec);
        }
    };
    let global = VolumeStore::open(root)?;
    for ns in global.namespaces() {
        let Ok(store) = VolumeStore::open_scoped(root, &ns) else {
            continue;
        };
        for v in store.list().unwrap_or_default() {
            if let Some(rec) = share_from_volume(&v, &ns) {
                push(rec, &mut out);
            }
        }
    }
    // A pre-scoping share lives in the global store and reports as `default`,
    // which is what it was.
    for v in global.list().unwrap_or_default() {
        if let Some(rec) = share_from_volume(&v, "default") {
            push(rec, &mut out);
        }
    }
    // Records not yet absorbed — flat ones first, for the same `default` reason.
    if let Ok(st) = shares_store_legacy(root) {
        for rec in st.list().unwrap_or_default() {
            push(rec, &mut out);
        }
    }
    if let Ok(rd) = std::fs::read_dir(root.join("sharevolumes")) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let ns = e.file_name().to_string_lossy().to_string();
            if let Ok(st) = shares_store_ns(root, &ns) {
                for rec in st.list().unwrap_or_default() {
                    push(rec, &mut out);
                }
            }
        }
    }
    out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(out)
}

/// Share volume names, for shell autocompletion (`cmd::complete::sharevolumes`).
///
/// Goes through `list_all` on purpose: the records are split across one
/// directory per namespace PLUS the legacy flat ones, and a completer with its
/// own copy of that layout would quietly stop offering half of them the day it
/// changes. Never fails — a TAB with no store yet is "no suggestions".
pub(crate) fn completion_names() -> Vec<String> {
    list_all(&state_root())
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.name)
        .collect()
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
            Some(&rec.storage_ref),
        )?;
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
    let vol = reg.register_external(
        name,
        &subdir,
        quota_bytes,
        spec.alert_pct,
        Some(&spec.storage_ref),
    )?;

    // Absorbs a pre-merge record, and the ORDER is the one this repo pays for
    // every time it gets it wrong: the volume — the new and only source of truth
    // — is written FIRST, above, and the old bookkeeping is dropped only after
    // it. Dying in between leaves both, and `load_record` prefers the volume, so
    // the share keeps answering with the same values; dropping the record first
    // and dying would leave a directory full of a tenant's bytes with nothing on
    // the node saying whose it is.
    //
    // Unconditional because `JsonStore::remove` is idempotent on a missing key:
    // asking whether there is a record first would be a second read whose only
    // effect is to make the ordering above harder to see.
    let _ = shares_store_legacy(root)?.remove(name);
    let _ = shares_store_ns(root, &namespace)?.remove(name);
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
    // Both, unconditionally: the share may still have a pre-merge record beside
    // its volume, and `JsonStore::remove` is idempotent on a missing key. Leaving
    // one behind would make the name resolve again from the record after the
    // volume is gone — a share pointing at a directory nobody registers.
    let _ = shares_store_legacy(root)?.remove(name);
    let _ = shares_store_ns(root, &rec.namespace)?.remove(name);
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

    /// **A fusão só vale se um share já criado sobreviver a ela**, e o que a
    /// prova é o caminho de leitura ANTIGO deixar de ser preciso sem que nada
    /// se perca: o registo é absorvido pelo volume, com o MOUNTPOINT intacto.
    ///
    /// O mountpoint é a asserção que importa. Recalculá-lo em vez de o preservar
    /// mudaria o directório debaixo de um inquilino cujos bytes já lá estão — a
    /// armadilha que o `apply_one` evita desde que os shares ganharam namespace,
    /// e que uma migração é a ocasião perfeita para reintroduzir.
    #[test]
    fn um_share_pre_fusao_e_absorvido_pelo_volume_sem_mudar_de_sitio() {
        let (vstore, sstore, tmp) = stores();
        vstore.create("nas-shared").unwrap();
        // Um registo como a versão anterior o escrevia: JsonStore + volume SEM
        // `parent`. O caminho é o que essa versão calculava.
        let mountpoint = tmp
            .join("volumes/nas-shared/_data/shares/default/legado")
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(&mountpoint).unwrap();
        std::fs::write(std::path::Path::new(&mountpoint).join("dados.txt"), b"x").unwrap();
        sstore
            .save(
                "legado",
                &ShareRecord {
                    name: "legado".into(),
                    namespace: "default".into(),
                    storage_ref: "nas-shared".into(),
                    mountpoint: mountpoint.clone(),
                    quota_bytes: Some(1024 * 1024),
                    alert_pct: Some(80),
                    created_unix: 111,
                },
            )
            .unwrap();

        // Antes do apply, lê-se pelo registo — é o que garante que nada quebra
        // entre a instalação da versão nova e o apply seguinte.
        let (antes, _) = load_record(&tmp, "default", "legado").unwrap();
        assert_eq!(antes.storage_ref, "nas-shared");
        assert_eq!(antes.mountpoint, mountpoint);

        // `apply_one` e nao `apply_share`: aquele recebe o root, este resolve
        // `state_root()` — um teste a chamar o segundo escreveria no estado REAL
        // da maquina, e so nao o fez por o `nas-shared` de la nao existir.
        apply_one(
            &tmp,
            "legado",
            &ShareVolumeSpec {
                storage_ref: "nas-shared".into(),
                quota: Some("1M".into()),
                alert_pct: Some(80),
            },
            "default",
        )
        .unwrap();

        // Depois: o volume É o registo, e o registo antigo desapareceu.
        let scoped = VolumeStore::open_scoped(&tmp, "default").unwrap();
        let v = scoped.inspect("legado").unwrap();
        assert_eq!(v.parent.as_deref(), Some("nas-shared"));
        assert_eq!(
            v.mountpoint, mountpoint,
            "a absorção mudou o directório debaixo dos dados"
        );
        assert!(
            std::path::Path::new(&mountpoint).join("dados.txt").exists(),
            "os bytes do inquilino têm de continuar lá"
        );
        assert!(
            sstore.load("legado").is_err(),
            "o registo antigo tem de ser largado DEPOIS do volume, não deixado a discordar dele"
        );
        // E continua a ler-se — agora pela fonte nova.
        let (depois, legacy) = load_record(&tmp, "default", "legado").unwrap();
        assert_eq!(depois.storage_ref, "nas-shared");
        assert_eq!(depois.mountpoint, mountpoint);
        assert!(
            !legacy,
            "um share em `default` vive no sub-tree dessa namespace"
        );
        // E aparece UMA vez na listagem, não duas.
        let listados: Vec<_> = list_all(&tmp)
            .unwrap()
            .into_iter()
            .filter(|r| r.name == "legado")
            .collect();
        assert_eq!(listados.len(), 1, "{listados:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Um share passa a ser possuível por uma stack, que era o objectivo da
    /// fusão: a posse é um label, e o `ShareRecord` não tinha nenhum.
    #[test]
    fn um_share_e_possuivel_por_uma_stack() {
        let (vstore, _sstore, tmp) = stores();
        vstore.create("nas-shared").unwrap();
        apply_one(
            &tmp,
            "db",
            &ShareVolumeSpec {
                storage_ref: "nas-shared".into(),
                quota: None,
                alert_pct: None,
            },
            "teamA",
        )
        .unwrap();
        let scoped = VolumeStore::open_scoped(&tmp, "teamA").unwrap();
        scoped
            .set_metadata(
                "db",
                &[("delonix.io/stack".to_string(), Some("loja".to_string()))],
                &[],
            )
            .unwrap();
        let v = scoped.inspect("db").unwrap();
        assert_eq!(
            v.labels.get("delonix.io/stack").map(String::as_str),
            Some("loja")
        );
        assert_eq!(v.parent.as_deref(), Some("nas-shared"));
        let _ = std::fs::remove_dir_all(&tmp);
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
        let (vstore, _sstore, tmp) = stores();
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

        // Lido pelo caminho de PRODUCAO (`load_record`), que e agora o volume:
        // um teste contra o `JsonStore` estaria a afirmar um registo que o apply
        // ja nao escreve, e passaria a nao provar nada.
        let (a, _) = load_record(&tmp, "default", "tenant-a").unwrap();
        let (b, _) = load_record(&tmp, "default", "tenant-b").unwrap();
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
        let (a2, _) = load_record(&tmp, "default", "tenant-a").unwrap();
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
        let (vstore, _sstore, tmp) = stores();
        vstore.create("nas-shared").unwrap();
        let spec = ShareVolumeSpec {
            storage_ref: "nas-shared".to_string(),
            quota: None,
            alert_pct: None,
        };
        apply_one(&tmp, "tenant-a", &spec, "default").unwrap();
        let mountpoint = load_record(&tmp, "default", "tenant-a")
            .unwrap()
            .0
            .mountpoint;
        std::fs::write(Path::new(&mountpoint).join("f"), b"data").unwrap();

        cmd_rm(&tmp, "tenant-a", false, "default").unwrap();
        assert!(
            load_record(&tmp, "default", "tenant-a").is_err(),
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
        let (vstore, _sstore, tmp) = stores();
        vstore.create("nas-shared").unwrap();
        let spec = ShareVolumeSpec {
            storage_ref: "nas-shared".to_string(),
            quota: None,
            alert_pct: None,
        };
        apply_one(&tmp, "tenant-a", &spec, "default").unwrap();
        let mountpoint = load_record(&tmp, "default", "tenant-a")
            .unwrap()
            .0
            .mountpoint;

        cmd_rm(&tmp, "tenant-a", true, "default").unwrap();
        assert!(
            !Path::new(&mountpoint).exists(),
            "--purge-data deve apagar o subdirectório"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

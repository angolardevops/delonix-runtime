//! `kind: StoragePool` — the use of a storage pool the administrator declared
//! (ADR-0067).
//!
//! A pool exists because the administrator wrote it into the allowlist
//! (`/etc/delonix/storage-pools.yaml`): its driver and what backs it come from
//! there, and from there only. This Kind says «this node's engine uses the
//! pool called X, with this over-allocation ceiling» — it never carries a
//! device, a path or a command, and those fields are refused by name instead
//! of being ignored.
//!
//! The engine never creates or destroys a pool. `delete storagepools` stops
//! using one; the pool and whatever is in it stay.
//!
//! This module is also the composition root of the storage context: it
//! registers the drivers, and it is the only place a volume is allocated in or
//! released from a pool.

use std::collections::BTreeMap;
use std::sync::Arc;

use delonix_model::{Error, Result};
use delonix_storage::allowlist::{self, Entry};
use delonix_storage::ownership::Owner;
use delonix_storage::pool::{
    PoolOp, PoolProbe, PoolRef, PoolUsage, StoragePoolDriver, VolumeRequest, VolumeShape,
};
use delonix_storage::Error as PoolError;
use delonix_volume::VolumeStore;
use serde::{Deserialize, Serialize};

use super::kinds as k;
use super::manifest::{self, ManifestDoc};
use super::output::{self, OutputFormat};
use super::util::state_root;

/// The allowlist is YAML; the storage context takes the decoder from here.
fn decode_allowlist(text: &str) -> std::result::Result<allowlist::Allowlist, String> {
    serde_yaml::from_str(text).map_err(|e| e.to_string())
}

/// Above this share of the pool's capacity in use, a NEW volume is refused
/// (ADR-0067 D6): a full pool fails every volume in it, not only the last one.
const REFUSE_ABOVE_PCT: u8 = 95;
const DEFAULT_ALERT_PCT: u8 = 80;
const DEFAULT_MAX_RATIO: f64 = 1.0;

/// `spec` of `kind: StoragePool`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct StoragePoolSpec {
    /// How far the sizes allocated in the pool may exceed its capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overcommit: Option<Overcommit>,
    /// Percentage of the pool in use above which an allocation warns (default 80).
    #[serde(default, rename = "alertPct", skip_serializing_if = "Option::is_none")]
    pub alert_pct: Option<u8>,
}

/// `spec.overcommit`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Overcommit {
    /// Allocated sizes over capacity. `1.0` (default) allows no over-allocation.
    #[serde(rename = "maxRatio")]
    pub max_ratio: f64,
}

pub const STORAGE_POOL_SPEC_FIELDS: &[&str] = &["overcommit", "alertPct"];
pub const RECONCILED_STORAGE_POOL_FIELDS: &[&str] = &["maxRatio", "alertPct"];

/// What a manifest may never say about a pool. Each of these is the
/// administrator's, in the allowlist; a document that carries one is asking
/// the engine to act on a device, a path or a command it was handed.
const ADMIN_ONLY_FIELDS: &[&str] = &[
    "driver",
    "path",
    "device",
    "devices",
    "vg",
    "thinPool",
    "dataset",
    "create",
    "mode",
    "command",
    "allowUsers",
    "maxVolumeBytes",
];

/// The use of one pool, as persisted in `<root>/storage-pools/<name>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolRecord {
    pub name: String,
    pub max_ratio: f64,
    pub alert_pct: u8,
    #[serde(default)]
    pub created_unix: u64,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

fn pool_err(e: PoolError) -> Error {
    e.into()
}

// ---- drivers ----------------------------------------------------------------

/// Registers the drivers this build has. Called once at start-up; registering
/// does no I/O.
pub fn register_drivers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // The mapped remover: a volume's data may belong to a sub-uid a
        // container wrote as, which this process cannot unlink directly.
        let dir =
            delonix_volume::pool_dir::DirPoolDriver::new(Some(delonix_linux::remove_tree_mapped));
        let run: Arc<dyn delonix_storage::exec::Run> =
            Arc::new(delonix_volume::pool_exec::SystemRun);
        let mounts = std::path::PathBuf::from(delonix_storage::helper::MOUNT_ROOT);
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        let drivers: [Arc<dyn StoragePoolDriver>; 4] = [
            Arc::new(dir),
            // A btrfs volume is made by this process; its hard quota (a
            // qgroup limit) is an administrative ioctl, set only as root.
            Arc::new(delonix_provider_btrfs::BtrfsDriver::new(
                run.clone(),
                Some(delonix_linux::remove_tree_mapped),
                root,
            )),
            Arc::new(delonix_provider_zfs::ZfsDriver::new(
                run.clone(),
                mounts.clone(),
            )),
            Arc::new(delonix_provider_lvm::LvmThinDriver::new(run.clone(), mounts.clone())),
        ];
        for d in drivers {
            // An id that is a literal cannot be empty, and the four differ;
            // the only refusals there are.
            let _ = delonix_storage::registry::register(d);
        }
        // Ceph (ADR-0081): the `ceph-rbd` and `cephfs` drivers reach the
        // cluster through the same `Run` the local drivers use.
        for d in delonix_provider_ceph::drivers(run, mounts) {
            let _ = delonix_storage::registry::register(d);
        }
    });
}

/// The privileged helper's client, when its socket is on this node. Nothing
/// is dialled here: the first operation that needs it connects.
fn helper_client() -> Option<Arc<dyn delonix_storage::helper::HelperCall>> {
    let path = delonix_storage::helper::socket_path();
    path.exists().then(|| {
        Arc::new(delonix_storage::helper::SocketClient::new(path))
            as Arc<dyn delonix_storage::helper::HelperCall>
    })
}

/// Who this engine is to a pool: this user, this state root.
fn owner() -> Owner {
    let root = state_root();
    let root = root.canonicalize().unwrap_or(root);
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    Owner::new(format!("{uid}:{}", root.display()))
}

/// A pool the allowlist names, with the driver that serves it.
struct Resolved {
    name: String,
    entry: Entry,
    driver: Arc<dyn StoragePoolDriver>,
}

impl Resolved {
    fn pool(&self) -> PoolRef<'_> {
        PoolRef {
            name: &self.name,
            entry: &self.entry,
        }
    }
}

/// Looks `name` up in the administrator's allowlist. Runs before any probe: a
/// name that is not there is refused without touching anything.
fn resolve(name: &str) -> Result<Resolved> {
    register_drivers();
    let file = allowlist::path();
    let list = allowlist::load(decode_allowlist)
        .map_err(pool_err)?
        .ok_or_else(|| {
            pool_err(PoolError::PoolUnavailable(super::po::tf(
                "storage pool '{name}': no pool is declared on this node — {file} does not exist. \
             Pools are declared there by the administrator, never by a manifest",
                &[("name", name), ("file", &file.display().to_string())],
            )))
        })?;
    let entry = list.pools.get(name).cloned().ok_or_else(|| {
        let known: Vec<&str> = list.pools.keys().map(String::as_str).collect();
        pool_err(PoolError::NotAllowed(super::po::tf(
            "storage pool '{name}' is not in the administrator's allowlist ({file}) — declared \
             there: {known}",
            &[
                ("name", name),
                ("file", &file.display().to_string()),
                (
                    "known",
                    &if known.is_empty() {
                        super::po::t("none").to_string()
                    } else {
                        known.join(", ")
                    },
                ),
            ],
        )))
    })?;
    let driver = delonix_storage::registry::driver(&entry.driver).ok_or_else(|| {
        pool_err(PoolError::PoolUnavailable(super::po::tf(
            "storage pool '{name}': driver '{driver}' is not built into this engine",
            &[("name", name), ("driver", &entry.driver)],
        )))
    })?;
    // `mode: in-process` is the administrator saying THIS engine runs the
    // privileged operations itself — which only a root engine can.
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if in_process_without_root(
        list.mode.as_deref(),
        euid,
        driver.required(PoolOp::Allocate(VolumeShape::Filesystem)),
    ) {
        return Err(pool_err(PoolError::HelperUnavailable(super::po::tf(
            "storage pool '{name}': the allowlist says `mode: in-process`, which is for an \
             engine running as root, and this engine is not — enable the storage helper \
             (`mode: helper`) or run the engine as root",
            &[("name", name)],
        ))));
    }
    let driver = delonix_storage::helper::route(driver, &list, helper_client());
    Ok(Resolved {
        name: name.to_string(),
        entry,
        driver,
    })
}

/// `mode: in-process` asks this engine to run a privileged operation itself,
/// which only a root engine can; a driver that needs no privilege is served
/// either way.
fn in_process_without_root(
    mode: Option<&str>,
    euid: u32,
    needs: delonix_storage::pool::Privilege,
) -> bool {
    mode == Some("in-process") && euid != 0 && needs == delonix_storage::pool::Privilege::Helper
}

/// [`resolve`], and the pool answers its probe. An unusable pool is class 69
/// whether the driver knows why or could not find out — and «could not find
/// out» is said as such, never as «nothing there».
fn usable(name: &str) -> Result<Resolved> {
    let r = resolve(name)?;
    match r.driver.probe(&r.pool()) {
        PoolProbe::Available => Ok(r),
        PoolProbe::Unavailable { missing, remedy } => {
            Err(pool_err(PoolError::PoolUnavailable(super::po::tf(
                "storage pool '{name}' cannot be used: {missing} — {remedy}",
                &[("name", name), ("missing", &missing), ("remedy", &remedy)],
            ))))
        }
        PoolProbe::Undetermined { reason, remedy } => {
            Err(pool_err(PoolError::PoolUndetermined(super::po::tf(
                "storage pool '{name}': could not determine its state — {reason}; {remedy}",
                &[("name", name), ("reason", &reason), ("remedy", &remedy)],
            ))))
        }
    }
}

// ---- the record -------------------------------------------------------------

fn dir() -> std::path::PathBuf {
    state_root().join("storage-pools")
}

fn path_in(d: &std::path::Path, name: &str) -> std::path::PathBuf {
    d.join(format!("{name}.json"))
}

/// A record that exists and does not parse is an error, not «no such pool»:
/// reading it as absent would let the next apply replace who owns it.
fn read_in(d: &std::path::Path, name: &str) -> Result<Option<PoolRecord>> {
    let path = path_in(d, name);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Invalid(format!("storagepool {name}: {e}"))),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|e| {
        Error::Invalid(format!(
            "storagepool {name}: {} is damaged ({e}) — restore it or remove it by hand",
            path.display()
        ))
    })
}

fn list_in(d: &std::path::Path) -> Vec<PoolRecord> {
    let mut v: Vec<PoolRecord> = std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

fn write_in(d: &std::path::Path, r: &PoolRecord) -> Result<()> {
    std::fs::create_dir_all(d).map_err(|e| Error::Invalid(format!("storagepool: {e}")))?;
    let json = serde_json::to_vec_pretty(r).map_err(|e| Error::Invalid(e.to_string()))?;
    delonix_state::write_atomic(&path_in(d, &r.name), &json)
        .map_err(|e| Error::Invalid(format!("storagepool: {e}")))
}

pub(crate) fn pool_get(name: &str) -> Option<PoolRecord> {
    read_in(&dir(), name).ok().flatten()
}

/// The registered use of `name`, or the «no such storage pool» refusal.
fn registered(name: &str) -> Result<PoolRecord> {
    read_in(&dir(), name)?.ok_or_else(|| {
        pool_err(PoolError::PoolNotRegistered(super::po::tf(
            "storage pool {name} (apply a `kind: StoragePool` named '{name}' first; `delonix get \
             storagepools` lists what the administrator declared)",
            &[("name", name)],
        )))
    })
}

/// The volumes of THIS engine root that are in `pool`, as `(name, size)`.
fn volumes_in(pool: &str) -> Result<Vec<(String, u64)>> {
    let store = VolumeStore::open(state_root())?;
    Ok(store
        .list_all()?
        .into_iter()
        .filter(|o| o.volume.pool.as_deref() == Some(pool))
        .map(|o| {
            let name = match &o.namespace {
                Some(ns) => format!("{ns}/{}", o.volume.name),
                None => o.volume.name.clone(),
            };
            (name, o.volume.quota_bytes.unwrap_or(0))
        })
        .collect())
}

// ---- the spec ---------------------------------------------------------------

/// Refuses, by name, every field that is the administrator's to set.
pub(crate) fn refuse_admin_fields(doc: &ManifestDoc) -> Result<()> {
    let serde_yaml::Value::Mapping(map) = &doc.spec else {
        return Ok(());
    };
    let found: Vec<&str> = map
        .keys()
        .filter_map(|key| key.as_str())
        .filter(|key| ADMIN_ONLY_FIELDS.contains(key))
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    Err(pool_err(PoolError::InvalidPoolRequest(super::po::tf(
        "StoragePool '{name}': {fields} cannot be set in a manifest — a manifest only NAMES a \
         pool; what backs it is declared by the administrator in {file}",
        &[
            ("name", &doc.metadata.name),
            (
                "fields",
                &found
                    .iter()
                    .map(|f| format!("`{f}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            ("file", &allowlist::path().display().to_string()),
        ],
    ))))
}

/// The spec's two numbers, validated: `(maxRatio, alertPct)`.
fn settings(name: &str, spec: &StoragePoolSpec) -> Result<(f64, u8)> {
    let ratio = spec
        .overcommit
        .as_ref()
        .map_or(DEFAULT_MAX_RATIO, |o| o.max_ratio);
    if !ratio.is_finite() || !(0.01..=100.0).contains(&ratio) {
        return Err(pool_err(PoolError::InvalidPoolRequest(super::po::tf(
            "StoragePool '{name}': overcommit.maxRatio must be between 0.01 and 100 (1.0 allows \
             no over-allocation)",
            &[("name", name)],
        ))));
    }
    let alert = spec.alert_pct.unwrap_or(DEFAULT_ALERT_PCT);
    if !(1..=99).contains(&alert) {
        return Err(pool_err(PoolError::InvalidPoolRequest(super::po::tf(
            "StoragePool '{name}': alertPct must be between 1 and 99",
            &[("name", name)],
        ))));
    }
    Ok((ratio, alert))
}

fn spec_of(doc: &ManifestDoc) -> Result<StoragePoolSpec> {
    refuse_admin_fields(doc)?;
    if !allowlist::valid_pool_name(&doc.metadata.name) {
        return Err(pool_err(PoolError::InvalidPoolRequest(super::po::tf(
            "StoragePool '{name}': a pool name is lower-case letters, digits, '.', '_' or '-', \
             starting with a letter or digit",
            &[("name", &doc.metadata.name)],
        ))));
    }
    if doc.spec.is_null() {
        return Ok(StoragePoolSpec::default());
    }
    manifest::spec_of(doc)
}

/// One canonical spelling of a ratio, so the manifest and the record compare
/// equal: `1` and `1.0` are the same ceiling.
fn fmt_ratio(r: f64) -> String {
    format!("{r}")
}

fn fields_of(ratio: f64, alert: u8) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("maxRatio".into(), fmt_ratio(ratio));
    f.insert("alertPct".into(), alert.to_string());
    f
}

// ---- the Kind ---------------------------------------------------------------

pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec = spec_of(doc)?;
    let (ratio, alert) = settings(&doc.metadata.name, &spec)?;
    Ok(super::reconcile::Desired {
        kind: k::STORAGE_POOL.into(),
        name: doc.metadata.name.clone(),
        fields: fields_of(ratio, alert),
        converges: true,
        ownable: true,
    })
}

pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    Ok(list_in(&dir())
        .into_iter()
        .map(|r| super::reconcile::Actual {
            kind: k::STORAGE_POOL.into(),
            fields: fields_of(r.max_ratio, r.alert_pct),
            owner: r.labels.get(super::reconcile::STACK_LABEL).cloned(),
            last_applied: r
                .annotations
                .get(super::reconcile::LAST_APPLIED)
                .and_then(|raw| super::reconcile::decode_last_applied(raw)),
            name: r.name,
        })
        .collect())
}

pub(crate) fn stamp(name: &str, stack: &str, fields: &BTreeMap<String, String>) -> Result<()> {
    let d = dir();
    let mut r =
        read_in(&d, name)?.ok_or_else(|| Error::NotFound(format!("storagepool: {name}")))?;
    r.labels
        .insert(super::reconcile::STACK_LABEL.into(), stack.into());
    r.labels
        .insert(super::reconcile::MANAGED_BY.into(), "delonix".into());
    r.annotations.insert(
        super::reconcile::LAST_APPLIED.into(),
        super::reconcile::encode_last_applied(fields),
    );
    write_in(&d, &r)
}

/// Stops using a pool. **Never touches the pool**: it is the administrator's,
/// and so is everything in it. Refused while this engine still has volumes
/// there — their records would point into a pool the engine no longer admits
/// to using, and nothing could release them.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let d = dir();
    if read_in(&d, name)?.is_none() {
        return Ok(());
    }
    let held = volumes_in(name)?;
    if !held.is_empty() {
        let names: Vec<&str> = held.iter().map(|(n, _)| n.as_str()).collect();
        return Err(pool_err(PoolError::PoolConflict(super::po::tf(
            "storage pool '{name}' still holds {n} volume(s) of this engine ({list}) — remove \
             them first (`delonix volume rm`); the pool itself is never deleted",
            &[
                ("name", name),
                ("n", &held.len().to_string()),
                ("list", &names.join(", ")),
            ],
        ))));
    }
    std::fs::remove_file(path_in(&d, name))
        .map_err(|e| Error::Invalid(format!("storagepool {name}: {e}")))
}

fn apply_one(doc: &ManifestDoc) -> Result<()> {
    let name = &doc.metadata.name;
    let spec = spec_of(doc)?;
    let (ratio, alert) = settings(name, &spec)?;
    let r = usable(name)?;
    let d = dir();
    let mut rec = read_in(&d, name)?.unwrap_or_else(|| PoolRecord {
        name: name.clone(),
        max_ratio: ratio,
        alert_pct: alert,
        created_unix: delonix_node::now_unix(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
    });
    rec.max_ratio = ratio;
    rec.alert_pct = alert;
    write_in(&d, &rec)?;
    let usage = r.driver.usage(&r.pool());
    println!(
        "{}",
        super::po::tf(
            "storagepool/{name}: {driver}, {capacity} capacity, {used} used",
            &[
                ("name", name),
                ("driver", r.driver.id()),
                ("capacity", &fmt_bytes(usage.capacity_bytes)),
                ("used", &fmt_pct(usage.used_pct())),
            ],
        )
    );
    Ok(())
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::STORAGE_POOL) {
        apply_one(doc)?;
    }
    Ok(())
}

pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    apply_one(doc)
}

pub(crate) fn presence_of(doc: &ManifestDoc) -> (String, String) {
    let name = &doc.metadata.name;
    if pool_get(name).is_none() {
        return ("no".into(), "-".into());
    }
    ("yes".into(), state_of(name).0.to_string())
}

pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec = spec_of(doc)?;
    let (ratio, alert) = settings(&doc.metadata.name, &spec)?;
    let full = StoragePoolSpec {
        overcommit: Some(Overcommit { max_ratio: ratio }),
        alert_pct: Some(alert),
    };
    serde_yaml::to_value(full).map_err(|e| Error::Invalid(format!("dry-run: {e}")))
}

// ---- volumes in a pool ------------------------------------------------------

/// Refuses an allocation the pool's ceiling does not allow. **Pure**: the
/// numbers come in already measured.
///
/// `others` is the sum of the sizes already allocated to OTHER volumes; `new`
/// says the volume does not exist yet. A volume that already exists is never
/// refused for the pool being full — it is already in it.
fn check_capacity(
    pool: &str,
    volume: &str,
    size: u64,
    others: u64,
    new: bool,
    usage: &PoolUsage,
    max_ratio: f64,
) -> std::result::Result<(), PoolError> {
    if new {
        // A thin pool's metadata fills on its own, and a full metadata area
        // fails every thin volume in it as surely as full data does.
        if let Some(pct) = usage.metadata_pct {
            if pct >= REFUSE_ABOVE_PCT {
                return Err(PoolError::PoolExhausted(super::po::tf(
                    "storage pool '{pool}': its metadata is {pct}% full — no new volume is \
                     allocated above {limit}%",
                    &[
                        ("pool", pool),
                        ("pct", &pct.to_string()),
                        ("limit", &REFUSE_ABOVE_PCT.to_string()),
                    ],
                )));
            }
        }
        if let Some(pct) = usage.used_pct() {
            if pct >= REFUSE_ABOVE_PCT {
                return Err(PoolError::PoolExhausted(super::po::tf(
                    "storage pool '{pool}' is {pct}% full — no new volume is allocated above \
                     {limit}% (a full pool fails every volume in it)",
                    &[
                        ("pool", pool),
                        ("pct", &pct.to_string()),
                        ("limit", &REFUSE_ABOVE_PCT.to_string()),
                    ],
                )));
            }
        }
    }
    // An unmeasured capacity cannot be exceeded on paper; the driver's own
    // allocation is what refuses then.
    let Some(capacity) = usage.capacity_bytes else {
        return Ok(());
    };
    let ceiling = (capacity as f64 * max_ratio) as u64;
    let wanted = others.saturating_add(size);
    if wanted > ceiling {
        return Err(PoolError::PoolExhausted(super::po::tf(
            "storage pool '{pool}': volume '{volume}' of {size} would bring the allocated total \
             to {wanted}, above the ceiling of {ceiling} ({capacity} capacity × \
             overcommit.maxRatio {ratio})",
            &[
                ("pool", pool),
                ("volume", volume),
                ("size", &output::fmt_size(size)),
                ("wanted", &output::fmt_size(wanted)),
                ("ceiling", &output::fmt_size(ceiling)),
                ("capacity", &output::fmt_size(capacity)),
                ("ratio", &fmt_ratio(max_ratio)),
            ],
        )));
    }
    Ok(())
}

/// Ensures volume `name` of `size` exists in `pool`, and returns its record.
///
/// The order is: every refusal first (the pool is registered, the allowlist
/// permits it, the pool answers, the size fits), then the driver allocates —
/// stamping the object in the pool — and only then the volume's record is
/// written. An apply that dies in between finds the stamped object again and
/// takes it back instead of making a second.
pub(crate) fn ensure_volume(
    store: &VolumeStore,
    name: &str,
    pool: &str,
    size: &str,
    alert_pct: Option<u8>,
) -> Result<delonix_volume::Volume> {
    let size_bytes = delonix_volume::parse_size_bytes(size)
        .filter(|b| *b > 0)
        .ok_or_else(|| {
            pool_err(PoolError::InvalidPoolRequest(super::po::tf(
                "volume '{name}': size '{size}' is not a size (e.g. 10G)",
                &[("name", name), ("size", size)],
            )))
        })?;
    let rec = registered(pool)?;
    let existing = store.inspect(name).ok();
    if let Some(v) = &existing {
        if v.pool.as_deref() != Some(pool) {
            return Err(pool_err(PoolError::InvalidPoolRequest(super::po::tf(
                "volume '{name}' already exists and is not in storage pool '{pool}' — a volume \
                 never moves; remove it or pick another name",
                &[("name", name), ("pool", pool)],
            ))));
        }
    }
    let r = usable(pool)?;
    if let Some(max) = r
        .entry
        .max_volume_bytes(pool, delonix_volume::parse_size_bytes)
        .map_err(pool_err)?
    {
        if size_bytes > max {
            return Err(pool_err(PoolError::NotAllowed(super::po::tf(
                "volume '{name}': {size} is above the {max} the administrator allows for one \
                 volume of storage pool '{pool}' (maxVolumeBytes)",
                &[
                    ("name", name),
                    ("size", &output::fmt_size(size_bytes)),
                    ("max", &output::fmt_size(max)),
                    ("pool", pool),
                ],
            ))));
        }
    }
    let others: u64 = volumes_in(pool)?
        .into_iter()
        .filter(|(n, _)| n != name)
        .map(|(_, s)| s)
        .sum();
    let usage = r.driver.usage(&r.pool());
    check_capacity(
        pool,
        name,
        size_bytes,
        others,
        existing.is_none(),
        &usage,
        rec.max_ratio,
    )
    .map_err(pool_err)?;
    if let Some(pct) = usage.used_pct() {
        if pct >= rec.alert_pct {
            output::warn(&super::po::tf(
                "storage pool '{pool}' is {pct}% full (alert at {alert}%)",
                &[
                    ("pool", pool),
                    ("pct", &pct.to_string()),
                    ("alert", &rec.alert_pct.to_string()),
                ],
            ));
        }
    }
    let alloc = r
        .driver
        .allocate(
            &r.pool(),
            &VolumeRequest {
                name: name.to_string(),
                shape: VolumeShape::Filesystem,
                size_bytes,
            },
            &owner(),
        )
        .map_err(pool_err)?;
    // An idempotent re-apply must not reset a threshold nobody mentioned.
    let alert = alert_pct.or(existing.as_ref().and_then(|v| v.alert_pct));
    let vol = store.register_in_pool(name, &alloc.path, size_bytes, alert, pool)?;
    if existing.is_none() {
        if alloc.adopted {
            output::warn(&super::po::tf(
                "volume '{name}': found in storage pool '{pool}' with this engine's stamp and no \
                 record — taken back, with the data it holds",
                &[("name", name), ("pool", pool)],
            ));
        }
        delonix_node::events::emit(
            &state_root(),
            "volume",
            "create",
            &vol.name,
            &vol.name,
            Some(&format!("pool={pool}")),
        );
    }
    Ok(vol)
}

/// Gives a volume's data back to its pool — the data and the stamp go. Called
/// BEFORE the volume's record is removed: the record is what says which pool
/// holds it.
pub(crate) fn release_volume(vol: &delonix_volume::Volume) -> Result<()> {
    let Some(pool) = vol.pool.as_deref() else {
        return Ok(());
    };
    let r = resolve(pool)?;
    r.driver
        .release(&r.pool(), &vol.name, &owner())
        .map_err(pool_err)
}

// ---- get / describe ---------------------------------------------------------

/// `(label, detail)` of a pool's state right now.
fn state_of(name: &str) -> (&'static str, String) {
    match resolve(name) {
        Err(e) => ("UNAVAILABLE", e.to_string()),
        Ok(r) => match r.driver.probe(&r.pool()) {
            PoolProbe::Available => ("AVAILABLE", String::new()),
            p @ PoolProbe::Unavailable { .. } => (p.label(), probe_detail(&p)),
            p @ PoolProbe::Undetermined { .. } => (p.label(), probe_detail(&p)),
        },
    }
}

fn probe_detail(p: &PoolProbe) -> String {
    match p {
        PoolProbe::Available => String::new(),
        PoolProbe::Unavailable { missing, remedy } => format!("{missing} — {remedy}"),
        PoolProbe::Undetermined { reason, remedy } => format!("{reason} — {remedy}"),
    }
}

fn fmt_bytes(b: Option<u64>) -> String {
    b.map(output::fmt_size).unwrap_or_else(|| "-".into())
}

fn fmt_pct(p: Option<u8>) -> String {
    p.map(|p| format!("{p}%")).unwrap_or_else(|| "-".into())
}

#[derive(Serialize)]
struct LsRow {
    name: String,
    /// `false`: the administrator declared it and no `kind: StoragePool` uses it yet.
    in_use: bool,
    driver: Option<String>,
    state: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    detail: String,
    capacity_bytes: Option<u64>,
    used_bytes: Option<u64>,
    allocated_bytes: u64,
    volumes: usize,
    max_ratio: Option<f64>,
    stack: Option<String>,
}

/// One row per pool: every pool this engine uses, and every pool the
/// administrator declared that it does not use yet. A listing with only the
/// first would not say what a manifest may name.
fn rows() -> Result<Vec<LsRow>> {
    register_drivers();
    let records = list_in(&dir());
    // An allowlist that cannot be read is said once, on stderr; the pools in
    // use are still listed, each with the reason it is unavailable.
    let declared = match allowlist::load(decode_allowlist) {
        Ok(l) => l.unwrap_or_default().pools,
        Err(e) => {
            output::warn(&Error::from(e).to_string());
            BTreeMap::new()
        }
    };
    let mut names: Vec<String> = records.iter().map(|r| r.name.clone()).collect();
    names.extend(declared.keys().cloned());
    names.sort();
    names.dedup();
    let mut out = Vec::new();
    for name in names {
        let rec = records.iter().find(|r| r.name == name);
        let entry = declared.get(&name);
        let (state, detail) = state_of(&name);
        // The same routed driver every other operation uses: a pool whose
        // usage needs the helper is measured through it, never read as empty.
        let usage = match resolve(&name) {
            Ok(r) if entry.is_some() && state == "AVAILABLE" => r.driver.usage(&r.pool()),
            _ => PoolUsage::default(),
        };
        let held = volumes_in(&name)?;
        out.push(LsRow {
            in_use: rec.is_some(),
            driver: entry.map(|e| e.driver.clone()),
            state: state.to_string(),
            detail,
            capacity_bytes: usage.capacity_bytes,
            used_bytes: usage.used_bytes,
            allocated_bytes: held.iter().map(|(_, s)| s).sum(),
            volumes: held.len(),
            max_ratio: rec.map(|r| r.max_ratio),
            stack: rec.and_then(|r| r.labels.get(super::reconcile::STACK_LABEL).cloned()),
            name,
        });
    }
    Ok(out)
}

pub(crate) fn cmd_ls(format: OutputFormat) -> Result<()> {
    let format = super::config::resolve_output(&state_root(), format);
    let rows = rows()?;
    if format == OutputFormat::Json {
        return output::print_json(&rows);
    }
    let mut t = output::Table::new(&[
        "NAME",
        "DRIVER",
        "STATE",
        "IN USE",
        "CAPACITY",
        "USED",
        "ALLOCATED",
        "VOLUMES",
        "STACK",
    ]);
    for r in &rows {
        t.row(vec![
            r.name.clone(),
            r.driver.clone().unwrap_or_else(|| "-".into()),
            r.state.clone(),
            super::po::t(if r.in_use { "yes" } else { "no" }).to_string(),
            fmt_bytes(r.capacity_bytes),
            fmt_bytes(r.used_bytes),
            output::fmt_size(r.allocated_bytes),
            r.volumes.to_string(),
            r.stack.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    t.print();
    Ok(())
}

pub(crate) fn cmd_describe(names: &[String]) -> Result<()> {
    let rows = rows()?;
    for name in names {
        let Some(r) = rows.iter().find(|r| &r.name == name) else {
            return Err(pool_err(PoolError::PoolNotRegistered(format!(
                "storage pool {name}"
            ))));
        };
        let mut d = output::Describe::new();
        d.field("Name", &r.name);
        d.field("Driver", r.driver.as_deref().unwrap_or("-"));
        d.field("State", &r.state);
        if !r.detail.is_empty() {
            d.field("Reason", &r.detail);
        }
        d.field(
            "In use",
            super::po::t(if r.in_use {
                "yes"
            } else {
                "no (declared by the administrator; apply a kind: StoragePool to use it)"
            }),
        );
        d.field("Capacity", fmt_bytes(r.capacity_bytes));
        d.field("Used", fmt_bytes(r.used_bytes));
        d.field("Allocated", output::fmt_size(r.allocated_bytes));
        if let Some(ratio) = r.max_ratio {
            d.field("Overcommit", fmt_ratio(ratio));
        }
        let held = volumes_in(name)?;
        d.field(
            "Volumes",
            if held.is_empty() {
                "<none>".to_string()
            } else {
                held.iter()
                    .map(|(n, s)| format!("{n} ({})", output::fmt_size(*s)))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        );
        d.field_opt("Stack", r.stack.as_deref());
        d.print();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(spec: &str) -> ManifestDoc {
        let text = format!(
            "apiVersion: storage.delonix.io/v1alpha1\nkind: StoragePool\nmetadata: {{ name: media }}\nspec: {spec}\n"
        );
        serde_yaml::from_str(&text).unwrap()
    }

    #[test]
    fn a_manifest_that_names_a_device_a_path_or_a_command_is_refused_by_name() {
        for field in ADMIN_ONLY_FIELDS {
            let e = desired(&doc(&format!("{{ {field}: x }}"))).unwrap_err();
            assert_eq!(e.number(), 1217, "{field}: {e}");
            assert!(
                e.to_string().contains(&format!("`{field}`")),
                "{field}: {e}"
            );
        }
        // Several at once are all named: fixing them one refusal at a time is
        // one apply per field.
        let e = desired(&doc("{ devices: [/dev/sdb], path: /srv }")).unwrap_err();
        let text = e.to_string();
        assert!(
            text.contains("`devices`") && text.contains("`path`"),
            "{text}"
        );
    }

    #[test]
    fn the_defaults_are_no_over_allocation_and_an_alert_at_80() {
        let d = desired(&doc("{}")).unwrap();
        assert_eq!(d.fields["maxRatio"], "1");
        assert_eq!(d.fields["alertPct"], "80");
        let d = desired(&doc("{ overcommit: { maxRatio: 1.5 }, alertPct: 70 }")).unwrap();
        assert_eq!(d.fields["maxRatio"], "1.5");
        assert_eq!(d.fields["alertPct"], "70");
        // `1` and `1.0` are one ceiling, or every plan would show a difference.
        assert_eq!(
            desired(&doc("{ overcommit: { maxRatio: 1 } }"))
                .unwrap()
                .fields,
            desired(&doc("{ overcommit: { maxRatio: 1.0 } }"))
                .unwrap()
                .fields
        );
    }

    #[test]
    fn a_ratio_or_an_alert_out_of_range_is_refused() {
        for bad in [
            "{ overcommit: { maxRatio: 0 } }",
            "{ overcommit: { maxRatio: -1 } }",
            "{ overcommit: { maxRatio: 1000 } }",
            "{ alertPct: 0 }",
            "{ alertPct: 100 }",
        ] {
            assert_eq!(desired(&doc(bad)).unwrap_err().number(), 1217, "{bad}");
        }
    }

    #[test]
    fn every_compared_field_is_one_the_record_gives_back() {
        let d = desired(&doc("{}")).unwrap();
        let mut keys: Vec<&str> = d.fields.keys().map(String::as_str).collect();
        keys.sort();
        let mut want = RECONCILED_STORAGE_POOL_FIELDS.to_vec();
        want.sort();
        assert_eq!(keys, want);
    }

    const GIB: u64 = 1 << 30;

    fn usage(cap: u64, used: u64) -> PoolUsage {
        PoolUsage {
            capacity_bytes: Some(cap),
            used_bytes: Some(used),
            metadata_pct: None,
        }
    }

    #[test]
    fn an_allocation_above_the_ceiling_is_refused_and_one_at_it_is_not() {
        let u = usage(100 * GIB, 10 * GIB);
        assert!(check_capacity("p", "v", 40 * GIB, 60 * GIB, true, &u, 1.0).is_ok());
        let e = check_capacity("p", "v", 41 * GIB, 60 * GIB, true, &u, 1.0).unwrap_err();
        assert!(matches!(e, PoolError::PoolExhausted(_)), "{e}");
        // The ratio is what moves the ceiling.
        assert!(check_capacity("p", "v", 41 * GIB, 60 * GIB, true, &u, 1.5).is_ok());
        assert!(check_capacity("p", "v", 20 * GIB, 60 * GIB, true, &u, 0.5).is_err());
    }

    #[test]
    fn a_full_pool_refuses_a_new_volume_and_not_one_already_in_it() {
        let full = usage(100 * GIB, 96 * GIB);
        let e = check_capacity("p", "v", GIB, 0, true, &full, 1.0).unwrap_err();
        assert!(e.to_string().contains("96% full"), "{e}");
        assert!(check_capacity("p", "v", GIB, 0, false, &full, 1.0).is_ok());
        // 94.x% rounds up to 95 and is refused; the limit is not a rounding accident.
        assert!(check_capacity("p", "v", GIB, 0, true, &usage(1000, 941), 100.0).is_err());
        assert!(check_capacity("p", "v", 1, 0, true, &usage(1000, 940), 100.0).is_ok());
    }

    #[test]
    fn a_thin_pool_whose_metadata_is_full_takes_no_new_volume() {
        let mut u = usage(100 * GIB, 10 * GIB);
        u.metadata_pct = Some(95);
        let e = check_capacity("p", "v", GIB, 0, true, &u, 1.0).unwrap_err();
        assert!(e.to_string().contains("metadata is 95% full"), "{e}");
        assert!(check_capacity("p", "v", GIB, 0, false, &u, 1.0).is_ok());
        u.metadata_pct = Some(94);
        assert!(check_capacity("p", "v", GIB, 0, true, &u, 1.0).is_ok());
    }

    #[test]
    fn in_process_mode_is_refused_to_a_non_root_engine_for_a_privileged_driver() {
        use delonix_storage::pool::Privilege;
        assert!(in_process_without_root(
            Some("in-process"),
            1000,
            Privilege::Helper
        ));
        assert!(!in_process_without_root(
            Some("in-process"),
            0,
            Privilege::Helper
        ));
        assert!(!in_process_without_root(
            Some("in-process"),
            1000,
            Privilege::Unprivileged
        ));
        assert!(!in_process_without_root(
            Some("helper"),
            1000,
            Privilege::Helper
        ));
        assert!(!in_process_without_root(None, 1000, Privilege::Helper));
    }

    #[test]
    fn every_pool_driver_the_allowlist_accepts_is_registered() {
        register_drivers();
        for (id, _) in delonix_storage::allowlist::DRIVERS {
            assert!(
                delonix_storage::registry::driver(id).is_some(),
                "driver '{id}' is accepted by the allowlist and not registered"
            );
        }
    }

    #[test]
    fn an_unmeasured_capacity_is_not_read_as_zero() {
        let unknown = PoolUsage::default();
        assert!(check_capacity("p", "v", 1000 * GIB, 0, true, &unknown, 1.0).is_ok());
    }
}

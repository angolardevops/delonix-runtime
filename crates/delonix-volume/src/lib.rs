//! `delonix-volume` — named volumes and *bind mounts* for the Delonix Engine.
//!
//! Two kinds of mount, both **zero-copy** (the kernel shares the blocks via
//! `MS_BIND`, there is no data copy):
//! - **named volume**: a directory managed by Delonix at
//!   `<root>/volumes/<name>/_data`, which **survives** the container;
//! - **bind mount**: an arbitrary host path, mounted into the container.
//!
//! The `-v` syntax follows Docker: `name:/target` (volume) or
//! `/host/path:/target` (bind), with an optional `:ro` for read-only.

use delonix_runtime_core::{write_atomic, Error, Mount, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Metadata of a named volume.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Volume {
    /// The volume name.
    pub name: String,
    /// The data directory on the host (`.../_data`).
    pub mountpoint: String,
    /// Creation instant (Unix seconds).
    pub created_unix: u64,
    /// Driver: `local` (default) or `nfs` (external TrueNAS/NFS).
    #[serde(default = "default_driver")]
    pub driver: String,
    /// For `nfs`: the *export* (`server:/path`).
    #[serde(default)]
    pub device: Option<String>,
    /// Mount options (`mount -o ...`), e.g.: `vers=4,ro`.
    #[serde(default)]
    pub options: Option<String>,
    /// Size quota in bytes (`--quota`). `None` = no limit. With privilege
    /// (root model) it is a HARD cap via a loop-mounted ext4 image; in rootless it is
    /// a MONITORED limit (measured usage, alert near the limit). [[hybrid #7]]
    #[serde(default)]
    pub quota_bytes: Option<u64>,
    /// Usage percentage above which an alert is raised (default 90).
    #[serde(default)]
    pub alert_pct: Option<u8>,
    /// Free labels (k8s style) — short, identifying, safe to show in a listing.
    /// A declarative `apply` stamps ownership here (`delonix.io/stack=<name>`),
    /// which is what makes `stack destroy`/`--prune` possible without inventing a
    /// registry of stacks that would drift out of sync. `#[serde(default)]` so
    /// every `meta.json` already on disk keeps deserializing.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Free annotations — the same idea, for data that is NOT identifying and may
    /// be large (the reconciler keeps the last applied spec under
    /// `delonix.io/last-applied` here, never in `labels`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    /// The volume this one is CARVED OUT OF, for a share: a real subdirectory of
    /// another volume's mountpoint, with its own name, quota and consumers
    /// (`kind: Volume` with a `share:` block; `delonix sharevolume`).
    ///
    /// It lives here, and not in a registry beside this one, because everything
    /// else about a share — mountpoint, quota, alert threshold, creation time —
    /// was ALREADY duplicated into a second record whose only unique field was
    /// this one. Two records for one object is how the two start disagreeing,
    /// and it is why a share could not be owned by a stack: ownership is stamped
    /// in `labels`, which only this record has.
    ///
    /// `#[serde(default)]`, so every `meta.json` already on disk keeps
    /// deserializing as what it is — a volume that is nobody's subdirectory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// The drivers that mount a network share (as opposed to `local`/loopback).
pub fn is_network_driver(driver: &str) -> bool {
    matches!(driver, "nfs" | "cifs" | "smb" | "webdav" | "dav")
}

/// The `mount` `-t <fstype>` for each network driver. `smb` is an alias of
/// `cifs` (the kernel only knows `cifs`); `dav` of `webdav` (`davfs`).
fn mount_fstype(driver: &str) -> &'static str {
    match driver {
        "cifs" | "smb" => "cifs",
        "webdav" | "dav" => "davfs",
        _ => "nfs",
    }
}

fn default_driver() -> String {
    "local".to_string()
}

/// Human size (`512m`, `2g`, `10G`, `1048576`) → bytes. Binary suffixes
/// (k=1024, m=1024², g=1024³, t=1024⁴); a trailing `b`/`B` is accepted. `None` if invalid.
pub fn parse_size_bytes(s: &str) -> Option<u64> {
    let lower = s.trim().to_lowercase();
    let body = lower.strip_suffix('b').unwrap_or(lower.as_str());
    let (num, mult) = match body.chars().last() {
        Some('k') => (&body[..body.len() - 1], 1024u64),
        Some('m') => (&body[..body.len() - 1], 1024 * 1024),
        Some('g') => (&body[..body.len() - 1], 1024 * 1024 * 1024),
        Some('t') => (&body[..body.len() - 1], 1024u64.pow(4)),
        _ => (body, 1),
    };
    let n: f64 = num.trim().parse().ok()?;
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    // BUG FIXED HERE: `as u64` on an f64 is a SATURATING cast in Rust, so
    // `--quota 99999999999t` silently produced `u64::MAX` — a quota that
    // `inspect`/`describe` print as genuinely SET (`quota_bytes:
    // 18446744073709551615`) but that no volume can ever reach, i.e. no quota
    // at all. A value that does not fit is an input error, never a silent clamp
    // to "unlimited" (the exact opposite of what the operator asked for).
    let bytes = n * mult as f64;
    if bytes >= u64::MAX as f64 {
        return None;
    }
    Some(bytes as u64)
}

/// The result of measuring a directory tree: the bytes actually SEEN, plus how
/// many directories could not be read at all.
///
/// The second field is the whole point. `dir_usage` used to swallow every
/// `read_dir` error and return a bare `0`, which is indistinguishable from an
/// empty volume — and in rootless that is the NORMAL case, not an edge case: a
/// container in a mapped userns writes `_data` as a SUBUID, and anything that
/// `chmod 700`s its data dir (Postgres, Odoo, MySQL — every managed database)
/// becomes unreadable to the real user. The measured consequences were a
/// `describe` reporting `Usage: 0 B` over 20 MiB of real data, a `system df`
/// reporting `volumes 0 B` on a filling disk, and a rootless quota — documented
/// as a MONITORED limit, the only enforcement rootless has — that could never
/// fire. Callers must treat an incomplete measurement as *unknown*, never as
/// zero: see [`Usage::is_complete`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Bytes of every file the walk could actually stat.
    pub bytes: u64,
    /// Directories the calling uid could not `read_dir` (EACCES and friends).
    /// Non-zero → `bytes` is a LOWER BOUND, not the real usage.
    pub unreadable: u64,
}

impl Usage {
    /// `true` when every directory in the tree was readable, so `bytes` is the
    /// real usage rather than a floor.
    pub fn is_complete(&self) -> bool {
        self.unreadable == 0
    }
}

/// Quota verdict for a measured tree. `measured: false` means the walk hit
/// directories it could not read, so `in_alert`/`above_quota` are **unknown**
/// (not `false`) — the caller has to say so instead of implying compliance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuotaState {
    pub in_alert: bool,
    pub above_quota: bool,
    pub measured: bool,
}

/// Measures any tree, reporting whether the walk was complete — the free
/// function behind every `usage*` method, exposed so the mapped-userns helper
/// (`__duusage`) can reuse the exact same walk instead of a second copy that
/// could drift from it.
pub fn measure(path: &std::path::Path) -> Usage {
    dir_usage(path)
}

/// Persists a volume's `meta.json` atomically and durably.
///
/// BUG FIXED HERE: the three call sites used a bare `fs::write`, which is
/// neither. `fs::write` TRUNCATES the existing file and then writes — so a
/// crash, a full disk, or an EIO partway through leaves a **truncated
/// meta.json**, and a truncated `meta.json` does not deserialize. `list()`
/// silently skips every volume whose metadata fails to parse, and `inspect()`
/// reports `NotFound`: the volume vanishes from `volumes ls`, from `system df`,
/// and from the quota checks — while every byte of its data is still on disk.
///
/// That is precisely the shape of the cross-tenant leak this crate already
/// fixed once from the other direction (see [`VolumeStore::remove_with`]): a
/// volume that no longer exists as far as the engine is concerned, whose NAME
/// is therefore free for the next `create` to take, handing the previous
/// owner's data to whoever mounts it next.
fn write_meta(path: &std::path::Path, vol: &Volume) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(vol)?)
}

/// A [`Volume`] together with the namespace that owns it.
///
/// The pair exists because the owner is NOT a field of `Volume`: it is the
/// on-disk location (`volumes/.ns/<namespace>/<name>`). Reading it back from
/// the path at each use is how two callers end up with two different answers,
/// so it is read once, here, and travels with the record.
#[derive(Clone, Debug)]
pub struct OwnedVolume {
    /// `None` = the unscoped root (no owner), NOT the `default` namespace.
    pub namespace: Option<String>,
    pub volume: Volume,
}

impl OwnedVolume {
    /// How an operator names this owner on the command line and in a report.
    pub fn owner(&self) -> &str {
        self.namespace.as_deref().unwrap_or(Self::NO_OWNER)
    }

    /// The label for a volume in the unscoped root. Not a namespace name —
    /// `valid_name` refuses the brackets, so it can never collide with one.
    pub const NO_OWNER: &'static str = "<none>";
}

/// The volume store, under `<root>/volumes`.
pub struct VolumeStore {
    root: PathBuf,
}

impl VolumeStore {
    /// Opens (creating) the volume store.
    pub fn open(base: impl Into<PathBuf>) -> Result<Self> {
        let root = base.into().join("volumes");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Sub-tree reserved for namespace-scoped volumes: `<root>/volumes/.ns/`.
    ///
    /// `.ns` cannot collide with a volume, ever — `valid_name` refuses a leading `.`. That
    /// is the whole reason for the odd-looking name: every other separator that came to
    /// mind (`-`, `_`, `.` inside the name) is a LEGAL character in a volume name, so any
    /// `<ns><sep><name>` encoding is ambiguous. It is the same collision class the compose
    /// project names already had to solve, and a directory boundary sidesteps it instead of
    /// encoding around it.
    const NS_SUBTREE: &'static str = ".ns";

    /// A store rooted at ONE namespace's sub-tree, so every existing method
    /// (`register_external`, `inspect`, `remove`, quotas) works on it unchanged.
    pub fn open_scoped(base: impl Into<PathBuf>, namespace: &str) -> Result<Self> {
        let root = base
            .into()
            .join("volumes")
            .join(Self::NS_SUBTREE)
            .join(namespace);
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The namespaces that hold at least one scoped volume, from the UNSCOPED store.
    ///
    /// Exists so a caller can walk every namespace without knowing the on-disk
    /// layout — the sub-tree name is private precisely so it can change. A
    /// listing that has to enumerate all shares is the reason: one that shows a
    /// single namespace is how an operator deletes a parent volume believing
    /// nothing hangs off it.
    pub fn namespaces(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(self.root.join(Self::NS_SUBTREE)) else {
            return out;
        };
        for e in rd.flatten() {
            if e.path().is_dir() {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        out.sort();
        out
    }

    /// A store rooted at ONE namespace's sub-tree of THIS store.
    ///
    /// [`Self::open_scoped`] does the same from a state root; this one does it
    /// from a store already open, which is what a sweep that walks every
    /// namespace needs — it holds the unscoped store and must reach each
    /// owner's sub-tree without re-deriving the base path (re-deriving it is
    /// how a caller ends up one directory off and reports an empty namespace).
    pub fn scoped(&self, namespace: &str) -> Result<Self> {
        let root = self.root.join(Self::NS_SUBTREE).join(namespace);
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Every volume in this store INCLUDING the namespace-scoped sub-trees,
    /// each one tagged with the namespace that owns it.
    ///
    /// [`Self::list`] deliberately reads only the direct children of the root,
    /// so a namespaced volume is invisible to it. That is correct for a listing
    /// scoped to one tenant and WRONG for anything that has to account for the
    /// whole store: a reclaim sweep built on `list` walks past every tenant's
    /// data and reports the store as clean. That is not hypothetical — it is
    /// why a tenant's volumes survived every `prune` ever run on a host and
    /// were 46 of the 141 GB eventually removed by hand.
    ///
    /// `namespace: None` means the unscoped root — a volume with no owner, not
    /// a volume owned by `default`. The two are different on disk and the
    /// distinction is what lets `--namespace default` mean one of them.
    pub fn list_all(&self) -> Result<Vec<OwnedVolume>> {
        let mut out: Vec<OwnedVolume> = self
            .list()?
            .into_iter()
            .map(|volume| OwnedVolume {
                namespace: None,
                volume,
            })
            .collect();
        for ns in self.namespaces() {
            // A namespace that cannot be opened is REPORTED, never skipped: a
            // sweep that silently drops an owner is the failure this function
            // exists to end.
            let scoped = self.scoped(&ns)?;
            out.extend(scoped.list()?.into_iter().map(|volume| OwnedVolume {
                namespace: Some(ns.clone()),
                volume,
            }));
        }
        Ok(out)
    }

    /// Whether a namespace-scoped volume of this name exists, seen from the UNSCOPED store.
    fn scoped_exists(&self, namespace: &str, name: &str) -> bool {
        self.root
            .join(Self::NS_SUBTREE)
            .join(namespace)
            .join(name)
            .join("meta.json")
            .exists()
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// The root directory of a volume (`<root>/volumes/<name>`) — for recovery
    /// operations in the CLI (e.g.: rm of orphans/subuids in a mapped userns).
    pub fn volume_dir(&self, name: &str) -> PathBuf {
        self.dir(name)
    }
    fn data_dir(&self, name: &str) -> PathBuf {
        self.dir(name).join("_data")
    }
    fn meta_path(&self, name: &str) -> PathBuf {
        self.dir(name).join("meta.json")
    }

    // BUG FIXED HERE (CRITICAL, found live by adversarial review): this charset
    // check alone accepts "." and ".." — a name made ENTIRELY of dots is still
    // "every char is alnum/-/_/.". `register_external`'s caller (`kind:
    // ShareVolume`) joins this name onto a parent path
    // (`<storage>/_data/shares/<name>`) WITHOUT normalizing — a name of ".."
    // resolves, at the OS level, to the PARENT's own `_data` directory itself:
    // total isolation bypass, and `sharevolume rm --purge-data` on it recursively
    // deletes the parent Storage's entire data. Same whitelist shape as
    // `delonix_vm::valid_vm_name` (doesn't start with `-`/`.`, never `..`) closes
    // this for every caller of this store, not just ShareVolume.
    fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && !name.starts_with('-')
            && !name.starts_with('.')
            && !name.contains("..")
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    }

    /// Creates a `local` volume (idempotent: returns the existing one if it already exists).
    pub fn create(&self, name: &str) -> Result<Volume> {
        if self.meta_path(name).exists() {
            return self.inspect(name); // preserves the driver/device of an already-created volume
        }
        self.create_with(name, "local", None, None)
    }

    /// Registers a volume whose data lives at an EXTERNAL `mountpoint` (not
    /// this store's own `<name>/_data` convention) — the base for
    /// `kind: ShareVolume` (`cmd::sharevolume` in the bin crate): a dedicated,
    /// ISOLATED subdirectory of an already-mounted `kind: Storage`, so N
    /// tenants can share one NFS/CIFS/WebDAV export without seeing each
    /// other's data (plain path confinement — a container that only ever
    /// bind-mounts ITS subdirectory cannot reach a sibling's). Idempotent:
    /// re-registering the same name updates `quota_bytes`/`alert_pct`
    /// in place rather than erroring, so a `ShareVolume` manifest's re-`apply`
    /// with a bumped quota just works. `mountpoint` is created if missing;
    /// this store never manages ITS lifecycle beyond that — `remove()` only
    /// ever deletes this record's OWN bookkeeping dir (`<root>/<name>/`), not
    /// `mountpoint`, so removing a share never touches the shared data.
    /// `parent` names the volume this one is carved out of (see [`Volume::parent`]).
    /// It is preserved on a re-register rather than overwritten with `None`: an
    /// idempotent re-apply must not turn a share back into a plain volume, which
    /// is how it would stop being recognised as one.
    pub fn register_external(
        &self,
        name: &str,
        mountpoint: &std::path::Path,
        quota_bytes: Option<u64>,
        alert_pct: Option<u8>,
        parent: Option<&str>,
    ) -> Result<Volume> {
        if !Self::valid_name(name) {
            return Err(Error::Invalid(format!("invalid volume name: {name:?}")));
        }
        fs::create_dir_all(mountpoint)?;
        fs::create_dir_all(self.dir(name))?; // this store's own per-name bookkeeping dir
        let vol = if let Ok(mut existing) = self.inspect(name) {
            existing.mountpoint = mountpoint.to_string_lossy().into_owned();
            existing.quota_bytes = quota_bytes;
            existing.alert_pct = alert_pct;
            if let Some(p) = parent {
                existing.parent = Some(p.to_string());
            }
            existing
        } else {
            Volume {
                name: name.to_string(),
                mountpoint: mountpoint.to_string_lossy().into_owned(),
                created_unix: now_unix(),
                driver: "local".to_string(),
                device: None,
                options: None,
                quota_bytes,
                alert_pct,
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                parent: parent.map(str::to_string),
            }
        };
        write_meta(&self.meta_path(name), &vol)?;
        Ok(vol)
    }

    /// Creates a volume with a driver (`local`/`nfs`). For `nfs`, it immediately
    /// mounts the *export* (`server:/path`) into the data directory — useful to
    /// connect to a TrueNAS or another NFS server. Idempotent.
    pub fn create_with(
        &self,
        name: &str,
        driver: &str,
        device: Option<String>,
        options: Option<String>,
    ) -> Result<Volume> {
        if !Self::valid_name(name) {
            return Err(Error::Invalid(format!("invalid volume name: {name:?}")));
        }
        if self.meta_path(name).exists() {
            let v = self.inspect(name)?;
            self.ensure_mounted(&v)?;
            return Ok(v);
        }
        // Network drivers require a `device` (the mount target): nfs
        // `server:/export`, cifs `//server/share`, webdav `https://…`.
        if is_network_driver(driver) && device.as_deref().unwrap_or("").is_empty() {
            return Err(Error::Invalid(format!(
                "{driver} volume requires a device (the mount target)"
            )));
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
            quota_bytes: None,
            alert_pct: None,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            // A volume created here owns its own data directory; only
            // `register_external` carves one out of another volume.
            parent: None,
        };
        // Mount BEFORE persisting: if NFS fails, we don't leave an orphan volume.
        if let Err(e) = self.ensure_mounted(&vol) {
            let _ = fs::remove_dir_all(self.dir(name));
            return Err(e);
        }
        write_meta(&self.meta_path(name), &vol)?;
        Ok(vol)
    }

    /// Merges `labels`/`annotations` into an existing volume's metadata and
    /// persists it. Used by the declarative reconciler to stamp ownership
    /// (`delonix.io/stack`) and the last applied spec.
    ///
    /// MERGES rather than replaces on purpose: a key this caller does not know
    /// about (set by an older version, or by a different tool) must survive. A
    /// key whose value is `None` is REMOVED — that is the only way to unstamp a
    /// volume that left a stack, and it has to be expressible.
    ///
    /// Read-modify-write without a lock, which is what every other mutation on
    /// this store already does (`register_external`, `set_quota`). The window is
    /// real but pre-existing and not widened here; the `Store<Container>` sibling
    /// is the one that carries an `flock`.
    pub fn set_metadata(
        &self,
        name: &str,
        labels: &[(String, Option<String>)],
        annotations: &[(String, Option<String>)],
    ) -> Result<Volume> {
        let mut vol = self.inspect(name)?;
        for (k, v) in labels {
            match v {
                Some(v) => {
                    vol.labels.insert(k.clone(), v.clone());
                }
                None => {
                    vol.labels.remove(k);
                }
            }
        }
        for (k, v) in annotations {
            match v {
                Some(v) => {
                    vol.annotations.insert(k.clone(), v.clone());
                }
                None => {
                    vol.annotations.remove(k);
                }
            }
        }
        write_meta(&self.meta_path(name), &vol)?;
        Ok(vol)
    }

    /// Ensures a NETWORK volume is mounted. No-op for local volumes or
    /// if it is already mounted. Best-effort: requires the type's mount helper
    /// (`mount.nfs`, `mount.cifs`, `mount.davfs`) and, typically, privilege.
    ///
    /// Supported types and their respective `mount -t`:
    /// - `nfs`   → `mount -t nfs   server:/export`  (external TrueNAS/NFS)
    /// - `cifs`/`smb` → `mount -t cifs //server/share` (Samba/Windows/TrueNAS SMB)
    /// - `webdav`/`dav` → `mount -t davfs https://…`  (Nextcloud/ownCloud WebDAV)
    pub fn ensure_mounted(&self, vol: &Volume) -> Result<()> {
        // Volume with a HARD quota (ext4 loopback): remounts the image if unmounted
        // (e.g.: after a host reboot). Best-effort — without privilege, no-op.
        let img = self.loop_img(&vol.name);
        if vol.quota_bytes.is_some() && img.exists() && !is_mounted(&vol.mountpoint) {
            let _ = Self::run(
                "mount",
                &["-o", "loop", &img.to_string_lossy(), &vol.mountpoint],
            );
        }
        if !is_network_driver(&vol.driver) || is_mounted(&vol.mountpoint) {
            return Ok(());
        }
        let fstype = mount_fstype(&vol.driver);
        let device = vol.device.as_ref().ok_or_else(|| {
            Error::Invalid(format!(
                "{} volume '{}' has no device",
                vol.driver, vol.name
            ))
        })?;
        // `--` before the positional device/mountpoint: `device` is built from a
        // `kind: Storage` manifest (`<server>:<share>`), and a server starting
        // with `-` would otherwise be read by `mount` as an option (`-r`, `-o …`)
        // rather than as an address. Not exploitable as RCE — this is argv, not a
        // shell, and `mount` runs no commands — but it is the same "a value must
        // never be mistaken for a flag" rule the ssh/scp/virsh call sites already
        // follow.
        let mut args = vec!["-t", fstype];
        if let Some(o) = &vol.options {
            args.push("-o");
            args.push(o);
        }
        args.push("--");
        args.push(device.as_str());
        args.push(vol.mountpoint.as_str());
        let ctx: &'static str = match fstype {
            "cifs" => "mount cifs",
            "davfs" => "mount webdav",
            _ => "mount nfs",
        };
        let out = std::process::Command::new("mount")
            .args(&args)
            .output()
            .map_err(|e| Error::Runtime {
                context: ctx,
                message: e.to_string(),
            })?;
        if !out.status.success() {
            return Err(Error::Runtime {
                context: ctx,
                message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    /// Lists the existing volumes.
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

    /// Inspects a volume by name.
    pub fn inspect(&self, name: &str) -> Result<Volume> {
        let meta = self.meta_path(name);
        if !meta.exists() {
            return Err(Error::NotFound(format!("volume {name}")));
        }
        Ok(serde_json::from_slice(&fs::read(meta)?)?)
    }

    // ---- Snapshots (Block B of the Odoo plan) ---------------------------------
    // A snapshot is a tar.gz of `_data`, stored in `<vol>/_snapshots/<snap>.tar.gz`
    // (survives the container; does NOT survive `volume rm` — it is a snapshot, not an
    // external backup). Crash-consistent: taken with the workload running; for
    // application consistency (e.g.: DB), the orchestrated backup (Block C) stops/dumps.
    // In rootless the tar runs in a mapped userns (effective owner of the subuids) — see the
    // CLI (`__volsnap`); this layer only knows about paths and listing.

    /// The snapshots directory of a volume.
    pub fn snapshots_dir(&self, name: &str) -> PathBuf {
        self.dir(name).join("_snapshots")
    }

    /// The file path of a snapshot (validates the name first).
    pub fn snapshot_path(&self, volume: &str, snap: &str) -> Result<PathBuf> {
        if !safe_snapshot_name(snap) {
            return Err(Error::Invalid(format!(
                "invalid snapshot name: '{snap}' (use [a-zA-Z0-9._-], no '/' or '..')"
            )));
        }
        Ok(self.snapshots_dir(volume).join(format!("{snap}.tar.gz")))
    }

    /// Lists the snapshots of a volume: `(name, bytes, mtime-unix)`.
    pub fn list_snapshots(&self, name: &str) -> Result<Vec<(String, u64, i64)>> {
        let dir = self.snapshots_dir(name);
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(&dir) else {
            return Ok(out);
        };
        for e in rd.flatten() {
            let p = e.path();
            let Some(fname) = p.file_name().and_then(|f| f.to_str()) else {
                continue;
            };
            let Some(snap) = fname.strip_suffix(".tar.gz") else {
                continue;
            };
            let md = e.metadata().ok();
            let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = md
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push((snap.to_string(), size, mtime));
        }
        out.sort_by_key(|s| s.2); // oldest first
        Ok(out)
    }

    /// Deletes a snapshot.
    pub fn remove_snapshot(&self, volume: &str, snap: &str) -> Result<()> {
        let p = self.snapshot_path(volume, snap)?;
        if !p.exists() {
            return Err(Error::NotFound(format!(
                "snapshot {snap} of volume {volume}"
            )));
        }
        fs::remove_file(p)?;
        Ok(())
    }

    /// Removes a volume (and its data). Unmounts first if it is `nfs`.
    pub fn remove(&self, name: &str) -> Result<()> {
        self.remove_with(name, None)
    }

    /// [`Self::remove`] with an injectable tree remover for data this process
    /// cannot unlink directly.
    ///
    /// **THE ORDER IS THE WHOLE POINT — this fixes a confirmed cross-tenant
    /// data leak.** This used to be a bare `fs::remove_dir_all(dir)`. In
    /// rootless, `_data` written by a container in a mapped userns belongs to a
    /// SUBUID, so `remove_dir_all` hits EACCES — but only AFTER it has already
    /// unlinked `meta.json`, because it deletes entries as it walks. The
    /// observed result: `rm` reported `Permission denied`, the volume vanished
    /// from `ls`/`inspect`/`system df`, and every byte stayed on disk. A later
    /// `create` of the SAME name then succeeded, reported `usage: 0 bytes`, and
    /// handed the previous owner's data to whoever mounted it — in a PaaS where
    /// volume names derive from app/addon names, tenant B silently inherits
    /// tenant A's database. Three orphans in exactly this state were found on a
    /// live host.
    ///
    /// So: **data first, bookkeeping last, and nothing at all is unlinked if
    /// the data cannot be removed** — a failed `rm` now leaves a volume that is
    /// still fully visible and still fully its owner's.
    ///
    /// `rmtree` lets the CLI inject `delonix_runtime::remove_tree_mapped` (the
    /// same mapped-userns helper `system prune` already uses) so subuid-owned
    /// data actually goes away instead of leaving a volume that no command can
    /// delete. Without the hook, the plain `fs` path is used and a subuid tree
    /// surfaces as a clean error.
    ///
    /// For a volume registered via [`Self::register_external`] (`kind:
    /// ShareVolume`) the external `mountpoint` is NOT under `dir`, so it is
    /// untouched here — unchanged behaviour, and the reason removing a share
    /// never destroys the shared data.
    pub fn remove_with(&self, name: &str, rmtree: Option<&dyn Fn(&std::path::Path)>) -> Result<()> {
        let dir = self.dir(name);
        if !dir.exists() {
            return Err(Error::NotFound(format!("volume {name}")));
        }
        if let Ok(v) = self.inspect(name) {
            // unmount nfs OR the hard-quota loopback before deleting the data.
            if (is_network_driver(&v.driver) || v.quota_bytes.is_some())
                && is_mounted(&v.mountpoint)
            {
                let _ = std::process::Command::new("umount")
                    .arg(&v.mountpoint)
                    .output();
            }
        }
        // 1) everything EXCEPT the metadata. Any failure here propagates with
        //    the record still intact.
        let meta = self.meta_path(name);
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path == meta {
                continue;
            }
            if path.is_dir() && !path.is_symlink() {
                if let Some(rm) = rmtree {
                    rm(&path);
                }
                if path.exists() {
                    fs::remove_dir_all(&path)?;
                }
            } else {
                fs::remove_file(&path)?;
            }
        }
        // 2) only now the bookkeeping, and only once the data is provably gone.
        if meta.exists() {
            fs::remove_file(&meta)?;
        }
        fs::remove_dir(&dir)?;
        Ok(())
    }

    // ---- Quota (#7, hybrid) --------------------------------------------------
    // ROOT model (privileged): HARD cap via a loop-mounted ext4 image on `_data`
    // (writes fail with ENOSPC when full; resize2fs grows it hot). ROOTLESS model
    // (monitor): the quota is a measured limit — `usage()`+`over_quota()`
    // expose the state and the alert; there is no hard cap (losetup needs CAP_SYS_ADMIN).

    fn loop_img(&self, name: &str) -> PathBuf {
        self.dir(name).join("data.img")
    }

    /// REAL usage in bytes of the volume (`du` of `_data`, recursive). For volumes with
    /// loopback, reflects what is used inside the ext4; for local ones, the data size.
    ///
    /// Prefer [`Self::usage_checked`] for anything a human or a quota decision
    /// reads: this returns a bare number and so cannot tell "empty" from
    /// "unreadable" (see [`Usage`]).
    pub fn usage(&self, name: &str) -> u64 {
        dir_usage(&self.data_dir(name)).bytes
    }

    /// Like [`Self::usage`], but reports whether the walk was COMPLETE.
    pub fn usage_checked(&self, name: &str) -> Usage {
        dir_usage(&self.data_dir(name))
    }

    /// Like [`Self::usage`], but for ANY path — not just this store's own
    /// `<name>/_data` convention. Lets a caller (e.g. `kind: ShareVolume`,
    /// which registers volumes via [`Self::register_external`] with a
    /// `mountpoint` OUTSIDE this store) measure the SAME way without
    /// duplicating the walk.
    pub fn usage_at(&self, path: &std::path::Path) -> u64 {
        dir_usage(path).bytes
    }

    /// Like [`Self::usage_at`], but reports whether the walk was COMPLETE.
    pub fn usage_at_checked(&self, path: &std::path::Path) -> Usage {
        dir_usage(path)
    }

    /// Is the volume at (or above) the alert threshold? `(in_alert, above_quota)`.
    pub fn quota_state(&self, vol: &Volume) -> (bool, bool) {
        quota_state_of(self.usage(&vol.name), vol.quota_bytes, vol.alert_pct)
    }

    /// Like [`Self::quota_state`], but carries whether the usage was actually
    /// measurable — an unreadable subtree must not read as "within quota".
    pub fn quota_state_checked(&self, vol: &Volume) -> QuotaState {
        let u = self.usage_checked(&vol.name);
        quota_state_checked_of(u, vol.quota_bytes, vol.alert_pct)
    }

    /// [`Self::quota_state_checked`] for an arbitrary path/limit (the
    /// `kind: ShareVolume` case — see [`Self::usage_at_checked`]).
    pub fn quota_state_at_checked(
        &self,
        path: &std::path::Path,
        quota_bytes: Option<u64>,
        alert_pct: Option<u8>,
    ) -> QuotaState {
        quota_state_checked_of(self.usage_at_checked(path), quota_bytes, alert_pct)
    }

    /// Like [`Self::quota_state`], parameterized directly instead of reading
    /// a stored [`Volume`] — for a caller tracking quota against an external
    /// path/limit of its own (see [`Self::usage_at`]).
    pub fn quota_state_at(
        &self,
        path: &std::path::Path,
        quota_bytes: Option<u64>,
        alert_pct: Option<u8>,
    ) -> (bool, bool) {
        quota_state_of(self.usage_at(path), quota_bytes, alert_pct)
    }

    fn run(cmd: &str, args: &[&str]) -> Result<()> {
        let out = std::process::Command::new(cmd)
            .args(args)
            .output()
            .map_err(|e| Error::Runtime {
                context: "quota",
                message: format!("{cmd}: {e}"),
            })?;
        if !out.status.success() {
            return Err(Error::Runtime {
                context: "quota",
                message: format!(
                    "{cmd} {}: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
        Ok(())
    }

    /// Finds the loop device serving the image (`losetup -j`), if any.
    fn loop_dev(img: &std::path::Path) -> Option<String> {
        let out = std::process::Command::new("losetup")
            .args(["-j", &img.to_string_lossy(), "-O", "NAME", "--noheadings"])
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        s.lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
    }

    /// Ensures the ext4 image (privileged) with `quota` bytes is mounted on `_data`.
    /// Creates it the 1st time (empty volume) or resizes it hot (grows: truncate +
    /// online resize2fs). Returns `Err` if privilege/tools are missing.
    fn apply_loopback(&self, name: &str, quota: u64) -> Result<()> {
        let img = self.loop_img(name);
        let data = self.data_dir(name);
        let data_s = data.to_string_lossy().into_owned();
        if !img.exists() {
            // we only create a loopback over an EMPTY `_data` (otherwise we'd hide data).
            if self.usage(name) > 0 {
                return Err(Error::Invalid(
                    "hard quota (loopback) only on an empty volume; create with --quota or empty it first".into(),
                ));
            }
            // sparse image the size of the quota → ext4 → loop mount.
            Self::run(
                "truncate",
                &["-s", &quota.to_string(), &img.to_string_lossy()],
            )?;
            Self::run(
                "mkfs.ext4",
                &["-q", "-F", "-m", "0", &img.to_string_lossy()],
            )?;
            fs::create_dir_all(&data)?;
            Self::run("mount", &["-o", "loop", &img.to_string_lossy(), &data_s])?;
            return Ok(());
        }
        // image already exists → ensure mounted and resize to the new quota.
        if !is_mounted(&data_s) {
            Self::run("mount", &["-o", "loop", &img.to_string_lossy(), &data_s])?;
        }
        let cur = fs::metadata(&img).map(|m| m.len()).unwrap_or(0);
        if quota > cur {
            // GROW hot: increase the image and the fs (online).
            Self::run(
                "truncate",
                &["-s", &quota.to_string(), &img.to_string_lossy()],
            )?;
            let dev = Self::loop_dev(&img).ok_or_else(|| Error::Runtime {
                context: "quota",
                message: "loop device not found".into(),
            })?;
            Self::run("losetup", &["-c", &dev])?; // recognizes the backing's new size
            Self::run("resize2fs", &[&dev])?; // online grow
        } else if quota < cur {
            // SHRINK: ext4 does not shrink online — do it offline (unmount/resize/mount).
            // Refuses if busy (container in use) or if the quota < current usage.
            if self.usage(name) > quota {
                return Err(Error::Invalid(
                    "the new quota is smaller than the current usage — free up space first".into(),
                ));
            }
            if std::process::Command::new("umount")
                .arg(&data_s)
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(true)
            {
                return Err(Error::Invalid(
                    "volume in use — stop the containers to shrink the quota".into(),
                ));
            }
            let blocks = format!("{}s", quota / 512); // resize2fs accepts size in sectors
                                                      // resize2fs needs e2fsck before shrinking; temporary loop.
            Self::run("e2fsck", &["-f", "-y", &img.to_string_lossy()]).ok();
            Self::run("resize2fs", &[&img.to_string_lossy(), &blocks])?;
            Self::run(
                "truncate",
                &["-s", &quota.to_string(), &img.to_string_lossy()],
            )?;
            Self::run("mount", &["-o", "loop", &img.to_string_lossy(), &data_s])?;
        }
        Ok(())
    }

    /// Sets (or removes) a volume's quota. `privileged` (root model) enables the
    /// HARD cap via ext4 loopback; otherwise it stays in MONITOR mode (only persists the limit).
    /// `quota=None` removes the limit (does not undo an already-created loopback).
    pub fn set_quota(
        &self,
        name: &str,
        quota: Option<u64>,
        alert_pct: Option<u8>,
        privileged: bool,
    ) -> Result<Volume> {
        let mut vol = self.inspect(name)?;
        if let (Some(q), true) = (quota, privileged) {
            self.apply_loopback(name, q)?;
        }
        vol.quota_bytes = quota;
        if alert_pct.is_some() {
            vol.alert_pct = alert_pct;
        }
        write_meta(&self.meta_path(name), &vol)?;
        Ok(vol)
    }

    /// `resolve_spec` for a workload in `namespace`.
    ///
    /// A named volume resolves in this order, and the order is the whole point:
    ///
    /// 1. the namespace's own volume (`.ns/<namespace>/<name>`) — what a `ShareVolume`
    ///    declared in that namespace registers, so `-v db:/data` in `teamA` reaches
    ///    `teamA`'s `db` and never `teamB`'s;
    /// 2. the global volume (`<name>`) — every plain `delonix volumes create`, and every
    ///    share that predates the scoping. This is the "two read paths" the migration
    ///    decision asked for: nothing that exists today stops resolving.
    ///
    /// **If BOTH exist it REFUSES.** Two different volumes answer to the same `-v db` and
    /// picking either one silently is how a workload ends up writing into another
    /// namespace's data. That case is rare, always the result of a half-done migration, and
    /// the error names the way out.
    ///
    /// On-demand creation (the Docker-like behaviour of `-v newname:/x`) stays GLOBAL: a
    /// name nobody declared is not a namespaced share, and creating it scoped would make
    /// the same command produce a different volume per namespace by accident.
    pub fn resolve_spec_in(&self, spec: &str, namespace: &str) -> Result<Mount> {
        let src = spec.split(':').next().unwrap_or_default();
        let named = !src.is_empty() && !src.starts_with('/') && !src.starts_with('.');
        if named && self.scoped_exists(namespace, src) {
            if self.meta_path(src).exists() {
                return Err(Error::Invalid(format!(
                    "volume {src:?} is ambiguous in namespace {namespace:?}: a namespaced \
                     volume and a global one of the same name both exist. Rename one, or \
                     finish the migration with `delonix sharevolume migrate`"
                )));
            }
            let scoped = Self {
                root: self.root.join(Self::NS_SUBTREE).join(namespace),
            };
            return scoped.resolve_spec(spec);
        }
        self.resolve_spec(spec)
    }

    /// Translates a `-v` specification into a [`Mount`], with NO namespace scoping.
    ///
    /// - `name:/target[:ro]` → named volume (created if it does not exist);
    /// - `/host:/target[:ro]` (or `./rel`) → *bind mount* of a host path.
    ///
    /// Prefer [`Self::resolve_spec_in`] for anything that runs on behalf of a workload:
    /// this one cannot see a namespaced `ShareVolume`.
    pub fn resolve_spec(&self, spec: &str) -> Result<Mount> {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return Err(Error::Invalid(format!(
                "invalid volume spec: {spec:?} (use source:/target[:ro])"
            )));
        }
        let src = parts[0];
        let target = parts[1];
        // 3rd field: only `ro`/`rw` recognized. Before, ANY other option
        // (`z`/`Z` SELinux, `U`, propagation) was SILENTLY ignored — the
        // bind mounted without the SELinux label and failed on RHEL/Fedora enforcing
        // with the user believing `:z` was handled. Fail-closed: explicit
        // error (finding from the Docker/Podman analysis; "no silent failure").
        // Propagation joined `ro`/`rw` here; SELinux labels did NOT, and the
        // error still says so rather than accepting them and doing nothing.
        let (readonly, propagation) = match parts.get(2) {
            None | Some(&"rw") => (false, None),
            Some(&"ro") => (true, None),
            Some(&"private") | Some(&"rprivate") => (false, Some("private".to_string())),
            Some(&"rslave") | Some(&"slave") => (false, Some("rslave".to_string())),
            Some(&"rshared") | Some(&"shared") => (false, Some("rshared".to_string())),
            Some(&"ro,rslave") => (true, Some("rslave".to_string())),
            Some(&"ro,rshared") => (true, Some("rshared".to_string())),
            Some(&"ro,rprivate") | Some(&"ro,private") => (true, Some("private".to_string())),
            Some(other) => {
                return Err(Error::Invalid(format!(
                    "unsupported bind option ':{other}' — supported: ':ro'/':rw', ':rprivate'/':rslave'/':rshared' (and 'ro,<propagation>'); SELinux ':z'/':Z' and ':U' are not implemented"
                )))
            }
        };
        if !target.starts_with('/') {
            return Err(Error::Invalid(format!(
                "target must be absolute: {target:?}"
            )));
        }

        let source = if src.starts_with('/') || src.starts_with('.') {
            // bind mount of a host path
            // `canonicalize` also fails with EACCES on a parent directory and
            // with ELOOP; "does not exist" would send the operator to the wrong
            // question in both.
            let p = fs::canonicalize(src)
                .map_err(|e| Error::not_found_or_io(e, || format!("bind path {src}")))?;
            p.to_string_lossy().into_owned()
        } else {
            // named volume (creates on demand, like Docker; mounts the NFS if applicable)
            let vol = self.create(src)?;
            self.ensure_mounted(&vol)?;
            vol.mountpoint
        };

        Ok(Mount {
            source,
            target: target.to_string(),
            readonly,
            propagation,
            optional: false,
        })
    }
}

/// Recursive directory size in bytes — the shared implementation behind
/// [`VolumeStore::usage`]/[`VolumeStore::usage_at`].
///
/// **Counts what `du` counts**, because that is what this number is used for:
/// allocated blocks (`st_blocks * 512`), with hardlinked files counted ONCE.
///
/// BUG FIXED HERE. This used to sum `m.len()` — the *apparent* size, with no
/// inode deduplication — while its own doc called it "`du` of `_data`". Two
/// independent errors, in opposite directions, on the number that IS the
/// rootless quota (the only enforcement rootless has, since `losetup` needs
/// CAP_SYS_ADMIN):
///
///  * **Hardlinks counted N times.** A tree with heavy linking (package
///    caches, `node_modules`, deduplicated OCI layers) over-reports, so a
///    volume trips its quota while genuinely holding far less.
///  * **Apparent size, not blocks.** Sparse files count at full nominal
///    length — including this crate's OWN hard-quota image, which
///    `apply_loopback` creates with `truncate -s <quota>`: an EMPTY volume with
///    a 100 GB quota reported 100 GB used. In the other direction, many tiny
///    files under-report, because each still occupies a whole block.
///
/// Measured on a real host store (~94 GiB): the old walk came out **+4.9 %**
/// against `du`, the two errors partially cancelling. The cancellation is
/// luck — it depends entirely on the workload mix, so the error was neither
/// bounded nor predictable in the direction that matters.
///
/// Blocks also make the two quota models agree: the hard cap is an ext4 image,
/// and ext4 raises ENOSPC on allocated blocks, not on apparent bytes.
fn dir_usage(p: &std::path::Path) -> Usage {
    // Only inodes with nlink > 1 are remembered — the overwhelming majority of
    // files are unlinked-once, and storing every one of them would make peak
    // memory proportional to the file COUNT of the tree rather than to the
    // number of actual hardlinks.
    let mut seen_links: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    dir_usage_inner(p, &mut seen_links)
}

/// Bytes a file actually occupies on disk, `du`-style: `st_blocks` is defined
/// by POSIX in 512-byte units regardless of the filesystem's own block size.
fn allocated_bytes(m: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.blocks().saturating_mul(512)
}

fn dir_usage_inner(
    p: &std::path::Path,
    seen_links: &mut std::collections::HashSet<(u64, u64)>,
) -> Usage {
    use std::os::unix::fs::MetadataExt;
    let mut out = Usage::default();
    let Ok(rd) = fs::read_dir(p) else {
        // The directory itself is unreadable — count it instead of pretending
        // the subtree is empty. See [`Usage`] for why this mattered in practice.
        out.unreadable += 1;
        return out;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            let sub = dir_usage_inner(&e.path(), seen_links);
            out.bytes += sub.bytes;
            out.unreadable += sub.unreadable;
        } else if let Ok(m) = e.metadata() {
            // A file reachable through several names must be charged once, or
            // the same blocks are billed to the volume as many times as it is
            // linked. Keyed on (dev, ino): an inode number alone is only unique
            // WITHIN a filesystem, and a volume tree can span mount points.
            if m.nlink() > 1 && !seen_links.insert((m.dev(), m.ino())) {
                continue;
            }
            out.bytes += allocated_bytes(&m);
        }
    }
    out
}

/// `(in_alert, above_quota)` from a measured `used` against `quota_bytes`/`alert_pct`
/// — the shared implementation behind [`VolumeStore::quota_state`]/[`VolumeStore::quota_state_at`].
/// [`quota_state_of`] carrying the "was it measurable at all?" bit. An
/// incomplete walk yields `measured: false` with both verdicts `false`, which
/// the caller must render as *unknown* — never as "within quota".
fn quota_state_checked_of(
    used: Usage,
    quota_bytes: Option<u64>,
    alert_pct: Option<u8>,
) -> QuotaState {
    if !used.is_complete() {
        return QuotaState {
            in_alert: false,
            above_quota: false,
            measured: false,
        };
    }
    let (in_alert, above_quota) = quota_state_of(used.bytes, quota_bytes, alert_pct);
    QuotaState {
        in_alert,
        above_quota,
        measured: true,
    }
}

/// `(in_alert, above_quota)` from a measured `used` against `quota_bytes`/`alert_pct`.
///
/// BUG FIXED HERE: this was `used * 100 >= q * pct`, and **both** products
/// overflow `u64`. `q * pct` goes over for any quota above ~182 PB with the
/// default 90 % — and `parse_size_bytes` explicitly accepts `1024t` (there is a
/// test asserting it), which is 1.15 EB. The workspace's `[profile.release]`
/// does not enable `overflow-checks`, so in a release build the multiplication
/// **wraps silently** and the alert verdict comes out arbitrary; in debug it
/// panics instead. Either way the operator is not told anything true.
///
/// Rewritten as a division on the larger side, which cannot overflow: comparing
/// `used / q` against `pct / 100` via `used >= q / 100 * pct` would lose
/// precision on small quotas, so the comparison is done in `u128` — exact for
/// every `u64` input, with no branch on magnitude to get wrong.
///
/// `alert_pct` is also clamped to 100: it is a `u8`, so nothing stopped an
/// operator (or a manifest) from setting 200, which silently meant "alert at
/// twice the quota", i.e. an alert that fires only after the limit is already
/// blown — the opposite of an early warning.
fn quota_state_of(used: u64, quota_bytes: Option<u64>, alert_pct: Option<u8>) -> (bool, bool) {
    match quota_bytes {
        Some(q) if q > 0 => {
            let pct = alert_pct.unwrap_or(90).min(100) as u128;
            let in_alert = used as u128 * 100 >= q as u128 * pct;
            (in_alert, used >= q)
        }
        _ => (false, false),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Safe snapshot name: `[A-Za-z0-9._-]+`, no path traversal.
pub fn safe_snapshot_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `true` if `path` is an active mount point (queries `/proc/mounts`).
fn is_mounted(path: &str) -> bool {
    fs::read_to_string("/proc/mounts")
        .map(|s| s.lines().any(|l| l.split_whitespace().nth(1) == Some(path)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `meta.json` written before this field existed has to keep
    /// deserializing — otherwise every volume on an upgraded host vanishes from
    /// `volumes ls` while its bytes stay on disk, which is exactly the
    /// cross-tenant leak `write_meta`'s own doc-comment warns about.
    #[test]
    fn meta_json_sem_labels_continua_a_desserializar() {
        let legacy =
            r#"{"name":"dados","mountpoint":"/x/_data","created_unix":1,"driver":"local"}"#;
        let v: Volume = serde_json::from_str(legacy).unwrap();
        assert_eq!(v.name, "dados");
        assert!(v.labels.is_empty() && v.annotations.is_empty());
        // And a volume with no metadata serializes without the keys at all
        // (`skip_serializing_if`), so re-writing a legacy record does not grow it.
        let back = serde_json::to_string(&v).unwrap();
        assert!(!back.contains("labels"), "{back}");
        assert!(!back.contains("annotations"), "{back}");
    }

    /// **The regression guard for the hole this API exists to close.**
    ///
    /// `list` is scoped to the root by design and must STAY that way — it is
    /// what a per-tenant listing is built on. The danger is the other half:
    /// anything that has to account for the whole store (a reclaim sweep, a
    /// usage report) reading `list` and concluding the store holds nothing but
    /// the root. That is not a hypothetical — a tenant's volumes survived every
    /// prune ever run on a real host and were 46 of the 141 GB later removed by
    /// hand.
    ///
    /// So this test asserts BOTH halves: that `list` still does not see a
    /// namespaced volume, and that `list_all` does, with the owner attached. If
    /// someone re-bases a sweep on `list`, the second half stops being true.
    #[test]
    fn list_nao_ve_o_volume_de_um_inquilino_e_list_all_ve_com_o_dono() {
        let tmp = std::env::temp_dir().join(format!("dlx-vol-listall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let store = VolumeStore::open(&tmp).unwrap();
        store.create("sem-dono").unwrap();
        store.scoped("acme").unwrap().create("pgdata").unwrap();
        store.scoped("globex").unwrap().create("pgdata").unwrap();

        // Half 1: the scoped listing stays scoped.
        let root: Vec<String> = store.list().unwrap().into_iter().map(|v| v.name).collect();
        assert_eq!(root, ["sem-dono"], "`list` deixou de ser a raiz apenas");

        // Half 2: the whole store is reachable, and each volume knows its owner.
        let mut all: Vec<(String, String)> = store
            .list_all()
            .unwrap()
            .into_iter()
            .map(|o| (o.owner().to_string(), o.volume.name))
            .collect();
        all.sort();
        assert_eq!(
            all,
            [
                ("<none>".to_string(), "sem-dono".to_string()),
                ("acme".to_string(), "pgdata".to_string()),
                ("globex".to_string(), "pgdata".to_string()),
            ],
            "um dono ficou de fora da varredura"
        );

        // And the two `pgdata` are genuinely different volumes on disk — the
        // reason the sweep's identity had to stop being the name.
        let a = store.scoped("acme").unwrap().inspect("pgdata").unwrap();
        let b = store.scoped("globex").unwrap().inspect("pgdata").unwrap();
        assert_ne!(a.mountpoint, b.mountpoint);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The trap this repo has paid for four times (`-v` not persisted, `-p` on a
    /// custom network, extra networks lost on restart, `Container.pod` never
    /// assigned): state needed to RECONSTRUCT the resource must survive every
    /// path that rewrites the record. `create`/`create_with` are idempotent and
    /// re-enter through `inspect`, so ownership stamped by a previous `apply`
    /// must still be there after a re-`apply` — otherwise `stack destroy` would
    /// stop recognizing its own volumes.
    #[test]
    fn as_labels_sobrevivem_a_um_create_repetido_e_a_set_quota() {
        let tmp = std::env::temp_dir().join(format!("dlx-vol-labels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let store = VolumeStore::open(&tmp).unwrap();
        store.create("dados").unwrap();
        store
            .set_metadata(
                "dados",
                &[("delonix.io/stack".into(), Some("web".into()))],
                &[("delonix.io/last-applied".into(), Some("{\"a\":1}".into()))],
            )
            .unwrap();

        // Re-`apply` of the same manifest: `create` short-circuits via `inspect`.
        let again = store.create("dados").unwrap();
        assert_eq!(again.labels.get("delonix.io/stack").unwrap(), "web");
        assert_eq!(
            again.annotations.get("delonix.io/last-applied").unwrap(),
            "{\"a\":1}"
        );

        // `create_with` takes a different branch (it also calls `ensure_mounted`).
        let w = store.create_with("dados", "local", None, None).unwrap();
        assert_eq!(w.labels.get("delonix.io/stack").unwrap(), "web");

        // A `None` value REMOVES the key — the only way to unstamp a volume that
        // left a stack.
        let cleared = store
            .set_metadata("dados", &[("delonix.io/stack".into(), None)], &[])
            .unwrap();
        assert!(cleared.labels.is_empty());
        assert!(
            cleared.annotations.contains_key("delonix.io/last-applied"),
            "removing a label must not touch the annotations"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bind_option_rejeita_selinux_e_desconhecidas() {
        // Fail-closed: an unsupported bind option (`:z`/`:Z` SELinux, `:U`,
        // propagation) gives an ERROR instead of being silently ignored.
        let tmp = std::env::temp_dir().join(format!("dlx-vol-bindopt-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = VolumeStore::open(&tmp).unwrap();
        let src = tmp.to_string_lossy();
        assert!(store.resolve_spec(&format!("{src}:/dst:z")).is_err());
        assert!(store.resolve_spec(&format!("{src}:/dst:Z")).is_err());
        assert!(store.resolve_spec(&format!("{src}:/dst:U")).is_err());
        // `ro`/`rw` still work (no regression).
        assert!(store.resolve_spec(&format!("{src}:/dst:ro")).is_ok());
        assert!(store.resolve_spec(&format!("{src}:/dst:rw")).is_ok());
        assert!(store.resolve_spec(&format!("{src}:/dst")).is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size_bytes("1024"), Some(1024));
        assert_eq!(parse_size_bytes("1k"), Some(1024));
        assert_eq!(parse_size_bytes("2m"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size_bytes("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size_bytes("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size_bytes("512mb"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size_bytes("0"), None);
        assert_eq!(parse_size_bytes("abc"), None);
        assert_eq!(parse_size_bytes(""), None);
    }

    /// The alert/above arithmetic, driven off a REAL measurement.
    ///
    /// It used to hardcode `950 bytes written ⇒ 950 bytes used`, which quietly
    /// asserted the apparent-size semantics that `dir_usage` has since been
    /// corrected away from (it now counts allocated blocks, like `du`). Deriving
    /// the quota from the measured usage tests the thing this test is actually
    /// about — the 90 %/100 % thresholds — instead of pinning a filesystem's
    /// block size into an assertion.
    #[test]
    fn quota_state_alerts() {
        let (s, dir) = store();
        s.create("qv").unwrap();
        std::fs::write(s.data_dir("qv").join("f"), vec![0u8; 950]).unwrap();
        let used = s.usage("qv");
        assert!(used > 0, "um ficheiro de 950 bytes tem de ocupar blocos");

        // quota chosen so `used` sits at ~95 % of it ⇒ in alert, not above.
        let quota = used * 100 / 95;
        let v = s.set_quota("qv", Some(quota), Some(90), false).unwrap();
        let (warn, over) = s.quota_state(&v);
        assert!(
            warn && !over,
            "{used}/{quota} (~95%) deve estar em alerta mas não acima"
        );

        // grow past the quota
        std::fs::write(s.data_dir("qv").join("g"), vec![0u8; 64 * 1024]).unwrap();
        let (_, over2) = s.quota_state(&v);
        assert!(over2, "{}/{quota} deve estar acima da quota", s.usage("qv"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// REGRESSION: a file reachable through several hardlinks must be charged
    /// ONCE. Counting it per-name over-reports any tree with heavy linking
    /// (package caches, `node_modules`, deduplicated OCI layers) and trips the
    /// rootless quota on a volume that genuinely holds far less. Reverting the
    /// `(dev, ino)` dedup in `dir_usage_inner` makes this fail.
    #[test]
    fn usage_conta_um_ficheiro_com_hardlinks_uma_so_vez() {
        let base = tmpbase("hardlinks");
        let store = VolumeStore::open(&base).unwrap();
        store.create("hl").unwrap();
        let data = store.data_dir("hl");

        fs::write(data.join("original"), vec![7u8; 64 * 1024]).unwrap();
        let one_copy = store.usage("hl");
        assert!(
            one_copy >= 64 * 1024,
            "esperava >=64 KiB, obtive {one_copy}"
        );

        // Nine extra NAMES for the same inode — zero extra blocks on disk.
        for i in 0..9 {
            fs::hard_link(data.join("original"), data.join(format!("link{i}"))).unwrap();
        }
        let with_links = store.usage("hl");
        assert_eq!(
            with_links, one_copy,
            "10 nomes do MESMO inode não podem contar 10 vezes ({with_links} vs {one_copy})"
        );

        // A genuinely distinct file still adds up (no over-eager dedup).
        fs::write(data.join("outro"), vec![3u8; 64 * 1024]).unwrap();
        assert!(
            store.usage("hl") > with_links,
            "um ficheiro NOVO tem de continuar a somar"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// REGRESSION: a sparse file must count the blocks it actually occupies,
    /// not its nominal length.
    ///
    /// This is not hypothetical for this crate: `apply_loopback` creates the
    /// hard-quota image with `truncate -s <quota>`, which is sparse by
    /// definition — so under the old apparent-size walk an EMPTY volume with a
    /// 100 GB quota reported 100 GB used, in `volumes inspect`, `system df` and
    /// the dashboard alike.
    #[test]
    fn usage_de_ficheiro_esparso_conta_blocos_nao_o_tamanho_nominal() {
        use std::io::{Seek, SeekFrom, Write};
        let base = tmpbase("sparse");
        let store = VolumeStore::open(&base).unwrap();
        store.create("sp").unwrap();

        const NOMINAL: u64 = 256 * 1024 * 1024; // 256 MiB de tamanho aparente
        let mut f = fs::File::create(store.data_dir("sp").join("sparse.img")).unwrap();
        f.set_len(NOMINAL).unwrap();
        // Um único byte no fim — o resto do ficheiro é um buraco.
        f.seek(SeekFrom::End(-1)).unwrap();
        f.write_all(&[1u8]).unwrap();
        f.sync_all().unwrap();
        drop(f);

        let apparent = fs::metadata(store.data_dir("sp").join("sparse.img"))
            .unwrap()
            .len();
        assert_eq!(apparent, NOMINAL, "o tamanho nominal tem de ser 256 MiB");

        let used = store.usage("sp");
        // Alguns filesystems de teste (tmpfs) não têm buracos reais; se este
        // não tiver, a asserção não teria significado — declara-o em vez de
        // passar por acaso.
        if used >= NOMINAL {
            eprintln!(
                "aviso: {} não suporta ficheiros esparsos (used={used}) — asserção saltada",
                base.display()
            );
        } else {
            assert!(
                used < NOMINAL / 100,
                "um ficheiro esparso de 256 MiB com 1 byte escrito não pode contar {used} bytes"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn valid_name_recusa_dot_dot_e_dot() {
        // CRITICAL fixed here: a name made ENTIRELY of the allowed char '.'
        // passed the old charset-only check. `register_external`'s caller
        // (kind: ShareVolume) joins the name onto `<parent>/shares/<name>` —
        // ".." resolves to the PARENT's own data dir, ".": to the "shares"
        // dir containing every sibling tenant. Both must be rejected.
        assert!(!VolumeStore::valid_name(".."));
        assert!(!VolumeStore::valid_name("."));
        assert!(!VolumeStore::valid_name("-x"));
        assert!(!VolumeStore::valid_name("a..b"));
        assert!(!VolumeStore::valid_name(""));
        // legitimate names with an internal dot still work.
        assert!(VolumeStore::valid_name("my.vol_02"));
        assert!(VolumeStore::valid_name("tenant-a"));
    }

    #[test]
    fn register_external_recusa_nome_dot_dot() {
        let (s, dir) = store();
        let external = dir.join("shares").join("..");
        let err = s
            .register_external("..", &external, None, None, None)
            .unwrap_err();
        assert!(format!("{err}").contains("invalid volume name"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn register_external_aponta_para_fora_e_e_idempotente() {
        let (s, dir) = store();
        let external = dir.join("shares").join("tenant-a");
        let v = s
            .register_external("share-a", &external, Some(1000), Some(80), Some("nas"))
            .unwrap();
        assert_eq!(v.mountpoint, external.to_string_lossy());
        assert!(
            external.exists(),
            "o mountpoint externo devia ter sido criado"
        );
        // Re-registering (a `ShareVolume` re-`apply`) updates quota in place,
        // does not error, and keeps pointing at the SAME external path.
        let v2 = s
            .register_external("share-a", &external, Some(2000), Some(90), None)
            .unwrap();
        assert_eq!(v2.quota_bytes, Some(2000));
        assert_eq!(v2.mountpoint, external.to_string_lossy());

        // usage_at/quota_state_at measure the EXTERNAL path directly.
        // Derivado da medição real, não de um número aparente fixo — ver a nota
        // em `quota_state_alerts` sobre porque o valor absoluto deixou de ser
        // uma asserção legítima depois de `dir_usage` passar a contar blocos.
        std::fs::write(external.join("f"), vec![0u8; 1900]).unwrap();
        let used = s.usage_at(&external);
        assert!(used > 0, "o caminho externo tem de ser medível");
        let quota = used * 100 / 95;
        let (warn, over) = s.quota_state_at(&external, Some(quota), Some(90));
        assert!(
            warn && !over,
            "{used}/{quota} (~95%) devia estar em alerta mas não acima"
        );

        // `remove` deletes ONLY this store's own bookkeeping dir — the
        // external data (the shared Storage's real subdirectory) survives.
        s.remove("share-a").unwrap();
        assert!(
            external.exists(),
            "remove() nunca deve tocar num mountpoint externo"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

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
    fn create_with_driver_idempotent_and_meta_on_disk() {
        let (vs, base) = store();
        // create with explicit `local` driver
        let v = vs.create_with("app_data", "local", None, None).unwrap();
        assert_eq!(v.driver, "local");
        // meta.json must exist on disk
        assert!(base.join("volumes/app_data/meta.json").exists());
        // idempotent: re-creating returns the existing one without error
        let v2 = vs.create_with("app_data", "local", None, None).unwrap();
        assert_eq!(v2.name, "app_data");
        assert_eq!(vs.list().unwrap().len(), 1);
        // invalid name → Error::Invalid
        assert!(matches!(
            vs.create_with("bad name!", "local", None, None),
            Err(Error::Invalid(_))
        ));
        // nfs without device → Error::Invalid
        assert!(matches!(
            vs.create_with("nas", "nfs", None, None),
            Err(Error::Invalid(_))
        ));
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

    #[test]
    fn snapshot_names_reject_traversal() {
        assert!(safe_snapshot_name("pre-upgrade-1"));
        assert!(safe_snapshot_name("2026.07.06_0300"));
        for bad in ["", "../x", "a/b", ".oculto", "a b", &"x".repeat(129)] {
            assert!(!safe_snapshot_name(bad), "aceitou '{bad}'");
        }
    }

    #[test]
    fn snapshot_paths_and_listing() {
        let (vs, base) = store();
        vs.create("v1").unwrap();
        // validated path + non-existent ones list empty
        assert!(vs.snapshot_path("v1", "../evil").is_err());
        assert_eq!(vs.list_snapshots("v1").unwrap().len(), 0);
        // a "made" snapshot (file in place) appears in the listing
        let p = vs.snapshot_path("v1", "s1").unwrap();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"tar").unwrap();
        let ls = vs.list_snapshots("v1").unwrap();
        assert_eq!(ls.len(), 1);
        assert_eq!(ls[0].0, "s1");
        assert_eq!(ls[0].1, 3);
        // remove
        vs.remove_snapshot("v1", "s1").unwrap();
        assert!(vs.remove_snapshot("v1", "s1").is_err());
        fs::remove_dir_all(&base).ok();
    }

    fn tmpbase(tag: &str) -> PathBuf {
        let b = std::env::temp_dir().join(format!(
            "dlx-vol-{tag}-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = fs::remove_dir_all(&b);
        fs::create_dir_all(&b).unwrap();
        b
    }

    /// REGRESSION (silent overflow): the alert arithmetic must hold for quotas
    /// anywhere in the range `parse_size_bytes` actually accepts.
    ///
    /// `used * 100 >= q * pct` overflows `u64` on the right-hand side for any
    /// quota above ~182 PB at the default 90 % — and `1024t` (1.15 EB) is a
    /// value this crate explicitly parses (see the test right below). Release
    /// builds have `overflow-checks` off, so the product wrapped and the verdict
    /// came out arbitrary; debug builds panicked. Restoring the `u64`
    /// multiplication makes this test fail (panic in debug, wrong answer in
    /// release).
    #[test]
    fn quota_nao_transborda_em_quotas_enormes() {
        // A maior quota que `parse_size_bytes` aceita nesta forma.
        let huge = parse_size_bytes("1024t").expect("1024t tem de continuar a parsear");
        // Vazio: nem em alerta nem acima.
        assert_eq!(quota_state_of(0, Some(huge), Some(90)), (false, false));
        // Metade da quota: longe do alerta de 90%.
        assert_eq!(
            quota_state_of(huge / 2, Some(huge), Some(90)),
            (false, false)
        );
        // 95%: em alerta, mas não acima.
        let at_95 = (huge as u128 * 95 / 100) as u64;
        assert_eq!(quota_state_of(at_95, Some(huge), Some(90)), (true, false));
        // Exactamente na quota: alerta E acima.
        assert_eq!(quota_state_of(huge, Some(huge), Some(90)), (true, true));
        // O extremo absoluto do tipo, para não deixar nenhum canto por cobrir.
        assert_eq!(
            quota_state_of(u64::MAX, Some(u64::MAX), Some(100)),
            (true, true)
        );

        // Quotas pequenas continuam exactas (a correcção não pode perder
        // precisão onde o comportamento antigo estava certo).
        assert_eq!(quota_state_of(899, Some(1000), Some(90)), (false, false));
        assert_eq!(quota_state_of(900, Some(1000), Some(90)), (true, false));
        assert_eq!(quota_state_of(1000, Some(1000), Some(90)), (true, true));
    }

    /// `alert_pct` é `u8`: nada impedia um 200, que significava "avisa ao dobro
    /// da quota" — um aviso que só dispara depois do limite já estourado, ou
    /// seja, o contrário de um aviso antecipado.
    #[test]
    fn alert_pct_acima_de_100_e_tratado_como_100() {
        // Com 200 sem clamp, 900/1000 não daria alerta nenhum (900*100 <
        // 1000*200) e o alerta só chegaria aos 2000 — depois de `above_quota`.
        assert_eq!(quota_state_of(999, Some(1000), Some(200)), (false, false));
        assert_eq!(quota_state_of(1000, Some(1000), Some(200)), (true, true));
        // O alerta nunca pode ficar para DEPOIS de se estar acima da quota.
        for pct in [0u8, 50, 90, 100, 150, 255] {
            let (warn, over) = quota_state_of(1000, Some(1000), Some(pct));
            assert!(
                warn || !over,
                "pct={pct}: acima da quota sem estar em alerta é incoerente"
            );
        }
    }

    /// A quota that overflows `u64` is an ERROR, never a silent saturation to
    /// `u64::MAX` — which reads as "quota set" but means "no quota at all".
    #[test]
    fn parse_size_bytes_recusa_overflow_em_vez_de_saturar() {
        assert_eq!(parse_size_bytes("99999999999t"), None);
        assert_eq!(parse_size_bytes("18446744073709551616"), None);
        // The largest sane values still parse (no over-eager rejection).
        assert_eq!(parse_size_bytes("1024t"), Some(1024 * 1024u64.pow(4)));
        assert_eq!(parse_size_bytes("2g"), Some(2 * 1024 * 1024 * 1024));
    }

    /// An unreadable subtree must be reported as INCOMPLETE, never as 0 bytes
    /// (see `Usage`): 0 is indistinguishable from an empty volume, and that is
    /// what made a rootless quota unable to ever fire.
    #[test]
    fn usage_marca_subarvore_ilegivel_em_vez_de_devolver_zero() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmpbase("usage-eacces");
        let store = VolumeStore::open(&base).unwrap();
        store.create("v1").unwrap();
        let data = store.data_dir("v1");
        let hidden = data.join("hidden");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join("f"), vec![7u8; 4096]).unwrap();
        fs::write(data.join("visible"), vec![1u8; 100]).unwrap();

        let before = store.usage_checked("v1");
        assert!(before.is_complete(), "tudo legível: {before:?}");
        // Blocos alocados, não tamanho aparente (ver `dir_usage`): 4096+100
        // bytes de conteúdo ocupam PELO MENOS isso em disco, tipicamente mais
        // por causa do enchimento até ao bloco. Fixar o número exacto voltaria
        // a codificar o tamanho do bloco do filesystem no teste.
        assert!(
            before.bytes >= 4196,
            "esperava pelo menos o conteúdo (4196), obtive {}",
            before.bytes
        );
        let all_readable = before.bytes;

        // Make the subtree unreadable — the rootless subuid case, reproduced
        // without needing subuids.
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        let after = store.usage_checked("v1");
        // Root ignores the mode bits — skip the assertion there, keep it honest.
        if !after.is_complete() {
            assert_eq!(after.unreadable, 1);
            assert!(
                after.bytes > 0 && after.bytes < all_readable,
                "só o que foi legível conta: {} (total legível era {all_readable})",
                after.bytes
            );
            let qs = quota_state_checked_of(after, Some(50), Some(90));
            assert!(!qs.measured, "não medido tem de ser 'desconhecido'");
            assert!(
                !qs.above_quota,
                "e nunca afirmar 'acima' nem 'dentro' da quota"
            );
        }
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    /// The cross-tenant leak: a `remove` that CANNOT delete the data must leave
    /// the volume fully intact — never unlink `meta.json` first and orphan the
    /// bytes, because the name then frees up and the next `create` of it hands
    /// the previous owner's data to someone else.
    #[test]
    fn remove_que_falha_nao_apaga_o_meta_nem_orfaniza_os_dados() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmpbase("rm-partial");
        let store = VolumeStore::open(&base).unwrap();
        store.create("v1").unwrap();
        let data = store.data_dir("v1");
        let inner = data.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("secret"), b"tenant-a").unwrap();
        // `_data` itself unreadable → the recursive delete cannot finish.
        fs::set_permissions(&data, fs::Permissions::from_mode(0o000)).unwrap();

        let res = store.remove("v1");
        fs::set_permissions(&data, fs::Permissions::from_mode(0o755)).unwrap();
        if res.is_err() {
            // The invariant that matters: the record survived the failure.
            assert!(
                store.meta_path("v1").exists(),
                "meta.json foi apagado numa remoção FALHADA — é este o bug"
            );
            assert!(
                store.inspect("v1").is_ok(),
                "o volume tem de continuar visível"
            );
            assert!(
                store.list().unwrap().iter().any(|v| v.name == "v1"),
                "e continuar em `ls`, para não haver dados órfãos invisíveis"
            );
            assert_eq!(
                fs::read(inner.join("secret")).unwrap(),
                b"tenant-a",
                "os dados do dono continuam lá"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// The happy path still removes everything, and the injected `rmtree` hook
    /// is what gets a chance at a tree the plain `fs` path cannot unlink.
    #[test]
    fn remove_apaga_tudo_e_chama_o_rmtree_injectado() {
        let base = tmpbase("rm-ok");
        let store = VolumeStore::open(&base).unwrap();
        store.create("v1").unwrap();
        fs::write(store.data_dir("v1").join("f"), b"x").unwrap();

        let called = std::cell::Cell::new(0usize);
        let hook = |_: &std::path::Path| called.set(called.get() + 1);
        store.remove_with("v1", Some(&hook)).unwrap();

        assert!(
            !store.volume_dir("v1").exists(),
            "o volume tem de desaparecer"
        );
        assert!(store.inspect("v1").is_err());
        assert!(called.get() >= 1, "o rmtree injectado tem de ser tentado");
        // Removing what is already gone is an error (docker parity), not a panic.
        assert!(store.remove("v1").is_err());
        let _ = fs::remove_dir_all(&base);
    }

    /// A `ShareVolume`'s EXTERNAL data must survive un-registering it — only
    /// this store's own bookkeeping dir goes away.
    #[test]
    fn remove_de_volume_externo_preserva_os_dados_partilhados() {
        let base = tmpbase("rm-external");
        let store = VolumeStore::open(&base).unwrap();
        let shared = base.join("nas").join("shares").join("tenant-a");
        store
            .register_external("tenant-a", &shared, Some(1024), Some(90), Some("nas"))
            .unwrap();
        fs::write(shared.join("data"), b"nas-payload").unwrap();

        store.remove("tenant-a").unwrap();
        assert!(store.inspect("tenant-a").is_err(), "o registo sai");
        assert_eq!(
            fs::read(shared.join("data")).unwrap(),
            b"nas-payload",
            "os dados do NAS NUNCA saem por um `rm` de share"
        );
        let _ = fs::remove_dir_all(&base);
    }
}

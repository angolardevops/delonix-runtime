//! The `zfs` storage pool driver (ADR-0067 D1, phase P3).
//!
//! A pool is a parent dataset `<dataset>` the administrator declared. A
//! container volume is a child dataset `<dataset>/<volume>` with `refquota` as
//! its hard quota, mounted under the helper's fixed root (its `mountpoint` is
//! set explicitly, so no request decides where a filesystem appears). The
//! stamp is a set of ZFS user properties ON the dataset (`delonix.io:*`).
//!
//! Reading a pool's state needs only a readable `/dev/zfs` (what the
//! `zfsutils-linux` udev rule gives) — delegated, run in the engine's own
//! process. Making and destroying a volume mounts and unmounts a filesystem,
//! which on Linux needs root: those go through the helper. When the engine
//! cannot read `/dev/zfs`, the routing asks the helper for the state too.
//!
//! The formats read here are `zfs-list(8)` and `zfs-get(8)` in scripted mode
//! (`-H`: tab-separated, no header; `-p`: exact numbers).

use std::path::PathBuf;
use std::sync::Arc;

use delonix_storage::admin::PoolAdmin;
use delonix_storage::exec::{argv, Output, Run};
use delonix_storage::mounts::{self, mountpoint};
use delonix_storage::ownership::Owner;
use delonix_storage::pool::{
    valid_volume_name, Allocation, PoolOp, PoolProbe, PoolRef, PoolUsage, Privilege,
    StoragePoolDriver, VolumeRequest, VolumeShape,
};
use delonix_storage::{Error, Result};

/// The driver's id in the allowlist.
pub const ID: &str = "zfs";

/// A NEW volume is refused above this share of the parent dataset's space.
pub const REFUSE_ABOVE_PCT: u8 = 95;

/// The user property that marks a zpool/dataset as made by `create-pool`.
pub const CREATED_PROP: &str = "delonix.io:created";

const P_ENGINE: &str = "delonix.io:engine";
const P_OWNER: &str = "delonix.io:owner";
const P_POOL: &str = "delonix.io:pool";
const P_VOLUME: &str = "delonix.io:volume";

/// What `zfs` answered, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Found(String),
    Missing(String),
    /// The module is not loaded or the tool is not there.
    Unavailable(String),
    /// No privilege to look (`/dev/zfs`), or an answer that cannot be read.
    Undetermined(String),
}

/// Classifies a `zfs`/`zpool` answer. The wording is OpenZFS's own
/// (`libzfs_error_init`, «dataset does not exist»).
pub fn classify(out: &Output) -> Answer {
    let all = format!("{}\n{}", out.stderr, out.stdout);
    if all.contains("modules are not loaded") {
        return Answer::Unavailable(
            "the zfs kernel module is not loaded (`modprobe zfs` as root)".into(),
        );
    }
    if all.contains("Permission denied") || all.contains("must be run as root") {
        return Answer::Undetermined(format!(
            "ZFS state needs a readable /dev/zfs (zfs printed: {})",
            out.why()
        ));
    }
    if !out.success() {
        let why = out.why();
        if why.contains("does not exist") || why.contains("no such pool") {
            return Answer::Missing(why);
        }
        return Answer::Undetermined(format!("zfs answered: {why}"));
    }
    if out.stdout.trim().is_empty() {
        return Answer::Undetermined("zfs listed nothing".into());
    }
    Answer::Found(out.stdout.clone())
}

/// The `property<TAB>value` lines of `zfs get -H -o property,value`.
pub fn parse_props(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('\t')?;
            Some((k.trim().to_string(), v.trim_end().to_string()))
        })
        .collect()
}

fn prop<'a>(props: &'a [(String, String)], key: &str) -> Option<&'a str> {
    props
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| *v != "-")
}

/// `used<TAB>avail` of `zfs list -Hp -o used,avail` → usage.
pub fn parse_usage(text: &str) -> PoolUsage {
    let mut it = text.lines().next().unwrap_or("").split('\t');
    let used = it.next().and_then(|v| v.trim().parse::<u64>().ok());
    let avail = it.next().and_then(|v| v.trim().parse::<u64>().ok());
    match (used, avail) {
        (Some(u), Some(a)) => PoolUsage {
            capacity_bytes: Some(u + a),
            used_bytes: Some(u),
            metadata_pct: None,
        },
        _ => PoolUsage::default(),
    }
}

/// The driver.
pub struct ZfsDriver {
    run: Arc<dyn Run>,
    mount_root: PathBuf,
}

impl ZfsDriver {
    pub fn new(run: Arc<dyn Run>, mount_root: PathBuf) -> Self {
        Self { run, mount_root }
    }

    fn dataset<'a>(pool: &'a PoolRef) -> Result<&'a str> {
        pool.entry.dataset.as_deref().ok_or_else(|| {
            Error::InvalidAllowlist(format!(
                "pool '{}': driver 'zfs' needs `dataset`",
                pool.name
            ))
        })
    }

    fn run(&self, parts: &[&str]) -> Result<Output> {
        self.run.run(&argv(parts)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::PoolUnavailable(format!(
                    "`{}` is not installed — install zfsutils-linux",
                    parts[0]
                ))
            } else {
                Error::PoolUnavailable(format!("`{}` cannot be run: {e}", parts[0]))
            }
        })
    }

    fn must(&self, parts: &[&str]) -> Result<Output> {
        let out = self.run(parts)?;
        if out.success() {
            Ok(out)
        } else {
            Err(Error::PoolUnavailable(format!(
                "`{}` failed: {}",
                parts.join(" "),
                out.why()
            )))
        }
    }

    fn found(&self, parts: &[&str]) -> Result<Option<String>> {
        match classify(&self.run(parts)?) {
            Answer::Found(t) => Ok(Some(t)),
            Answer::Missing(_) => Ok(None),
            Answer::Unavailable(w) => Err(Error::PoolUnavailable(w)),
            Answer::Undetermined(w) => Err(Error::PoolUndetermined(w)),
        }
    }

    fn usage_of(&self, ds: &str) -> Result<PoolUsage> {
        match self.found(&["zfs", "list", "-Hp", "-o", "used,avail", ds])? {
            Some(t) => Ok(parse_usage(&t)),
            None => Err(Error::PoolUnavailable(format!(
                "dataset {ds} does not exist"
            ))),
        }
    }

    fn props_of(&self, ds: &str) -> Result<Option<Vec<(String, String)>>> {
        let keys = [P_ENGINE, P_OWNER, P_POOL, P_VOLUME, "mountpoint", "mounted"].join(",");
        Ok(self
            .found(&["zfs", "get", "-Hp", "-o", "property,value", &keys, ds])?
            .map(|t| parse_props(&t)))
    }

    fn stamped_for(props: &[(String, String)], pool: &str, volume: &str, owner: &Owner) -> bool {
        prop(props, P_ENGINE) == Some("delonix")
            && prop(props, P_OWNER) == Some(owner.id())
            && prop(props, P_POOL) == Some(pool)
            && prop(props, P_VOLUME) == Some(volume)
    }

    /// Mounts the dataset if it is not, and hands its data directory to the
    /// volume's user.
    fn ready(&self, ds: &str, props: &[(String, String)], uid: u32) -> Result<PathBuf> {
        let mp = prop(props, "mountpoint")
            .ok_or_else(|| Error::PoolUnavailable(format!("dataset {ds} has no mountpoint")))?;
        if !std::path::Path::new(mp).starts_with(&self.mount_root) {
            return Err(Error::PoolConflict(format!(
                "dataset {ds} is mounted at {mp}, outside {} — it was not made by this helper",
                self.mount_root.display()
            )));
        }
        if prop(props, "mounted") != Some("yes") {
            self.must(&["zfs", "mount", ds])?;
        }
        mounts::prepare_data_dir(std::path::Path::new(mp), uid)
    }
}

fn check_volume(volume: &str) -> Result<()> {
    if valid_volume_name(volume) {
        Ok(())
    } else {
        Err(Error::InvalidPoolRequest(format!(
            "'{volume}' is not a volume name a pool can hold"
        )))
    }
}

impl StoragePoolDriver for ZfsDriver {
    fn id(&self) -> &'static str {
        ID
    }

    fn probe(&self, pool: &PoolRef) -> PoolProbe {
        let ds = match Self::dataset(pool) {
            Ok(d) => d,
            Err(e) => {
                return PoolProbe::Unavailable {
                    missing: e.to_string(),
                    remedy: "fix the pool's entry in the allowlist".into(),
                }
            }
        };
        let out = match self.run(&["zfs", "list", "-Hp", "-o", "name", ds]) {
            Ok(o) => o,
            Err(e) => {
                return PoolProbe::Unavailable {
                    missing: e.to_string(),
                    remedy: "the node's administrator installs it".into(),
                }
            }
        };
        match classify(&out) {
            Answer::Found(t) if t.lines().next().map(str::trim) == Some(ds) => PoolProbe::Available,
            Answer::Found(t) => PoolProbe::Undetermined {
                reason: format!("zfs listed '{}' for {ds}", t.trim()),
                remedy: "look at `zfs list` on the node".into(),
            },
            Answer::Missing(why) => PoolProbe::Unavailable {
                missing: format!("dataset {ds} does not exist ({why})"),
                remedy: format!(
                    "the administrator creates it: `sudo delonix-storage-helper create-pool {}`",
                    pool.name
                ),
            },
            Answer::Unavailable(missing) => PoolProbe::Unavailable {
                missing,
                remedy: "load the zfs module on the node".into(),
            },
            Answer::Undetermined(reason) => PoolProbe::Undetermined {
                reason,
                remedy: delonix_storage::helper::HELPER_REMEDY.into(),
            },
        }
    }

    fn required(&self, op: PoolOp) -> Privilege {
        match op {
            PoolOp::Probe | PoolOp::Usage => Privilege::Delegated("a readable /dev/zfs"),
            PoolOp::Allocate(_) | PoolOp::Release => Privilege::Helper,
        }
    }

    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation> {
        if req.shape == VolumeShape::Block {
            return Err(Error::PoolUnavailable(format!(
                "storage pool '{}': driver 'zfs' does not hand out block volumes in this version \
                 (VM disks in pools are ADR-0067 P5)",
                pool.name
            )));
        }
        check_volume(&req.name)?;
        let parent = Self::dataset(pool)?;
        let (uid, _) = owner.parts().ok_or_else(|| {
            Error::InvalidPoolRequest(format!("owner '{}' is not <uid>:<root>", owner.id()))
        })?;
        let ds = format!("{parent}/{}", req.name);
        if let Some(props) = self.props_of(&ds)? {
            if !Self::stamped_for(&props, pool.name, &req.name, owner) {
                return Err(Error::PoolConflict(format!(
                    "dataset {ds} exists and carries no stamp of this engine for this state root \
                     — it is never adopted by its name; pick another volume name"
                )));
            }
            let path = self.ready(&ds, &props, uid)?;
            return Ok(Allocation {
                pool: pool.name.to_string(),
                volume: req.name.clone(),
                shape: VolumeShape::Filesystem,
                path,
                adopted: true,
            });
        }
        if let Some(p) = self.usage_of(parent)?.used_pct() {
            if p >= REFUSE_ABOVE_PCT {
                return Err(Error::PoolExhausted(format!(
                    "storage pool '{}' is {p}% full — no new volume is made above \
                     {REFUSE_ABOVE_PCT}%",
                    pool.name
                )));
            }
        }
        let mp = mountpoint(&self.mount_root, pool.name, &req.name);
        if let Some(parent_dir) = mp.parent() {
            mounts::ensure_dir(parent_dir)?;
        }
        let o_mp = format!("mountpoint={}", mp.display());
        let o_quota = format!("refquota={}", req.size_bytes);
        let o_engine = format!("{P_ENGINE}=delonix");
        let o_owner = format!("{P_OWNER}={}", owner.id());
        let o_pool = format!("{P_POOL}={}", pool.name);
        let o_vol = format!("{P_VOLUME}={}", req.name);
        self.must(&[
            "zfs",
            "create",
            "-o",
            &o_mp,
            "-o",
            &o_quota,
            "-o",
            "canmount=on",
            "-o",
            &o_engine,
            "-o",
            &o_owner,
            "-o",
            &o_pool,
            "-o",
            &o_vol,
            &ds,
        ])?;
        let props = self.props_of(&ds)?.ok_or_else(|| {
            Error::PoolUnavailable(format!("dataset {ds} was created and cannot be read back"))
        })?;
        match self.ready(&ds, &props, uid) {
            Ok(path) => Ok(Allocation {
                pool: pool.name.to_string(),
                volume: req.name.clone(),
                shape: VolumeShape::Filesystem,
                path,
                adopted: false,
            }),
            Err(e) => {
                let _ = self.run(&["zfs", "destroy", &ds]);
                Err(e)
            }
        }
    }

    fn release(&self, pool: &PoolRef, volume: &str, owner: &Owner) -> Result<()> {
        check_volume(volume)?;
        let ds = format!("{}/{volume}", Self::dataset(pool)?);
        let mp = mountpoint(&self.mount_root, pool.name, volume);
        let Some(props) = self.props_of(&ds)? else {
            let _ = std::fs::remove_dir(&mp);
            return Ok(());
        };
        if !Self::stamped_for(&props, pool.name, volume, owner) {
            return Err(Error::PoolConflict(format!(
                "dataset {ds} does not carry this engine's stamp for volume '{volume}' — it is \
                 not released; nothing was deleted"
            )));
        }
        // No -r: a snapshot someone took of the volume stops the release
        // instead of going with it.
        self.must(&["zfs", "destroy", &ds])?;
        match std::fs::remove_dir(&mp) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::PoolUnavailable(format!(
                "{} is left behind: {e}",
                mp.display()
            ))),
        }
    }

    fn usage(&self, pool: &PoolRef) -> PoolUsage {
        Self::dataset(pool)
            .and_then(|ds| self.usage_of(ds))
            .unwrap_or_default()
    }
}

/// The zpool a dataset lives in.
pub fn zpool_of(dataset: &str) -> &str {
    dataset.split('/').next().unwrap_or(dataset)
}

impl PoolAdmin for ZfsDriver {
    fn id(&self) -> &'static str {
        ID
    }

    fn create_pool(&self, pool: &PoolRef, devices: &[PathBuf]) -> Result<String> {
        let ds = Self::dataset(pool)?;
        let zp = zpool_of(ds);
        if self
            .run(&["zpool", "list", "-H", "-o", "name", zp])?
            .success()
        {
            return Err(Error::DeviceInUse(format!(
                "zpool '{zp}' already exists — declare the pool without `create:` to use it as \
                 it is"
            )));
        }
        let mark = format!("{CREATED_PROP}=yes");
        let devs: Vec<String> = devices
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        let mut cmd: Vec<&str> = vec!["zpool", "create", "-O", "mountpoint=none", "-O", &mark, zp];
        cmd.extend(devs.iter().map(String::as_str));
        self.must(&cmd)?;
        if ds != zp {
            self.must(&["zfs", "create", "-o", &mark, ds])?;
        }
        Ok(format!(
            "storage pool '{}': zpool {zp} created on {} with dataset {ds}",
            pool.name,
            devs.join(", ")
        ))
    }

    fn created_here(&self, pool: &PoolRef) -> Result<bool> {
        let ds = Self::dataset(pool)?;
        let zp = zpool_of(ds);
        for target in [zp, ds] {
            let v = self.found(&["zfs", "get", "-Hp", "-o", "value", CREATED_PROP, target])?;
            if v.as_deref().map(str::trim) != Some("yes") {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn contents(&self, pool: &PoolRef) -> Result<Vec<String>> {
        let ds = Self::dataset(pool)?;
        let zp = zpool_of(ds);
        // Everything in the whole zpool: destroy-pool takes the zpool, so
        // anything anywhere in it is in the way.
        let t = self
            .found(&["zfs", "list", "-H", "-o", "name", "-t", "all", "-r", zp])?
            .unwrap_or_default();
        Ok(t.lines()
            .map(str::trim)
            .filter(|n| !n.is_empty() && *n != zp && *n != ds)
            .map(String::from)
            .collect())
    }

    fn destroy_pool(&self, pool: &PoolRef) -> Result<String> {
        let ds = Self::dataset(pool)?;
        let zp = zpool_of(ds);
        self.must(&["zpool", "destroy", zp])?;
        Ok(format!(
            "storage pool '{}': zpool {zp} destroyed",
            pool.name
        ))
    }
}

#[cfg(test)]
mod tests;

//! The `lvm-thin` storage pool driver (ADR-0067 D1, phase P4).
//!
//! A pool is a thin pool `<vg>/<thinPool>` the administrator declared. A
//! container volume is a thin logical volume `dlx-<volume>` in it, with an
//! ext4 filesystem, mounted under the helper's fixed root; the stamp is a set
//! of LVM tags ON the logical volume. Everything goes through device-mapper,
//! so every operation needs root: the engine reaches this driver through the
//! privileged helper (or as a root engine in `mode: in-process`), never
//! unprivileged.
//!
//! A tool's answer is classified, never trusted by its exit status: an
//! unprivileged `lvs` prints a warning, lists nothing and exits 0 — that is
//! «could not determine», never «no pool» (ADR-0067 D3.5).
//!
//! The output format read here is `lvmreport(7)`'s JSON (`--reportformat
//! json`), with sizes in bytes (`--units b --nosuffix`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use delonix_storage::admin::PoolAdmin;
use delonix_storage::exec::{argv, Output, Run};
use delonix_storage::mounts::{self, mountpoint};
use delonix_storage::ownership::{root_digest, Owner};
use delonix_storage::pool::{
    valid_volume_name, Allocation, PoolOp, PoolProbe, PoolRef, PoolUsage, Privilege,
    StoragePoolDriver, VolumeRequest, VolumeShape,
};
use delonix_storage::{Error, Result};

/// The driver's id in the allowlist.
pub const ID: &str = "lvm-thin";

/// A NEW volume is refused above this share of the thin pool's data or
/// metadata: a full thin pool puts every volume in it into I/O errors.
pub const REFUSE_ABOVE_PCT: u8 = 95;

/// The tag that marks a VG and its thin pool as made by `create-pool`.
pub const CREATED_TAG: &str = "delonix.io_created";

const ENGINE_TAG: &str = "delonix.io_engine";

/// LVM refuses a logical volume name containing one of these (`lvm(8)`).
const RESERVED: &[&str] = &[
    "_cdata", "_cmeta", "_corig", "_cvol", "_iorig", "_mimage", "_mlog", "_pmspare", "_rimage",
    "_rmeta", "_tdata", "_tmeta", "_vdata", "_vorigin", "_wcorig",
];

/// The driver. The runner and the mount root are injected so the tests can
/// run it with a scripted runner and a temporary root.
pub struct LvmThinDriver {
    run: Arc<dyn Run>,
    mount_root: PathBuf,
}

impl LvmThinDriver {
    pub fn new(run: Arc<dyn Run>, mount_root: PathBuf) -> Self {
        Self { run, mount_root }
    }

    fn vg_tp<'a>(pool: &'a PoolRef) -> Result<(&'a str, &'a str)> {
        match (pool.entry.vg.as_deref(), pool.entry.thin_pool.as_deref()) {
            (Some(vg), Some(tp)) => Ok((vg, tp)),
            _ => Err(Error::InvalidAllowlist(format!(
                "pool '{}': driver 'lvm-thin' needs `vg` and `thinPool`",
                pool.name
            ))),
        }
    }

    fn run(&self, parts: &[&str]) -> Result<Output> {
        self.run.run(&argv(parts)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::PoolUnavailable(format!(
                    "`{}` is not installed — install lvm2 and thin-provisioning-tools",
                    parts[0]
                ))
            } else {
                Error::PoolUnavailable(format!("`{}` cannot be run: {e}", parts[0]))
            }
        })
    }

    /// Runs a command that must succeed.
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

    /// One logical volume's report row, classified.
    fn lv(&self, vg: &str, lv: &str) -> Result<Answer<LvRow>> {
        let target = format!("{vg}/{lv}");
        let out = self.run(&[
            "lvs",
            "--reportformat",
            "json",
            "--units",
            "b",
            "--nosuffix",
            "-o",
            "lv_name,lv_attr,lv_size,data_percent,metadata_percent,lv_tags",
            &target,
        ])?;
        Ok(match classify(&out) {
            Answer::Found(rows) => match rows.into_iter().next() {
                Some(row) => Answer::Found(row),
                None => Answer::Undetermined("lvs listed nothing".into()),
            },
            Answer::Missing(why) => Answer::Missing(why),
            Answer::Undetermined(why) => Answer::Undetermined(why),
        })
    }

    fn mountpoint_of(&self, pool: &str, volume: &str) -> PathBuf {
        mountpoint(&self.mount_root, pool, volume)
    }

    fn is_mounted(&self, mp: &Path) -> Result<bool> {
        // `findmnt` exits 1 when nothing is mounted there (`findmnt(8)`).
        let out = self.run(&[
            "findmnt",
            "--noheadings",
            "--output",
            "TARGET",
            "--mountpoint",
            &mp.to_string_lossy(),
        ])?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Error::PoolUndetermined(format!(
                "cannot tell whether {} is mounted: {}",
                mp.display(),
                out.why()
            ))),
        }
    }

    fn thin_usage(&self, vg: &str, tp: &str) -> Result<PoolUsage> {
        match self.lv(vg, tp)? {
            Answer::Found(row) => Ok(row.usage()),
            Answer::Missing(why) => Err(Error::PoolUnavailable(why)),
            Answer::Undetermined(why) => Err(Error::PoolUndetermined(why)),
        }
    }

    /// Makes the filesystem on a fresh device, mounts it, and hands its data
    /// directory to the volume's user.
    fn mount_volume(&self, dev: &str, mp: &Path, uid: u32, fresh: bool) -> Result<PathBuf> {
        if fresh {
            self.must(&["mkfs.ext4", "-q", "-m", "0", "-L", "dlx", dev])?;
        }
        mounts::ensure_dir(mp)?;
        if !self.is_mounted(mp)? {
            self.must(&[
                "mount",
                "-t",
                "ext4",
                "-o",
                "nodev,nosuid",
                "--",
                dev,
                &mp.to_string_lossy(),
            ])?;
        }
        mounts::prepare_data_dir(mp, uid)
    }
}

/// What a classified listing said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer<T> {
    Found(T),
    /// The object does not exist — the tool said so in words.
    Missing(String),
    /// The tool could not tell (no privilege, an empty answer).
    Undetermined(String),
}

/// One row of an `lvs` report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LvRow {
    pub name: String,
    pub attr: String,
    pub size: Option<u64>,
    pub data_pct: Option<f64>,
    pub meta_pct: Option<f64>,
    pub tags: Vec<String>,
}

impl LvRow {
    fn usage(&self) -> PoolUsage {
        let used = match (self.size, self.data_pct) {
            (Some(s), Some(p)) => Some(((s as f64) * p / 100.0).ceil() as u64),
            _ => None,
        };
        PoolUsage {
            capacity_bytes: self.size,
            used_bytes: used,
            metadata_pct: self.meta_pct.map(|p| p.ceil().min(100.0) as u8),
        }
    }

    fn is_thin_pool(&self) -> bool {
        self.attr.starts_with('t')
    }
}

/// Did the tool say it lacks privilege? `lvm` running as a non-root user
/// warns and lists nothing, exit 0; device-mapper and the lock files answer
/// «Permission denied».
fn lacks_privilege(out: &Output) -> bool {
    let all = format!("{}\n{}", out.stderr, out.stdout);
    all.contains("Permission denied")
        || all.contains("non-root user")
        || all.contains("Operation not permitted")
}

/// Classifies an `lvs`/`vgs` JSON report. The rule ADR-0067 D3.5 fixes:
/// without privilege the answer is «could not determine», whatever the exit
/// status, and an empty answer is never «no pool».
pub fn classify(out: &Output) -> Answer<Vec<LvRow>> {
    if lacks_privilege(out) {
        return Answer::Undetermined(format!("LVM state needs root (lvm printed: {})", out.why()));
    }
    if !out.success() {
        let why = out.why();
        if why.contains("not found") || why.contains("Failed to find") {
            return Answer::Missing(why);
        }
        return Answer::Undetermined(format!("lvm answered: {why}"));
    }
    match parse_report(&out.stdout) {
        Some(rows) if !rows.is_empty() => Answer::Found(rows),
        Some(_) => Answer::Undetermined("lvm listed nothing".into()),
        None => Answer::Undetermined("lvm's report could not be read".into()),
    }
}

/// Reads the rows of an `lvmreport(7)` JSON report: `{"report":[{"lv":[…]}]}`
/// for `lvs`, `"vg"` for `vgs`, `"pv"` for `pvs`. Every value is a string; a
/// number that does not apply is `""`.
pub fn parse_report(text: &str) -> Option<Vec<LvRow>> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let mut rows = Vec::new();
    for section in v.get("report")?.as_array()? {
        let obj = section.as_object()?;
        for (_kind, list) in obj {
            for row in list.as_array()? {
                let get = |k: &str| row.get(k).and_then(|x| x.as_str()).unwrap_or("");
                let number = |k: &str| get(k).trim().parse::<f64>().ok();
                let name = ["lv_name", "vg_name", "pv_name"]
                    .iter()
                    .map(|k| get(k))
                    .find(|s| !s.is_empty())
                    .unwrap_or("")
                    .to_string();
                let size = ["lv_size", "vg_size", "pv_size"]
                    .iter()
                    .find_map(|k| get(k).trim().parse::<u64>().ok());
                let tags_raw = [get("lv_tags"), get("vg_tags")].join(",");
                rows.push(LvRow {
                    name,
                    attr: get("lv_attr").to_string(),
                    size,
                    data_pct: number("data_percent"),
                    meta_pct: number("metadata_percent"),
                    tags: tags_raw
                        .split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(String::from)
                        .collect(),
                });
            }
        }
    }
    Some(rows)
}

/// The logical volume a pool volume is.
pub fn lv_name(volume: &str) -> String {
    format!("dlx-{volume}")
}

/// The tags that stamp a logical volume for `owner`. LVM tags take only
/// `[A-Za-z0-9_+.-/=!:&#]`, so the state root goes in as a digest beside the
/// uid in clear: only the uid separates one user from another.
pub fn stamp_tags(pool: &str, volume: &str, owner: &Owner) -> Result<Vec<String>> {
    let (uid, root) = owner.parts().ok_or_else(|| {
        Error::InvalidPoolRequest(format!("owner '{}' is not <uid>:<root>", owner.id()))
    })?;
    Ok(vec![
        ENGINE_TAG.to_string(),
        format!("delonix.io_uid={uid}"),
        format!("delonix.io_root={}", root_digest(root)),
        format!("delonix.io_pool={pool}"),
        format!("delonix.io_volume={volume}"),
    ])
}

/// Is a logical volume with `tags` the stamped volume of `owner`?
pub fn stamped_for(tags: &[String], pool: &str, volume: &str, owner: &Owner) -> bool {
    match stamp_tags(pool, volume, owner) {
        Ok(want) => want.iter().all(|t| tags.contains(t)),
        Err(_) => false,
    }
}

fn valid_lv_volume(volume: &str) -> Result<()> {
    if !valid_volume_name(volume) {
        return Err(Error::InvalidPoolRequest(format!(
            "'{volume}' is not a volume name a pool can hold"
        )));
    }
    if let Some(r) = RESERVED.iter().find(|r| volume.contains(*r)) {
        return Err(Error::InvalidPoolRequest(format!(
            "volume '{volume}' contains '{r}', which LVM reserves for its own volumes — pick \
             another name"
        )));
    }
    Ok(())
}

impl StoragePoolDriver for LvmThinDriver {
    fn id(&self) -> &'static str {
        ID
    }

    fn probe(&self, pool: &PoolRef) -> PoolProbe {
        let (vg, tp) = match Self::vg_tp(pool) {
            Ok(x) => x,
            Err(e) => {
                return PoolProbe::Unavailable {
                    missing: e.to_string(),
                    remedy: "fix the pool's entry in the allowlist".into(),
                }
            }
        };
        let answer = match self.lv(vg, tp) {
            Ok(a) => a,
            Err(e) => {
                return PoolProbe::Unavailable {
                    missing: e.to_string(),
                    remedy: "install lvm2 and thin-provisioning-tools on the node".into(),
                }
            }
        };
        match answer {
            Answer::Found(row) if row.is_thin_pool() => PoolProbe::Available,
            Answer::Found(row) => PoolProbe::Unavailable {
                missing: format!("{vg}/{tp} is not a thin pool (lv_attr {})", row.attr),
                remedy: "point `thinPool` at a thin pool, or let `create-pool` make one".into(),
            },
            Answer::Missing(why) => PoolProbe::Unavailable {
                missing: format!("{vg}/{tp} does not exist ({why})"),
                remedy: format!(
                    "the administrator creates it: `sudo delonix-storage-helper create-pool {}`",
                    pool.name
                ),
            },
            Answer::Undetermined(reason) => PoolProbe::Undetermined {
                reason,
                remedy: delonix_storage::helper::HELPER_REMEDY.into(),
            },
        }
    }

    fn required(&self, _op: PoolOp) -> Privilege {
        Privilege::Helper
    }

    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation> {
        if req.shape == VolumeShape::Block {
            return Err(Error::PoolUnavailable(format!(
                "storage pool '{}': driver 'lvm-thin' does not hand out block volumes in this \
                 version (VM disks in pools are ADR-0067 P5)",
                pool.name
            )));
        }
        valid_lv_volume(&req.name)?;
        let (vg, tp) = Self::vg_tp(pool)?;
        let (uid, _) = owner.parts().ok_or_else(|| {
            Error::InvalidPoolRequest(format!("owner '{}' is not <uid>:<root>", owner.id()))
        })?;
        let lv = lv_name(&req.name);
        let dev = format!("/dev/{vg}/{lv}");
        let mp = self.mountpoint_of(pool.name, &req.name);
        match self.lv(vg, &lv)? {
            Answer::Found(row) => {
                if !stamped_for(&row.tags, pool.name, &req.name, owner) {
                    return Err(Error::PoolConflict(format!(
                        "{vg}/{lv} exists and carries no stamp of this engine for this state \
                         root — it is never adopted by its name; pick another volume name"
                    )));
                }
                // An apply that died between lvcreate and mkfs left a volume
                // with no filesystem: make it now — nothing is on it.
                let fresh = delonix_storage::admin::signature(self.run.as_ref(), Path::new(&dev))?
                    .is_none();
                let path = self.mount_volume(&dev, &mp, uid, fresh)?;
                return Ok(Allocation {
                    pool: pool.name.to_string(),
                    volume: req.name.clone(),
                    shape: VolumeShape::Filesystem,
                    path,
                    adopted: true,
                });
            }
            Answer::Undetermined(why) => return Err(Error::PoolUndetermined(why)),
            Answer::Missing(_) => {}
        }
        let usage = self.thin_usage(vg, tp)?;
        for (what, pct) in [("data", usage.used_pct()), ("metadata", usage.metadata_pct)] {
            if let Some(p) = pct {
                if p >= REFUSE_ABOVE_PCT {
                    return Err(Error::PoolExhausted(format!(
                        "storage pool '{}': the thin pool's {what} is {p}% full — no new volume \
                         is made above {REFUSE_ABOVE_PCT}% (a full thin pool puts every volume \
                         in it into I/O errors)",
                        pool.name
                    )));
                }
            }
        }
        // LVM sizes a thin volume in extents; asking in whole MiB, rounded
        // up, never gives less than was asked.
        let mib = req.size_bytes.div_ceil(1 << 20).max(1);
        let size = format!("{mib}m");
        let tags = stamp_tags(pool.name, &req.name, owner)?;
        let mut cmd: Vec<&str> = vec![
            "lvcreate",
            "--yes",
            "--type",
            "thin",
            "--thinpool",
            tp,
            "--virtualsize",
            &size,
            "--name",
            &lv,
        ];
        for t in &tags {
            cmd.push("--addtag");
            cmd.push(t);
        }
        cmd.push(vg);
        self.must(&cmd)?;
        match self.mount_volume(&dev, &mp, uid, true) {
            Ok(path) => Ok(Allocation {
                pool: pool.name.to_string(),
                volume: req.name.clone(),
                shape: VolumeShape::Filesystem,
                path,
                adopted: false,
            }),
            Err(e) => {
                // Nothing is on a volume whose filesystem was never made or
                // mounted: take it back rather than leave a stamped shell.
                let _ = self.run(&["lvremove", "--yes", &format!("{vg}/{lv}")]);
                Err(e)
            }
        }
    }

    fn release(&self, pool: &PoolRef, volume: &str, owner: &Owner) -> Result<()> {
        valid_lv_volume(volume)?;
        let (vg, _) = Self::vg_tp(pool)?;
        let lv = lv_name(volume);
        let mp = self.mountpoint_of(pool.name, volume);
        match self.lv(vg, &lv)? {
            Answer::Missing(_) => {
                let _ = std::fs::remove_dir(&mp);
                return Ok(());
            }
            Answer::Undetermined(why) => return Err(Error::PoolUndetermined(why)),
            Answer::Found(row) => {
                if !stamped_for(&row.tags, pool.name, volume, owner) {
                    return Err(Error::PoolConflict(format!(
                        "{vg}/{lv} does not carry this engine's stamp for volume '{volume}' — it \
                         is not released; nothing was deleted"
                    )));
                }
            }
        }
        if self.is_mounted(&mp)? {
            self.must(&["umount", "--", &mp.to_string_lossy()])?;
        }
        self.must(&["lvremove", "--yes", &format!("{vg}/{lv}")])?;
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
        match Self::vg_tp(pool) {
            Ok((vg, tp)) => self.thin_usage(vg, tp).unwrap_or_default(),
            Err(_) => PoolUsage::default(),
        }
    }
}

impl PoolAdmin for LvmThinDriver {
    fn id(&self) -> &'static str {
        ID
    }

    fn create_pool(&self, pool: &PoolRef, devices: &[PathBuf]) -> Result<String> {
        let (vg, tp) = Self::vg_tp(pool)?;
        let vgs = self.run(&["vgs", "--reportformat", "json", "-o", "vg_name", vg])?;
        if vgs.success() && !lacks_privilege(&vgs) {
            return Err(Error::DeviceInUse(format!(
                "volume group '{vg}' already exists — declare the pool without `create:` to \
                 use it as it is"
            )));
        }
        let devs: Vec<String> = devices
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        let mut pv: Vec<&str> = vec!["pvcreate", "--yes"];
        pv.extend(devs.iter().map(String::as_str));
        self.must(&pv)?;
        let mut vgc: Vec<&str> = vec!["vgcreate", "--addtag", CREATED_TAG, vg];
        vgc.extend(devs.iter().map(String::as_str));
        self.must(&vgc)?;
        self.must(&[
            "lvcreate",
            "--yes",
            "--type",
            "thin-pool",
            "--extents",
            "90%FREE",
            "--addtag",
            CREATED_TAG,
            "--name",
            tp,
            vg,
        ])?;
        Ok(format!(
            "storage pool '{}': thin pool {vg}/{tp} created on {} (90% of the volume group; the \
             rest is left for metadata growth and autoextend)",
            pool.name,
            devs.join(", ")
        ))
    }

    fn created_here(&self, pool: &PoolRef) -> Result<bool> {
        let (vg, _) = Self::vg_tp(pool)?;
        let out = self.run(&["vgs", "--reportformat", "json", "-o", "vg_name,vg_tags", vg])?;
        match classify(&out) {
            Answer::Found(rows) => Ok(rows.iter().any(|r| r.tags.iter().any(|t| t == CREATED_TAG))),
            Answer::Missing(_) => Ok(false),
            Answer::Undetermined(why) => Err(Error::PoolUndetermined(why)),
        }
    }

    fn contents(&self, pool: &PoolRef) -> Result<Vec<String>> {
        let (vg, tp) = Self::vg_tp(pool)?;
        let out = self.run(&["lvs", "--reportformat", "json", "-o", "lv_name", vg])?;
        match classify(&out) {
            Answer::Found(rows) => Ok(rows
                .into_iter()
                .map(|r| r.name)
                .filter(|n| n != tp)
                .collect()),
            Answer::Missing(why) => Err(Error::PoolUnavailable(why)),
            Answer::Undetermined(why) => Err(Error::PoolUndetermined(why)),
        }
    }

    fn destroy_pool(&self, pool: &PoolRef) -> Result<String> {
        let (vg, tp) = Self::vg_tp(pool)?;
        // The physical volumes are read from LVM itself, before anything goes:
        // the allowlist may name other devices by now.
        let out = self.run(&[
            "pvs",
            "--reportformat",
            "json",
            "-o",
            "pv_name",
            "--select",
            &format!("vg_name={vg}"),
        ])?;
        let pvs: Vec<String> = match classify(&out) {
            Answer::Found(rows) => rows.into_iter().map(|r| r.name).collect(),
            Answer::Missing(why) | Answer::Undetermined(why) => {
                return Err(Error::PoolUndetermined(format!(
                    "the physical volumes of '{vg}' could not be read: {why}"
                )))
            }
        };
        self.must(&["lvremove", "--yes", &format!("{vg}/{tp}")])?;
        self.must(&["vgremove", "--yes", vg])?;
        let mut pr: Vec<&str> = vec!["pvremove", "--yes"];
        pr.extend(pvs.iter().map(String::as_str));
        self.must(&pr)?;
        Ok(format!(
            "storage pool '{}': thin pool {vg}/{tp}, volume group {vg} and physical volumes {} \
             removed",
            pool.name,
            pvs.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests;

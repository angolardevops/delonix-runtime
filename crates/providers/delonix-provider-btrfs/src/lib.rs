//! The `btrfs` storage pool driver (ADR-0067 D1, phase P2).
//!
//! A pool is a btrfs filesystem the administrator mounted at `path`, where the
//! engine's user can write. A container volume is a subvolume `<path>/<volume>`
//! holding the owner stamp (`.delonix-volume.json`) and `_data`.
//!
//! Everything runs in the engine's own process, unprivileged: creating a
//! subvolume needs only write access to its parent, and removing an EMPTY one
//! is a plain `rmdir` (kernel ≥ 4.18); a non-empty delete needs the pool to be
//! mounted with `user_subvol_rm_allowed`, which `create-pool` does. The hard
//! quota (a qgroup limit) is an administrative ioctl: it is applied only when
//! the engine runs as root (`mode: in-process`); otherwise a pool volume's
//! size is a monitored quota, like a `dir` pool's. btrfs cannot be mounted
//! inside a user namespace, so the pool is always the administrator's mount.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use delonix_storage::admin::PoolAdmin;
use delonix_storage::exec::{argv, Output, Run};
use delonix_storage::mounts::{self, DATA_DIR};
use delonix_storage::ownership::{Owner, Stamp, STAMP_FILE};
use delonix_storage::pool::{
    valid_volume_name, Allocation, PoolOp, PoolProbe, PoolRef, PoolUsage, Privilege,
    StoragePoolDriver, VolumeRequest, VolumeShape,
};
use delonix_storage::{Error, Result};

/// The driver's id in the allowlist.
pub const ID: &str = "btrfs";

/// `BTRFS_SUPER_MAGIC` (`statfs(2)`).
pub const BTRFS_MAGIC: i64 = 0x9123_683E;

/// The inode number of a subvolume's root (`BTRFS_FIRST_FREE_OBJECTID`).
pub const SUBVOLUME_INO: u64 = 256;

/// A NEW volume is refused above this share of the filesystem.
pub const REFUSE_ABOVE_PCT: u8 = 95;

/// The file `create-pool` leaves at the filesystem's root.
pub const POOL_MARK: &str = ".delonix-pool.json";

/// Removes a tree this process cannot unlink directly (a container in a
/// mapped user namespace wrote it as a sub-uid); handed in by the composition
/// root, like the `dir` driver's.
pub type RemoveTree = fn(&Path);

/// The driver.
pub struct BtrfsDriver {
    run: Arc<dyn Run>,
    rmtree: Option<RemoveTree>,
    /// Set the qgroup limit of each new volume: only when the engine runs as
    /// root (`mode: in-process`) — `qgroup limit` is an administrative ioctl.
    qgroups: bool,
    magic: fn(&Path) -> Option<i64>,
    subvolume: fn(&Path) -> bool,
    fill: fn(&Path) -> PoolUsage,
}

impl BtrfsDriver {
    pub fn new(run: Arc<dyn Run>, rmtree: Option<RemoveTree>, qgroups: bool) -> Self {
        Self {
            run,
            rmtree,
            qgroups,
            magic: mounts::fs_magic,
            subvolume: is_subvolume,
            fill: mounts::statvfs_usage,
        }
    }

    /// The same driver, with the filesystem-type probe replaced (tests: a
    /// temporary directory is never a btrfs).
    pub fn with_magic(mut self, magic: fn(&Path) -> Option<i64>) -> Self {
        self.magic = magic;
        self
    }

    /// The same driver, with the «is this a subvolume?» check replaced
    /// (tests: only a real btrfs gives a directory inode 256).
    pub fn with_subvolume_check(mut self, subvolume: fn(&Path) -> bool) -> Self {
        self.subvolume = subvolume;
        self
    }

    /// The same driver, with the filesystem fill measure replaced (tests).
    pub fn with_fill(mut self, fill: fn(&Path) -> PoolUsage) -> Self {
        self.fill = fill;
        self
    }

    fn root_of(pool: &PoolRef) -> Result<PathBuf> {
        pool.entry.path.clone().ok_or_else(|| {
            Error::InvalidAllowlist(format!("pool '{}': driver 'btrfs' needs `path`", pool.name))
        })
    }

    fn run(&self, parts: &[&str]) -> Result<Output> {
        self.run.run(&argv(parts)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::PoolUnavailable(format!(
                    "`{}` is not installed — install btrfs-progs",
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

    fn stamp(&self, dir: &Path, pool: &str, volume: &str, owner: &Owner) -> Result<()> {
        let s = Stamp::new(pool, volume, owner, now_unix());
        mounts::write_file_atomic(&dir.join(STAMP_FILE), &s.encode(), 0o644)
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_stamp(dir: &Path) -> Option<Stamp> {
    Stamp::decode(&std::fs::read(dir.join(STAMP_FILE)).ok()?)
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
}

/// Is `dir` the root of a subvolume? Its inode is always 256.
pub fn is_subvolume(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(dir)
        .map(|m| m.is_dir() && m.ino() == SUBVOLUME_INO)
        .unwrap_or(false)
}

fn can_write(dir: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
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

/// The devices of a mounted btrfs, from `btrfs filesystem show --raw`: one
/// line per device, `devid <n> size <bytes> used <bytes> path <device>`
/// (`btrfs-filesystem(8)`).
pub fn parse_show_devices(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter(|l| l.trim_start().starts_with("devid"))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            while let Some(w) = it.next() {
                if w == "path" {
                    return it.next().map(PathBuf::from);
                }
            }
            None
        })
        .collect()
}

impl StoragePoolDriver for BtrfsDriver {
    fn id(&self) -> &'static str {
        ID
    }

    fn probe(&self, pool: &PoolRef) -> PoolProbe {
        let root = match Self::root_of(pool) {
            Ok(r) => r,
            Err(e) => {
                return PoolProbe::Unavailable {
                    missing: e.to_string(),
                    remedy: "fix the pool's entry in the allowlist".into(),
                }
            }
        };
        match std::fs::metadata(&root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return PoolProbe::Unavailable {
                    missing: format!("{} does not exist", root.display()),
                    remedy: format!(
                        "the administrator creates and mounts it: `sudo delonix-storage-helper \
                         create-pool {}`",
                        pool.name
                    ),
                }
            }
            Err(e) => {
                return PoolProbe::Undetermined {
                    reason: format!("{} could not be examined: {e}", root.display()),
                    remedy: "give this user search permission on the directories above it".into(),
                }
            }
            Ok(m) if !m.is_dir() => {
                return PoolProbe::Unavailable {
                    missing: format!("{} is not a directory", root.display()),
                    remedy: "point the pool's `path` at the btrfs mount".into(),
                }
            }
            Ok(_) => {}
        }
        if (self.magic)(&root) != Some(BTRFS_MAGIC) {
            return PoolProbe::Unavailable {
                missing: format!("{} is not a mounted btrfs filesystem", root.display()),
                remedy: "the administrator mounts the pool's btrfs there (it cannot be mounted \
                         rootless)"
                    .into(),
            };
        }
        if !can_write(&root) {
            return PoolProbe::Unavailable {
                missing: format!("this user cannot write to {}", root.display()),
                remedy: format!(
                    "the administrator grants it: `sudo chown $USER {}`",
                    root.display()
                ),
            };
        }
        if let Err(e) = self.run(&["btrfs", "--version"]) {
            return PoolProbe::Unavailable {
                missing: e.to_string(),
                remedy: "the node's administrator installs it".into(),
            };
        }
        PoolProbe::Available
    }

    fn required(&self, _op: PoolOp) -> Privilege {
        Privilege::Unprivileged
    }

    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation> {
        if req.shape == VolumeShape::Block {
            return Err(Error::PoolUnavailable(format!(
                "storage pool '{}': driver 'btrfs' does not hand out block volumes in this \
                 version (VM disks in pools are ADR-0067 P5)",
                pool.name
            )));
        }
        check_volume(&req.name)?;
        let (uid, _) = owner.parts().ok_or_else(|| {
            Error::InvalidPoolRequest(format!("owner '{}' is not <uid>:<root>", owner.id()))
        })?;
        let root = Self::root_of(pool)?;
        let dir = root.join(&req.name);
        let mut adopted = false;
        if std::fs::symlink_metadata(&dir).is_ok() {
            match read_stamp(&dir) {
                Some(s) if s.is_for(pool.name, &req.name, owner) => adopted = true,
                Some(s) => {
                    return Err(Error::PoolConflict(format!(
                        "storage pool '{}' already holds a volume '{}' that belongs to another \
                         engine state root ({}) — pick another name",
                        pool.name, req.name, s.owner
                    )))
                }
                // An apply that died between `subvolume create` and the stamp
                // left an empty subvolume: nothing in it is anyone's.
                None if (self.subvolume)(&dir) && is_empty_dir(&dir) => {
                    self.stamp(&dir, pool.name, &req.name, owner)?;
                }
                None => {
                    return Err(Error::PoolConflict(format!(
                        "{} exists and carries no stamp of this engine — it is never adopted by \
                         its name; pick another volume name or have its owner remove it",
                        dir.display()
                    )))
                }
            }
        } else {
            if let Some(p) = (self.fill)(&root).used_pct() {
                if p >= REFUSE_ABOVE_PCT {
                    return Err(Error::PoolExhausted(format!(
                        "storage pool '{}' is {p}% full — no new volume is made above \
                         {REFUSE_ABOVE_PCT}%",
                        pool.name
                    )));
                }
            }
            self.must(&["btrfs", "subvolume", "create", &dir.to_string_lossy()])?;
            if let Err(e) = self.stamp(&dir, pool.name, &req.name, owner) {
                // An unstamped empty subvolume is taken by the next apply, but
                // nothing is left to chance: it is empty, so it goes.
                let _ = std::fs::remove_dir(&dir);
                return Err(e);
            }
            if self.qgroups {
                // A qgroup limit is refused unless quota accounting is enabled
                // for the whole filesystem. `create-pool` turns it on, but a
                // pool the administrator mounted by hand (the common case for an
                // existing btrfs) has it off, and the limit then fails with
                // "quota not enabled". Enable it here, on the filesystem root —
                // idempotent and instant when already on (measured), and it
                // creates the qgroup for the subvolume made just above.
                self.must(&["btrfs", "quota", "enable", &root.to_string_lossy()])?;
                let size = req.size_bytes.to_string();
                self.must(&["btrfs", "qgroup", "limit", &size, &dir.to_string_lossy()])?;
            }
        }
        let path = mounts::prepare_data_dir(&dir, uid)?;
        debug_assert_eq!(path, dir.join(DATA_DIR));
        Ok(Allocation {
            pool: pool.name.to_string(),
            volume: req.name.clone(),
            shape: VolumeShape::Filesystem,
            path,
            adopted,
        })
    }

    fn release(&self, pool: &PoolRef, volume: &str, owner: &Owner) -> Result<()> {
        check_volume(volume)?;
        let dir = Self::root_of(pool)?.join(volume);
        if std::fs::symlink_metadata(&dir).is_err() {
            return Ok(());
        }
        match read_stamp(&dir) {
            Some(s) if s.is_for(pool.name, volume, owner) => {}
            _ => {
                return Err(Error::PoolConflict(format!(
                    "{} does not carry this engine's stamp for volume '{volume}' — it is not \
                     released; nothing was deleted",
                    dir.display()
                )))
            }
        }
        // The data first, the stamp last, the subvolume after: a release that
        // fails half-way leaves a volume still recognisably this engine's.
        let data = dir.join(DATA_DIR);
        if data.exists() {
            if std::fs::remove_dir_all(&data).is_err() {
                if let Some(rm) = self.rmtree {
                    rm(&data);
                }
            }
            if data.exists() {
                std::fs::remove_dir_all(&data).map_err(|e| {
                    Error::PoolUnavailable(format!("{} cannot be removed: {e}", data.display()))
                })?;
            }
        }
        std::fs::remove_file(dir.join(STAMP_FILE)).map_err(|e| {
            Error::PoolUnavailable(format!("{} cannot be unstamped: {e}", dir.display()))
        })?;
        // An empty subvolume is an `rmdir` for its owner (kernel ≥ 4.18); a
        // plain `remove_dir` also stops at anything this driver did not make.
        if std::fs::remove_dir(&dir).is_err() {
            self.must(&["btrfs", "subvolume", "delete", &dir.to_string_lossy()])?;
        }
        Ok(())
    }

    fn usage(&self, pool: &PoolRef) -> PoolUsage {
        match Self::root_of(pool) {
            Ok(root) => (self.fill)(&root),
            Err(_) => PoolUsage::default(),
        }
    }
}

/// The creation mark `create-pool` writes at the filesystem's root.
#[derive(serde::Serialize, serde::Deserialize)]
struct PoolMark {
    engine: String,
    pool: String,
}

impl PoolAdmin for BtrfsDriver {
    fn id(&self) -> &'static str {
        ID
    }

    fn create_pool(&self, pool: &PoolRef, devices: &[PathBuf]) -> Result<String> {
        let root = Self::root_of(pool)?;
        if root.exists() && !is_empty_dir(&root) {
            return Err(Error::DeviceInUse(format!(
                "{} exists and is not empty — the pool is mounted on an empty directory",
                root.display()
            )));
        }
        let devs: Vec<String> = devices
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        let label = format!("dlx-{}", pool.name);
        let mut mk: Vec<&str> = vec!["mkfs.btrfs", "-q", "-L", &label];
        mk.extend(devs.iter().map(String::as_str));
        self.must(&mk)?;
        mounts::ensure_dir(&root)?;
        let first = devs.first().map(String::as_str).unwrap_or_default();
        self.must(&[
            "mount",
            "-t",
            "btrfs",
            "-o",
            "user_subvol_rm_allowed,nodev,nosuid",
            "--",
            first,
            &root.to_string_lossy(),
        ])?;
        let mark = serde_json::to_vec_pretty(&PoolMark {
            engine: "delonix".into(),
            pool: pool.name.into(),
        })
        .unwrap_or_default();
        mounts::write_file_atomic(&root.join(POOL_MARK), &mark, 0o644)?;
        let uuid = self
            .run(&["blkid", "-s", "UUID", "-o", "value", first])
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default();
        Ok(format!(
            "storage pool '{}': btrfs on {} mounted at {}. Make the mount survive a reboot with \
             this /etc/fstab line:\n  UUID={uuid} {} btrfs user_subvol_rm_allowed,nodev,nosuid 0 \
             0\nand give the engine's user the directory: `chown <user> {}`",
            pool.name,
            devs.join(", "),
            root.display(),
            root.display(),
            root.display()
        ))
    }

    fn created_here(&self, pool: &PoolRef) -> Result<bool> {
        let root = Self::root_of(pool)?;
        let Ok(bytes) = std::fs::read(root.join(POOL_MARK)) else {
            return Ok(false);
        };
        Ok(serde_json::from_slice::<PoolMark>(&bytes)
            .map(|m| m.engine == "delonix" && m.pool == pool.name)
            .unwrap_or(false))
    }

    fn contents(&self, pool: &PoolRef) -> Result<Vec<String>> {
        let root = Self::root_of(pool)?;
        let mut names: Vec<String> = std::fs::read_dir(&root)
            .map_err(|e| Error::PoolUndetermined(format!("{}: {e}", root.display())))?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != POOL_MARK)
            .collect();
        names.sort();
        Ok(names)
    }

    fn destroy_pool(&self, pool: &PoolRef) -> Result<String> {
        let root = Self::root_of(pool)?;
        let root_s = root.to_string_lossy().into_owned();
        // The devices come from the mounted filesystem itself, before it goes.
        let shown = self.must(&["btrfs", "filesystem", "show", "--raw", &root_s])?;
        let devs = parse_show_devices(&shown.stdout);
        if devs.is_empty() {
            return Err(Error::PoolUndetermined(format!(
                "the devices of {root_s} could not be read from `btrfs filesystem show`"
            )));
        }
        self.must(&["umount", "--", &root_s])?;
        for d in &devs {
            self.must(&["wipefs", "--all", "--", &d.to_string_lossy()])?;
        }
        let _ = std::fs::remove_dir(&root);
        Ok(format!(
            "storage pool '{}': btrfs unmounted from {root_s} and wiped from {} — remove its \
             /etc/fstab line",
            pool.name,
            devs.iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

#[cfg(test)]
mod tests;

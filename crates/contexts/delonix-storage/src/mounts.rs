//! The filesystem side every pool driver shares: where a volume's data lives
//! inside what the driver made, who owns it, and how full a mounted
//! filesystem is.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::pool::PoolUsage;
use crate::{Error, Result};

/// The directory inside a pool volume that holds its data — what a container
/// mounts. The volume's root keeps the backend's own entries (`lost+found`, a
/// stamp file) out of the container's sight.
pub const DATA_DIR: &str = "_data";

fn io(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::PoolUnavailable(format!("{what} {}: {e}", path.display()))
}

/// Where a privileged driver mounts volume `volume` of `pool`: one level per
/// name under the helper's fixed root. Both names were validated as single
/// path components before they got here.
pub fn mountpoint(root: &Path, pool: &str, volume: &str) -> PathBuf {
    root.join(pool).join(volume)
}

/// Creates `dir` and its parents with mode 0755 (traversable by the engine's
/// user, writable only by its owner). An existing directory is kept.
pub fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .map_err(|e| io("cannot create", dir, e))
}

/// The data directory inside a mounted volume, owned by `uid` (and its
/// primary group) so a rootless engine can write into it. A `_data` that is a
/// symlink or not a directory is refused — the helper chowns it as root, and
/// must never follow a link the volume's user could have planted.
pub fn prepare_data_dir(volume_root: &Path, uid: u32) -> Result<PathBuf> {
    let data = volume_root.join(DATA_DIR);
    match std::fs::symlink_metadata(&data) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            return Err(Error::PoolConflict(format!(
                "{} is not a plain directory — it is never followed or reused",
                data.display()
            )))
        }
        // Already there: its owner is whoever the volume was made for, and is
        // left alone (the data under it may be a container's sub-uids).
        Ok(_) => return Ok(data),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io("cannot examine", &data, e)),
    }
    std::fs::DirBuilder::new()
        .mode(0o755)
        .create(&data)
        .map_err(|e| io("cannot create", &data, e))?;
    let gid = crate::ownership::primary_gid(uid);
    std::os::unix::fs::lchown(&data, Some(uid), Some(gid))
        .map_err(|e| io("cannot hand to the volume's user", &data, e))?;
    Ok(data)
}

/// Capacity and use of the filesystem holding `path`, as `df` shows them:
/// used over used-plus-available-to-this-user. `None` fields when it cannot be
/// read — never zero.
pub fn statvfs_usage(path: &Path) -> PoolUsage {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return PoolUsage::default();
    };
    // SAFETY: `statvfs` is a C struct of integers; all-zero is a valid value.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return PoolUsage::default();
    }
    let frsize = st.f_frsize as u64;
    let used = (st.f_blocks as u64).saturating_sub(st.f_bfree as u64) * frsize;
    let avail = st.f_bavail as u64 * frsize;
    PoolUsage {
        capacity_bytes: Some(used + avail),
        used_bytes: Some(used),
        metadata_pct: None,
    }
}

/// The filesystem type magic of `path` (`statfs(2)`'s `f_type`).
pub fn fs_magic(path: &Path) -> Option<i64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statfs` is a C struct of integers; all-zero is a valid value.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a valid out-pointer.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some(st.f_type as i64)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[test]
    fn the_data_dir_is_made_once_and_handed_to_the_volumes_user() {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: no preconditions.
        let me = unsafe { libc::geteuid() };
        let d = prepare_data_dir(tmp.path(), me).unwrap();
        assert_eq!(d, tmp.path().join(DATA_DIR));
        let m = std::fs::metadata(&d).unwrap();
        assert_eq!(m.uid(), me);
        assert_eq!(m.mode() & 0o777, 0o755);
        // Again: kept as it is.
        std::fs::write(d.join("f"), b"x").unwrap();
        prepare_data_dir(tmp.path(), me).unwrap();
        assert!(d.join("f").exists());
    }

    #[test]
    fn a_planted_symlink_is_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, tmp.path().join(DATA_DIR)).unwrap();
        // SAFETY: no preconditions.
        let me = unsafe { libc::geteuid() };
        assert_eq!(prepare_data_dir(tmp.path(), me).unwrap_err().number(), 5201);
        std::fs::write(tmp.path().join("plain"), b"").unwrap();
        let other = tmp.path().join("v2");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join(DATA_DIR), b"").unwrap();
        assert_eq!(prepare_data_dir(&other, me).unwrap_err().number(), 5201);
    }

    #[test]
    fn usage_of_a_real_directory_is_measured_and_of_a_missing_one_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let u = statvfs_usage(tmp.path());
        assert!(u.capacity_bytes.unwrap() > 0);
        assert_eq!(
            statvfs_usage(&tmp.path().join("nope")),
            PoolUsage::default()
        );
        assert!(fs_magic(tmp.path()).is_some());
    }

    #[test]
    fn a_mountpoint_is_one_level_per_name_under_the_root() {
        assert_eq!(
            mountpoint(Path::new("/var/lib/delonix-storage/mounts"), "fast", "db"),
            PathBuf::from("/var/lib/delonix-storage/mounts/fast/db")
        );
    }
}

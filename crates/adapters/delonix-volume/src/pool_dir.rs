//! The `dir` storage pool driver (ADR-0067 D1): a directory the administrator
//! prepared, with one sub-directory per volume.
//!
//! ```text
//! <path>/<volume>/.delonix-volume.json   the owner stamp
//! <path>/<volume>/_data/                 what a container mounts
//! ```
//!
//! Everything runs in the engine's own process — the directory is one the
//! user can write, or the pool is not usable and the probe says so. There is
//! no hard size limit here: a volume's size is a monitored quota, the same as
//! a local volume's in a rootless session.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use delonix_storage::ownership::{Owner, Stamp, STAMP_FILE};
use delonix_storage::pool::{
    Allocation, PoolOp, PoolProbe, PoolRef, PoolUsage, Privilege, StoragePoolDriver, VolumeRequest,
    VolumeShape,
};
use delonix_storage::{Error, Result};

/// The directory inside a pool volume that holds its data.
pub const DATA_DIR: &str = "_data";

/// Removes a tree this process cannot unlink directly — a container in a
/// mapped user namespace wrote it as a sub-uid. The composition root hands in
/// the engine's mapped remover; without one, such a tree is a clean error.
pub type RemoveTree = fn(&Path);

/// The driver. Stateless but for the injected tree remover.
pub struct DirPoolDriver {
    rmtree: Option<RemoveTree>,
}

impl DirPoolDriver {
    pub fn new(rmtree: Option<RemoveTree>) -> Self {
        Self { rmtree }
    }
}

/// The pool's directory. The allowlist parser refuses a `dir` entry without
/// one, so its absence here is a damaged entry, not a state to work around.
fn root_of(pool: &PoolRef) -> Result<PathBuf> {
    pool.entry.path.clone().ok_or_else(|| {
        Error::InvalidAllowlist(format!("pool '{}': driver 'dir' needs `path`", pool.name))
    })
}

/// A volume name as ONE path component of the pool. The volume store already
/// validates names; the driver checks again because it is the one that joins
/// the name onto the administrator's directory.
fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.starts_with('-')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn can_write(dir: &Path) -> bool {
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

fn read_stamp(dir: &Path) -> Option<Stamp> {
    Stamp::decode(&std::fs::read(dir.join(STAMP_FILE)).ok()?)
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn io(e: std::io::Error) -> Error {
    Error::Engine(delonix_model::Error::Io(e))
}

impl StoragePoolDriver for DirPoolDriver {
    fn id(&self) -> &'static str {
        "dir"
    }

    fn probe(&self, pool: &PoolRef) -> PoolProbe {
        let root = match root_of(pool) {
            Ok(r) => r,
            Err(e) => {
                return PoolProbe::Unavailable {
                    missing: e.to_string(),
                    remedy: "fix the pool's entry in the allowlist".into(),
                }
            }
        };
        match std::fs::metadata(&root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PoolProbe::Unavailable {
                missing: format!("{} does not exist", root.display()),
                remedy: format!(
                    "the administrator creates it: `sudo install -d -o $USER {}`",
                    root.display()
                ),
            },
            // Not «absent»: this process could not look.
            Err(e) => PoolProbe::Undetermined {
                reason: format!("{} could not be examined: {e}", root.display()),
                remedy: format!(
                    "give this user search permission on the directories above {}",
                    root.display()
                ),
            },
            Ok(m) if !m.is_dir() => PoolProbe::Unavailable {
                missing: format!("{} is not a directory", root.display()),
                remedy: "point the pool's `path` at a directory".into(),
            },
            Ok(_) if !can_write(&root) => PoolProbe::Unavailable {
                missing: format!("this user cannot write to {}", root.display()),
                remedy: format!(
                    "the administrator grants it: `sudo chown $USER {}`",
                    root.display()
                ),
            },
            Ok(_) => PoolProbe::Available,
        }
    }

    fn required(&self, _op: PoolOp) -> Privilege {
        Privilege::Unprivileged
    }

    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation> {
        if req.shape == VolumeShape::Block {
            return Err(Error::PoolUnavailable(format!(
                "storage pool '{}': driver 'dir' does not hand out block volumes in this version \
                 (VM disks in pools are ADR-0067 P5)",
                pool.name
            )));
        }
        if !safe_component(&req.name) {
            return Err(Error::InvalidPoolRequest(format!(
                "'{}' is not a volume name a pool can hold",
                req.name
            )));
        }
        let dir = root_of(pool)?.join(&req.name);
        let data = dir.join(DATA_DIR);
        let mut adopted = false;
        if dir.exists() {
            match read_stamp(&dir) {
                Some(s) if s.is_for(pool.name, &req.name, owner) => adopted = true,
                Some(s) => {
                    return Err(Error::PoolConflict(format!(
                        "storage pool '{}' already holds a volume '{}' that belongs to another \
                         engine state root ({}) — pick another name",
                        pool.name, req.name, s.owner
                    )))
                }
                // An apply that died between `mkdir` and the stamp left an
                // empty directory: nothing in it is anyone's, so it is taken.
                None if is_empty_dir(&dir) => {}
                None => {
                    return Err(Error::PoolConflict(format!(
                        "{} exists and carries no stamp of this engine — it is never adopted by \
                         its name; pick another volume name or have its owner remove it",
                        dir.display()
                    )))
                }
            }
        } else {
            // `create_dir`, not `create_dir_all`: the pool's own directory is
            // the administrator's to make, and its absence is the probe's answer.
            std::fs::create_dir(&dir).map_err(io)?;
        }
        if !adopted {
            // The stamp BEFORE the data directory: a volume with data and no
            // stamp is one no later apply could tell from someone else's.
            let stamp = Stamp::new(pool.name, &req.name, owner, now_unix());
            delonix_state::write_atomic(&dir.join(STAMP_FILE), &stamp.encode()).map_err(|e| {
                Error::Engine(delonix_model::Error::Invalid(format!(
                    "could not stamp {}: {e}",
                    dir.display()
                )))
            })?;
        }
        if !data.is_dir() {
            std::fs::create_dir(&data).map_err(io)?;
        }
        Ok(Allocation {
            pool: pool.name.to_string(),
            volume: req.name.clone(),
            shape: VolumeShape::Filesystem,
            path: data,
            adopted,
        })
    }

    fn release(&self, pool: &PoolRef, volume: &str, owner: &Owner) -> Result<()> {
        if !safe_component(volume) {
            return Err(Error::InvalidPoolRequest(format!(
                "'{volume}' is not a volume name a pool can hold"
            )));
        }
        let dir = root_of(pool)?.join(volume);
        if !dir.exists() {
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
        // The data first, the stamp last: a release that fails half-way leaves
        // a volume that is still recognisably this engine's, so the next
        // attempt can finish it.
        let data = dir.join(DATA_DIR);
        if data.exists() {
            if std::fs::remove_dir_all(&data).is_err() {
                if let Some(rm) = self.rmtree {
                    rm(&data);
                }
            }
            if data.exists() {
                // Whatever is left says why: usually a sub-uid's files and no
                // mapped remover.
                std::fs::remove_dir_all(&data).map_err(io)?;
            }
        }
        std::fs::remove_file(dir.join(STAMP_FILE)).map_err(io)?;
        // `remove_dir`, so anything in there that this driver did not create
        // stops the removal instead of going with it.
        std::fs::remove_dir(&dir).map_err(io)
    }

    fn usage(&self, pool: &PoolRef) -> PoolUsage {
        let Ok(root) = root_of(pool) else {
            return PoolUsage::default();
        };
        let Ok(c) = CString::new(root.as_os_str().as_bytes()) else {
            return PoolUsage::default();
        };
        // SAFETY: `statvfs` is a C struct of integers; all-zero is a valid value.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a valid NUL-terminated path and `st` a valid out-pointer.
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return PoolUsage::default();
        }
        // The figures `df` shows: used over used-plus-available-to-this-user.
        // Counting root's reserved blocks as free would call a pool emptier
        // than any write by this user will find it.
        let frsize = st.f_frsize as u64;
        let used = (st.f_blocks as u64).saturating_sub(st.f_bfree as u64) * frsize;
        let avail = st.f_bavail as u64 * frsize;
        PoolUsage {
            capacity_bytes: Some(used + avail),
            used_bytes: Some(used),
            metadata_pct: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delonix_storage::allowlist::Entry;

    fn entry(path: &Path) -> Entry {
        Entry {
            driver: "dir".into(),
            path: Some(path.to_path_buf()),
            ..Default::default()
        }
    }

    fn req(name: &str) -> VolumeRequest {
        VolumeRequest {
            name: name.into(),
            shape: VolumeShape::Filesystem,
            size_bytes: 1 << 20,
        }
    }

    #[test]
    fn the_probe_has_three_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let d = DirPoolDriver::new(None);
        let e = entry(tmp.path());
        let pool = PoolRef {
            name: "p",
            entry: &e,
        };
        assert_eq!(d.probe(&pool), PoolProbe::Available);

        let gone = entry(&tmp.path().join("nope"));
        let p = d.probe(&PoolRef {
            name: "p",
            entry: &gone,
        });
        assert!(
            matches!(p, PoolProbe::Unavailable { ref missing, .. } if missing.contains("does not exist")),
            "{p:?}"
        );

        let file = tmp.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let not_dir = entry(&file);
        let p = d.probe(&PoolRef {
            name: "p",
            entry: &not_dir,
        });
        assert!(
            matches!(p, PoolProbe::Unavailable { ref missing, .. } if missing.contains("not a directory")),
            "{p:?}"
        );

        // A path under a FILE cannot be examined at all (ENOTDIR): that is
        // «could not find out», never «absent».
        let under = entry(&file.join("x"));
        let p = d.probe(&PoolRef {
            name: "p",
            entry: &under,
        });
        assert!(matches!(p, PoolProbe::Undetermined { .. }), "{p:?}");
    }

    #[test]
    fn allocate_stamps_then_adopts_its_own_and_refuses_anyone_elses() {
        let tmp = tempfile::tempdir().unwrap();
        let d = DirPoolDriver::new(None);
        let e = entry(tmp.path());
        let pool = PoolRef {
            name: "media",
            entry: &e,
        };
        let me = Owner::new("1000:/a");

        let a = d.allocate(&pool, &req("db"), &me).unwrap();
        assert!(!a.adopted);
        assert_eq!(a.path, tmp.path().join("db/_data"));
        assert!(a.path.is_dir());
        let s = read_stamp(&tmp.path().join("db")).unwrap();
        assert!(s.is_for("media", "db", &me));

        // Again, with data in it: taken back, nothing recreated.
        std::fs::write(a.path.join("f"), b"x").unwrap();
        let again = d.allocate(&pool, &req("db"), &me).unwrap();
        assert!(again.adopted);
        assert_eq!(std::fs::read(a.path.join("f")).unwrap(), b"x");

        // Another state root asking for the same name is refused.
        let other = d
            .allocate(&pool, &req("db"), &Owner::new("1000:/b"))
            .unwrap_err();
        assert!(matches!(other, Error::PoolConflict(_)), "{other}");

        // A directory that was there before, with content and no stamp.
        std::fs::create_dir(tmp.path().join("old")).unwrap();
        std::fs::write(tmp.path().join("old/keep"), b"k").unwrap();
        let foreign = d.allocate(&pool, &req("old"), &me).unwrap_err();
        assert!(matches!(foreign, Error::PoolConflict(_)), "{foreign}");
        assert_eq!(std::fs::read(tmp.path().join("old/keep")).unwrap(), b"k");

        // An EMPTY leftover directory is nobody's, and is taken.
        std::fs::create_dir(tmp.path().join("half")).unwrap();
        assert!(!d.allocate(&pool, &req("half"), &me).unwrap().adopted);
    }

    #[test]
    fn a_block_volume_and_a_name_that_is_not_one_component_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let d = DirPoolDriver::new(None);
        let e = entry(tmp.path());
        let pool = PoolRef {
            name: "p",
            entry: &e,
        };
        let me = Owner::new("o");
        let block = VolumeRequest {
            shape: VolumeShape::Block,
            ..req("disk")
        };
        assert!(matches!(
            d.allocate(&pool, &block, &me),
            Err(Error::PoolUnavailable(_))
        ));
        for bad in ["../x", "a/b", ".hid", "", ".."] {
            assert!(
                matches!(
                    d.allocate(&pool, &req(bad), &me),
                    Err(Error::InvalidPoolRequest(_))
                ),
                "{bad}"
            );
            assert!(
                matches!(
                    d.release(&pool, bad, &me),
                    Err(Error::InvalidPoolRequest(_))
                ),
                "{bad}"
            );
        }
        assert!(is_empty_dir(tmp.path()));
    }

    #[test]
    fn release_deletes_only_what_carries_this_owners_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let d = DirPoolDriver::new(None);
        let e = entry(tmp.path());
        let pool = PoolRef {
            name: "p",
            entry: &e,
        };
        let me = Owner::new("me");

        let a = d.allocate(&pool, &req("v"), &me).unwrap();
        std::fs::write(a.path.join("f"), b"x").unwrap();

        // Someone else's release of the same name deletes nothing.
        let e1 = d.release(&pool, "v", &Owner::new("other")).unwrap_err();
        assert!(matches!(e1, Error::PoolConflict(_)), "{e1}");
        assert!(a.path.join("f").exists());

        // An unstamped directory is never released.
        std::fs::create_dir(tmp.path().join("plain")).unwrap();
        std::fs::write(tmp.path().join("plain/keep"), b"k").unwrap();
        assert!(matches!(
            d.release(&pool, "plain", &me),
            Err(Error::PoolConflict(_))
        ));
        assert!(tmp.path().join("plain/keep").exists());

        d.release(&pool, "v", &me).unwrap();
        assert!(!tmp.path().join("v").exists());
        // Gone already is not an error.
        d.release(&pool, "v", &me).unwrap();
    }

    /// A file beside the data that this driver did not create stops the
    /// removal of the volume's directory instead of being deleted with it.
    #[test]
    fn release_leaves_what_it_did_not_create() {
        let tmp = tempfile::tempdir().unwrap();
        let d = DirPoolDriver::new(None);
        let e = entry(tmp.path());
        let pool = PoolRef {
            name: "p",
            entry: &e,
        };
        let me = Owner::new("me");
        d.allocate(&pool, &req("v"), &me).unwrap();
        std::fs::write(tmp.path().join("v/notes.txt"), b"mine").unwrap();
        assert!(d.release(&pool, "v", &me).is_err());
        assert_eq!(
            std::fs::read(tmp.path().join("v/notes.txt")).unwrap(),
            b"mine"
        );
    }

    #[test]
    fn usage_reports_a_capacity_no_smaller_than_what_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        let d = DirPoolDriver::new(None);
        let e = entry(tmp.path());
        let u = d.usage(&PoolRef {
            name: "p",
            entry: &e,
        });
        let (cap, used) = (u.capacity_bytes.unwrap(), u.used_bytes.unwrap());
        assert!(cap > 0 && used <= cap, "{u:?}");
        let gone = entry(&tmp.path().join("nope"));
        assert_eq!(
            d.usage(&PoolRef {
                name: "p",
                entry: &gone
            }),
            PoolUsage::default()
        );
    }
}

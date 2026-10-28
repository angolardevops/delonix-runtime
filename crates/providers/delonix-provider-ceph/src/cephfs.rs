//! The `cephfs` storage pool driver (ADR-0081): filesystem volumes backed by
//! CephFS subvolumes.
//!
//! A pool is a CephFS file system the administrator declared (`cephFs`). A
//! filesystem volume is a subvolume made with `ceph fs subvolume create`,
//! stamped with subvolume metadata so it is adopted or released by its stamp
//! and never by its name alone, mounted under the helper's fixed root and
//! handed to a container as a directory — the same shape the dir/btrfs/zfs
//! drivers hand out.
//!
//! Reading the pool needs a readable cephx keyring, run in the engine's own
//! process. Making a subvolume and mounting it is root, so those go through
//! the helper.

use std::path::PathBuf;
use std::sync::Arc;

use delonix_storage::exec::Run;
use delonix_storage::mounts::{self, mountpoint};
use delonix_storage::ownership::Owner;
use delonix_storage::pool::{
    valid_volume_name, Allocation, PoolOp, PoolProbe, PoolRef, PoolUsage, Privilege,
    StoragePoolDriver, VolumeRequest, VolumeShape,
};
use delonix_storage::{Error, Result};

use crate::ceph::{self, Answer, CephCtx, META_OWNER, META_POOL};

/// The driver's id in the allowlist.
pub const ID: &str = "cephfs";

/// The CephFS driver. `mount_root` is the helper's fixed root under which every
/// subvolume appears — a request never decides where a filesystem is mounted.
pub struct CephFsDriver {
    run: Arc<dyn Run>,
    mount_root: PathBuf,
}

impl CephFsDriver {
    pub fn new(run: Arc<dyn Run>, mount_root: PathBuf) -> Self {
        Self { run, mount_root }
    }

    fn fs_name<'a>(pool: &'a PoolRef) -> Result<&'a str> {
        pool.entry.ceph_fs.as_deref().ok_or_else(|| {
            Error::InvalidAllowlist(format!(
                "storage pool '{}': the `cephfs` driver needs a `cephFs` (the CephFS file \
                 system the subvolumes live in)",
                pool.name
            ))
        })
    }

    fn ctx(pool: &PoolRef) -> CephCtx {
        CephCtx::from_entry(pool.entry)
    }

    fn ceph(&self, ctx: &CephCtx, parts: &[&str]) -> Answer {
        ceph::run(&*self.run, &ctx.argv("ceph", parts))
    }

    /// The owner stamp on a subvolume, if it exists. `Ok(None)` is «no such
    /// subvolume».
    fn stamp_of(&self, ctx: &CephCtx, fs: &str, name: &str) -> Result<Option<Option<String>>> {
        match self.ceph(
            ctx,
            &["fs", "subvolume", "info", fs, name, "--format", "json"],
        ) {
            Answer::Ok(_) => {}
            Answer::Missing(_) => return Ok(None),
            Answer::Unavailable(m) => return Err(Error::PoolUnavailable(m)),
            Answer::Undetermined(m) => return Err(Error::PoolUndetermined(m)),
        }
        match self.ceph(
            ctx,
            &[
                "fs",
                "subvolume",
                "metadata",
                "ls",
                fs,
                name,
                "--format",
                "json",
            ],
        ) {
            Answer::Ok(out) => {
                let map: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(out.stdout.trim()).unwrap_or_default();
                Ok(Some(
                    map.get(META_OWNER)
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                ))
            }
            // The subvolume is there but carries no metadata yet.
            Answer::Missing(_) => Ok(Some(None)),
            Answer::Unavailable(m) => Err(Error::PoolUnavailable(m)),
            Answer::Undetermined(m) => Err(Error::PoolUndetermined(m)),
        }
    }

    /// The subvolume's path inside the CephFS (`/volumes/_nogroup/<name>/<uuid>`).
    fn getpath(&self, ctx: &CephCtx, fs: &str, name: &str) -> Result<String> {
        match self.ceph(ctx, &["fs", "subvolume", "getpath", fs, name]) {
            Answer::Ok(out) => {
                let p = out.stdout.trim().to_string();
                if p.is_empty() {
                    Err(Error::PoolUndetermined(format!(
                        "subvolume {name} has no path yet"
                    )))
                } else {
                    Ok(p)
                }
            }
            Answer::Missing(m) => Err(Error::PoolUnavailable(m)),
            Answer::Unavailable(m) => Err(Error::PoolUnavailable(m)),
            Answer::Undetermined(m) => Err(Error::PoolUndetermined(m)),
        }
    }

    /// Mounts the subvolume at its fixed mountpoint, if it is not there already,
    /// and returns the data directory a container writes under.
    fn mount_and_ready(
        &self,
        ctx: &CephCtx,
        pool: &PoolRef,
        fs: &str,
        name: &str,
        subpath: &str,
        uid: u32,
    ) -> Result<PathBuf> {
        let mnt = mountpoint(&self.mount_root, pool.name, name);
        if let Some(parent) = mnt.parent() {
            mounts::ensure_dir(parent)?;
        }
        mounts::ensure_dir(&mnt)?;
        if !is_mounted(&mnt) {
            // `<user>@.<fs>=<subpath>` lets mount.ceph resolve the fsid and the
            // mon addresses from the configuration, and the secret from the
            // keyring beside it — no key ever appears on a command line.
            let dev = format!("{}@.{fs}={subpath}", ctx.user);
            let mut argv = vec![
                "mount".to_string(),
                "-t".into(),
                "ceph".into(),
                dev,
                mnt.display().to_string(),
            ];
            if let Some(conf) = &ctx.conf {
                argv.push("-o".into());
                argv.push(format!("conf={}", conf.display()));
            }
            match ceph::run(&*self.run, &argv) {
                Answer::Ok(_) => {}
                Answer::Missing(m) | Answer::Unavailable(m) => {
                    return Err(Error::PoolUnavailable(m))
                }
                Answer::Undetermined(m) => return Err(Error::PoolUndetermined(m)),
            }
        }
        mounts::prepare_data_dir(&mnt, uid)
    }
}

fn is_mounted(path: &std::path::Path) -> bool {
    mounts::fs_magic(path) == Some(CEPH_SUPER_MAGIC)
}

/// `CEPH_SUPER_MAGIC` from the kernel (`include/uapi/linux/magic.h`).
const CEPH_SUPER_MAGIC: i64 = 0x00c3_6400;

fn check_volume(volume: &str) -> Result<()> {
    if valid_volume_name(volume) {
        Ok(())
    } else {
        Err(Error::InvalidPoolRequest(format!(
            "'{volume}' is not a volume name a pool can hold"
        )))
    }
}

impl StoragePoolDriver for CephFsDriver {
    fn id(&self) -> &'static str {
        ID
    }

    fn probe(&self, pool: &PoolRef) -> PoolProbe {
        let fs = match Self::fs_name(pool) {
            Ok(f) => f,
            Err(e) => {
                return PoolProbe::Unavailable {
                    missing: e.to_string(),
                    remedy: "add `cephFs:` to the pool's entry in the allowlist".into(),
                }
            }
        };
        let ctx = Self::ctx(pool);
        match self.ceph(&ctx, &["fs", "subvolume", "ls", fs, "--format", "json"]) {
            Answer::Ok(_) => PoolProbe::Available,
            Answer::Missing(why) => PoolProbe::Unavailable {
                missing: format!("CephFS '{fs}' does not exist ({why})"),
                remedy: format!(
                    "the cluster administrator creates it: `ceph fs volume create {fs}`"
                ),
            },
            Answer::Unavailable(missing) => PoolProbe::Unavailable {
                missing,
                remedy: "install the Ceph client tools (`ceph-common`) on the node".into(),
            },
            Answer::Undetermined(reason) => PoolProbe::Undetermined {
                reason,
                remedy: format!(
                    "check the cluster is reachable and the keyring is readable ({})",
                    ceph::conf_label(&ctx.conf)
                ),
            },
        }
    }

    fn required(&self, op: PoolOp) -> Privilege {
        match op {
            PoolOp::Probe | PoolOp::Usage => Privilege::Delegated("a readable ceph keyring"),
            // Mounting a CephFS subvolume is root.
            PoolOp::Allocate(_) | PoolOp::Release => Privilege::Helper,
        }
    }

    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation> {
        if req.shape != VolumeShape::Filesystem {
            return Err(Error::PoolUnavailable(format!(
                "storage pool '{}': the `cephfs` driver hands out filesystem volumes only; \
                 a block volume is the `ceph-rbd` driver's",
                pool.name
            )));
        }
        check_volume(&req.name)?;
        let fs = Self::fs_name(pool)?;
        let ctx = Self::ctx(pool);
        let (uid, _) = owner.parts().ok_or_else(|| {
            Error::InvalidPoolRequest(format!("owner '{}' is not <uid>:<root>", owner.id()))
        })?;

        // Already there? Adopt only on this owner's stamp.
        if let Some(stamp) = self.stamp_of(&ctx, fs, &req.name)? {
            if stamp.as_deref() != Some(owner.id()) {
                return Err(Error::PoolConflict(format!(
                    "CephFS subvolume '{}' exists in '{fs}' and carries no stamp of this engine \
                     — it is never adopted by its name; pick another volume name",
                    req.name
                )));
            }
            let subpath = self.getpath(&ctx, fs, &req.name)?;
            let path = self.mount_and_ready(&ctx, pool, fs, &req.name, &subpath, uid)?;
            return Ok(Allocation {
                pool: pool.name.to_string(),
                volume: req.name.clone(),
                shape: VolumeShape::Filesystem,
                path,
                adopted: true,
            });
        }

        // Make it. A size caps the subvolume with a quota; a `cephPool` places
        // its data on a named pool.
        let size = req.size_bytes.to_string();
        let mut parts = vec!["fs", "subvolume", "create", fs, req.name.as_str()];
        if req.size_bytes > 0 {
            parts.push("--size");
            parts.push(&size);
        }
        if let Some(data_pool) = pool.entry.ceph_pool.as_deref() {
            parts.push("--pool_layout");
            parts.push(data_pool);
        }
        match self.ceph(&ctx, &parts) {
            Answer::Ok(_) => {}
            Answer::Unavailable(m) => return Err(Error::PoolUnavailable(m)),
            Answer::Missing(m) | Answer::Undetermined(m) => return Err(Error::PoolUndetermined(m)),
        }
        let _ = self.ceph(
            &ctx,
            &[
                "fs",
                "subvolume",
                "metadata",
                "set",
                fs,
                &req.name,
                META_POOL,
                pool.name,
            ],
        );
        match self.ceph(
            &ctx,
            &[
                "fs",
                "subvolume",
                "metadata",
                "set",
                fs,
                &req.name,
                META_OWNER,
                owner.id(),
            ],
        ) {
            Answer::Ok(_) => {}
            other => {
                let _ = self.ceph(&ctx, &["fs", "subvolume", "rm", fs, &req.name]);
                return Err(Error::PoolUndetermined(format!(
                    "created subvolume '{}' but could not stamp it ({other:?}); it was removed",
                    req.name
                )));
            }
        }
        let subpath = self.getpath(&ctx, fs, &req.name)?;
        let path = self.mount_and_ready(&ctx, pool, fs, &req.name, &subpath, uid)?;
        Ok(Allocation {
            pool: pool.name.to_string(),
            volume: req.name.clone(),
            shape: VolumeShape::Filesystem,
            path,
            adopted: false,
        })
    }

    fn release(&self, pool: &PoolRef, volume: &str, owner: &Owner) -> Result<()> {
        check_volume(volume)?;
        let fs = Self::fs_name(pool)?;
        let ctx = Self::ctx(pool);
        match self.stamp_of(&ctx, fs, volume)? {
            None => return Ok(()),
            Some(stamp) if stamp.as_deref() != Some(owner.id()) => {
                return Err(Error::PoolConflict(format!(
                    "CephFS subvolume '{volume}' carries no stamp of this engine — it is not \
                     released by its name"
                )));
            }
            Some(_) => {}
        }
        // Unmount the fixed mountpoint if it is still mounted.
        let mnt = mountpoint(&self.mount_root, pool.name, volume);
        if is_mounted(&mnt) {
            let _ = ceph::run(
                &*self.run,
                &["umount".to_string(), mnt.display().to_string()],
            );
        }
        match self.ceph(&ctx, &["fs", "subvolume", "rm", fs, volume]) {
            Answer::Ok(_) | Answer::Missing(_) => Ok(()),
            Answer::Unavailable(m) => Err(Error::PoolUnavailable(m)),
            Answer::Undetermined(m) => Err(Error::PoolUndetermined(m)),
        }
    }

    fn usage(&self, pool: &PoolRef) -> PoolUsage {
        // The data pool's room, read the same way RBD reads it.
        let ctx = Self::ctx(pool);
        let Answer::Ok(out) = ceph::run(&*self.run, &ctx.argv("ceph", &["df", "--format", "json"]))
        else {
            return PoolUsage::default();
        };
        let Ok(fs) = Self::fs_name(pool) else {
            return PoolUsage::default();
        };
        let data_pool = pool
            .entry
            .ceph_pool
            .clone()
            .unwrap_or_else(|| format!("cephfs.{fs}.data"));
        crate::rbd::parse_pool_df(&out.stdout, &data_pool)
    }
}

/// The subvolumes a CephFS holds (`ceph fs subvolume ls <fs>`), for the node
/// API's read-only window onto a `cephfs` pool. `Err` is a message to surface.
pub fn list_subvolumes(
    run: &dyn Run,
    ctx: &CephCtx,
    fs: &str,
) -> std::result::Result<Vec<String>, String> {
    match ceph::run(
        run,
        &ctx.argv("ceph", &["fs", "subvolume", "ls", fs, "--format", "json"]),
    ) {
        Answer::Ok(out) => {
            let list: Vec<serde_json::Value> =
                serde_json::from_str(out.stdout.trim()).unwrap_or_default();
            Ok(list
                .into_iter()
                .filter_map(|e| e.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect())
        }
        Answer::Missing(m) | Answer::Unavailable(m) | Answer::Undetermined(m) => Err(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delonix_storage::allowlist::Entry;
    use delonix_storage::exec::testing::Script;

    fn pool_entry() -> Entry {
        Entry {
            driver: ID.into(),
            ceph_fs: Some("delonixfs".into()),
            ceph_conf: Some("/etc/ceph/ceph.conf".into()),
            ..Default::default()
        }
    }

    fn owner() -> Owner {
        Owner::of(1000, "/home/walter/.local/share/delonix")
    }

    #[test]
    fn a_pool_without_a_cephfs_is_unavailable() {
        let d = CephFsDriver::new(Arc::new(Script::new()), "/tmp/m".into());
        let entry = Entry {
            driver: ID.into(),
            ..Default::default()
        };
        let pool = PoolRef {
            name: "shared",
            entry: &entry,
        };
        assert!(matches!(d.probe(&pool), PoolProbe::Unavailable { .. }));
    }

    #[test]
    fn probe_available_when_subvolumes_list() {
        let script = Script::new();
        script.on(&["ceph"], 0, "[]", "");
        let d = CephFsDriver::new(Arc::new(script), "/tmp/m".into());
        let entry = pool_entry();
        let pool = PoolRef {
            name: "shared",
            entry: &entry,
        };
        assert_eq!(d.probe(&pool), PoolProbe::Available);
    }

    #[test]
    fn allocate_refuses_a_block_shape() {
        let d = CephFsDriver::new(Arc::new(Script::new()), "/tmp/m".into());
        let entry = pool_entry();
        let pool = PoolRef {
            name: "shared",
            entry: &entry,
        };
        let req = VolumeRequest {
            name: "db".into(),
            shape: VolumeShape::Block,
            size_bytes: 1 << 30,
        };
        assert!(matches!(
            d.allocate(&pool, &req, &owner()),
            Err(Error::PoolUnavailable(_))
        ));
    }

    #[test]
    fn a_subvolume_without_this_stamp_is_never_adopted() {
        let script = Arc::new(Script::new());
        // info → exists
        script.on(
            &[
                "ceph",
                "-c",
                "/etc/ceph/ceph.conf",
                "--id",
                "admin",
                "fs",
                "subvolume",
                "info",
            ],
            0,
            "{}",
            "",
        );
        // metadata ls → another owner
        script.on(
            &[
                "ceph",
                "-c",
                "/etc/ceph/ceph.conf",
                "--id",
                "admin",
                "fs",
                "subvolume",
                "metadata",
                "ls",
            ],
            0,
            r#"{"delonix.io/owner":"999:/other"}"#,
            "",
        );
        let d = CephFsDriver::new(script, "/tmp/m".into());
        let entry = pool_entry();
        let pool = PoolRef {
            name: "shared",
            entry: &entry,
        };
        let req = VolumeRequest {
            name: "db".into(),
            shape: VolumeShape::Filesystem,
            size_bytes: 0,
        };
        assert!(matches!(
            d.allocate(&pool, &req, &owner()),
            Err(Error::PoolConflict(_))
        ));
    }

    #[test]
    fn release_of_a_missing_subvolume_is_ok() {
        let script = Arc::new(Script::new());
        script.on(
            &["ceph"],
            2,
            "",
            "Error ENOENT: subvolume 'db' does not exist",
        );
        let d = CephFsDriver::new(script, "/tmp/m".into());
        let entry = pool_entry();
        let pool = PoolRef {
            name: "shared",
            entry: &entry,
        };
        assert!(d.release(&pool, "db", &owner()).is_ok());
    }
}

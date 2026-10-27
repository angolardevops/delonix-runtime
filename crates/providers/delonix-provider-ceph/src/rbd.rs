//! The `ceph-rbd` storage pool driver (ADR-0081): block volumes backed by RBD
//! images in a RADOS pool.
//!
//! A pool is a RADOS pool the administrator declared (`cephPool`). A block
//! volume is an image `<pool>/<volume>` created with `rbd create`, stamped with
//! image metadata (`rbd image-meta`) so it is adopted or released by its stamp
//! and never by its name alone, and mapped to a kernel block device
//! (`/dev/rbdN`) — the device a VM disk or a block volume claim consumes, the
//! same shape the LVM-thin driver hands out.
//!
//! Reading the pool (`rbd ls`, `ceph df`) needs a readable cephx keyring, run
//! in the engine's own process. Making, mapping and destroying an image writes
//! and maps a kernel device — root — so those go through the helper.

use std::path::PathBuf;
use std::sync::Arc;

use delonix_storage::exec::Run;
use delonix_storage::ownership::Owner;
use delonix_storage::pool::{
    valid_volume_name, Allocation, PoolOp, PoolProbe, PoolRef, PoolUsage, Privilege,
    StoragePoolDriver, VolumeRequest, VolumeShape,
};
use delonix_storage::{Error, Result};

use crate::ceph::{self, Answer, CephCtx, META_OWNER, META_POOL};

/// The driver's id in the allowlist.
pub const ID: &str = "ceph-rbd";

/// The RBD driver.
pub struct RbdDriver {
    run: Arc<dyn Run>,
}

impl RbdDriver {
    pub fn new(run: Arc<dyn Run>) -> Self {
        Self { run }
    }

    /// The RADOS pool this entry names, or a refusal when it names none.
    fn rados_pool<'a>(pool: &'a PoolRef) -> Result<&'a str> {
        pool.entry.ceph_pool.as_deref().ok_or_else(|| {
            Error::InvalidAllowlist(format!(
                "storage pool '{}': the `ceph-rbd` driver needs a `cephPool` (the RADOS pool \
                 that holds the images)",
                pool.name
            ))
        })
    }

    fn ctx(pool: &PoolRef) -> CephCtx {
        CephCtx::from_entry(pool.entry)
    }

    /// `rbd <global> <parts…>`.
    fn rbd(&self, ctx: &CephCtx, parts: &[&str]) -> Answer {
        ceph::run(&*self.run, &ctx.argv("rbd", parts))
    }

    /// The owner stamp on an image, if the image exists. `Ok(None)` is «no
    /// such image»; `Err` is «could not tell».
    fn stamp_of(&self, ctx: &CephCtx, spec: &str) -> Result<Option<Option<String>>> {
        match self.rbd(ctx, &["image-meta", "list", spec, "--format", "json"]) {
            Answer::Ok(out) => {
                let map: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(out.stdout.trim()).unwrap_or_default();
                let owner = map
                    .get(META_OWNER)
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                Ok(Some(owner))
            }
            Answer::Missing(_) => Ok(None),
            Answer::Unavailable(m) => Err(Error::PoolUnavailable(m)),
            Answer::Undetermined(m) => Err(Error::PoolUndetermined(m)),
        }
    }

    /// The kernel device an image is mapped to, if any.
    fn mapped_device(&self, ctx: &CephCtx, rados: &str, image: &str) -> Option<PathBuf> {
        let Answer::Ok(out) = self.rbd(ctx, &["device", "list", "--format", "json"]) else {
            return None;
        };
        let list: Vec<serde_json::Value> = serde_json::from_str(out.stdout.trim()).ok()?;
        list.into_iter()
            .find(|e| {
                e.get("pool").and_then(|v| v.as_str()) == Some(rados)
                    && e.get("name").and_then(|v| v.as_str()) == Some(image)
            })
            .and_then(|e| e.get("device").and_then(|v| v.as_str()).map(PathBuf::from))
    }

    /// Maps the image (or finds it already mapped) and returns its device.
    fn map(&self, ctx: &CephCtx, rados: &str, image: &str) -> Result<PathBuf> {
        if let Some(dev) = self.mapped_device(ctx, rados, image) {
            return Ok(dev);
        }
        let spec = format!("{rados}/{image}");
        match self.rbd(ctx, &["device", "map", &spec]) {
            Answer::Ok(out) => {
                let dev = out.stdout.trim();
                if dev.is_empty() {
                    // Some builds print nothing; ask the list.
                    self.mapped_device(ctx, rados, image).ok_or_else(|| {
                        Error::PoolUnavailable(format!("mapped {spec} but found no device"))
                    })
                } else {
                    Ok(PathBuf::from(dev))
                }
            }
            Answer::Missing(m) => Err(Error::PoolUnavailable(m)),
            Answer::Unavailable(m) => Err(Error::PoolUnavailable(m)),
            Answer::Undetermined(m) => Err(Error::PoolUndetermined(m)),
        }
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

impl StoragePoolDriver for RbdDriver {
    fn id(&self) -> &'static str {
        ID
    }

    fn probe(&self, pool: &PoolRef) -> PoolProbe {
        let rados = match Self::rados_pool(pool) {
            Ok(p) => p,
            Err(e) => {
                return PoolProbe::Unavailable {
                    missing: e.to_string(),
                    remedy: "add `cephPool:` to the pool's entry in the allowlist".into(),
                }
            }
        };
        let ctx = Self::ctx(pool);
        match self.rbd(&ctx, &["ls", rados]) {
            Answer::Ok(_) => PoolProbe::Available,
            Answer::Missing(why) => PoolProbe::Unavailable {
                missing: format!("RADOS pool '{rados}' does not exist ({why})"),
                remedy: format!(
                    "the cluster administrator creates it: `ceph osd pool create {rados}` \
                     and `rbd pool init {rados}`"
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
            // `rbd device map` makes a kernel block device — root.
            PoolOp::Allocate(_) | PoolOp::Release => Privilege::Helper,
        }
    }

    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation> {
        if req.shape != VolumeShape::Block {
            return Err(Error::PoolUnavailable(format!(
                "storage pool '{}': the `ceph-rbd` driver hands out block volumes only; \
                 a filesystem volume is the `cephfs` driver's",
                pool.name
            )));
        }
        check_volume(&req.name)?;
        let rados = Self::rados_pool(pool)?;
        let ctx = Self::ctx(pool);
        let spec = format!("{rados}/{}", req.name);

        // Already there? It is adopted only if it carries this owner's stamp.
        if let Some(stamp) = self.stamp_of(&ctx, &spec)? {
            if stamp.as_deref() != Some(owner.id()) {
                return Err(Error::PoolConflict(format!(
                    "RBD image {spec} exists and carries no stamp of this engine — it is never \
                     adopted by its name; pick another volume name"
                )));
            }
            let dev = self.map(&ctx, rados, &req.name)?;
            return Ok(Allocation {
                pool: pool.name.to_string(),
                volume: req.name.clone(),
                shape: VolumeShape::Block,
                path: dev,
                adopted: true,
            });
        }

        // Make it: `rbd create` takes a size in MiB.
        let mib = req.size_bytes.div_ceil(1 << 20).max(1).to_string();
        match self.rbd(&ctx, &["create", &spec, "--size", &mib]) {
            Answer::Ok(_) => {}
            Answer::Unavailable(m) => return Err(Error::PoolUnavailable(m)),
            Answer::Missing(m) | Answer::Undetermined(m) => return Err(Error::PoolUndetermined(m)),
        }
        // Stamp it, data first then owner last — the owner key is what release
        // checks, so it is written only once the image is fully there.
        let _ = self.rbd(&ctx, &["image-meta", "set", &spec, META_POOL, pool.name]);
        match self.rbd(&ctx, &["image-meta", "set", &spec, META_OWNER, owner.id()]) {
            Answer::Ok(_) => {}
            other => {
                // Could not stamp — undo the image so a later call is not fooled.
                let _ = self.rbd(&ctx, &["rm", &spec]);
                return Err(Error::PoolUndetermined(format!(
                    "created {spec} but could not stamp it ({other:?}); it was removed"
                )));
            }
        }
        let dev = self.map(&ctx, rados, &req.name)?;
        Ok(Allocation {
            pool: pool.name.to_string(),
            volume: req.name.clone(),
            shape: VolumeShape::Block,
            path: dev,
            adopted: false,
        })
    }

    fn release(&self, pool: &PoolRef, volume: &str, owner: &Owner) -> Result<()> {
        check_volume(volume)?;
        let rados = Self::rados_pool(pool)?;
        let ctx = Self::ctx(pool);
        let spec = format!("{rados}/{volume}");

        match self.stamp_of(&ctx, &spec)? {
            // Already gone — releasing what is not there is success.
            None => return Ok(()),
            Some(stamp) if stamp.as_deref() != Some(owner.id()) => {
                return Err(Error::PoolConflict(format!(
                    "RBD image {spec} carries no stamp of this engine — it is not released by \
                     its name"
                )));
            }
            Some(_) => {}
        }
        // Unmap first (if mapped), then destroy the image.
        if let Some(dev) = self.mapped_device(&ctx, rados, volume) {
            let _ = self.rbd(&ctx, &["device", "unmap", &dev.display().to_string()]);
        }
        match self.rbd(&ctx, &["rm", &spec]) {
            Answer::Ok(_) | Answer::Missing(_) => Ok(()),
            Answer::Unavailable(m) => Err(Error::PoolUnavailable(m)),
            Answer::Undetermined(m) => Err(Error::PoolUndetermined(m)),
        }
    }

    fn usage(&self, pool: &PoolRef) -> PoolUsage {
        let Ok(rados) = Self::rados_pool(pool) else {
            return PoolUsage::default();
        };
        let ctx = Self::ctx(pool);
        let Answer::Ok(out) = ceph::run(&*self.run, &ctx.argv("ceph", &["df", "--format", "json"]))
        else {
            return PoolUsage::default();
        };
        parse_pool_df(&out.stdout, rados)
    }
}

/// Reads `ceph df --format json` for one pool: its used bytes and the bytes it
/// could still store (`stored` + `max_avail`). `None` stays «not measured».
pub(crate) fn parse_pool_df(json: &str, rados: &str) -> PoolUsage {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return PoolUsage::default();
    };
    let Some(pools) = v.get("pools").and_then(|p| p.as_array()) else {
        return PoolUsage::default();
    };
    let Some(p) = pools
        .iter()
        .find(|p| p.get("name").and_then(|n| n.as_str()) == Some(rados))
    else {
        return PoolUsage::default();
    };
    let stats = p.get("stats");
    let used = stats.and_then(|s| s.get("stored")).and_then(|n| n.as_u64());
    let avail = stats
        .and_then(|s| s.get("max_avail"))
        .and_then(|n| n.as_u64());
    let capacity = match (used, avail) {
        (Some(u), Some(a)) => Some(u.saturating_add(a)),
        _ => None,
    };
    PoolUsage {
        capacity_bytes: capacity,
        used_bytes: used,
        metadata_pct: None,
    }
}

/// The images a RADOS pool holds (`rbd ls <pool>`), for the node API's
/// read-only window onto a `ceph-rbd` pool. `Err` is a message to surface.
pub fn list_images(
    run: &dyn Run,
    ctx: &CephCtx,
    rados: &str,
) -> std::result::Result<Vec<String>, String> {
    match ceph::run(run, &ctx.argv("rbd", &["ls", rados])) {
        Answer::Ok(out) => Ok(out
            .stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()),
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
            ceph_pool: Some("rbdpool".into()),
            ceph_conf: Some("/etc/ceph/ceph.conf".into()),
            ..Default::default()
        }
    }

    fn owner() -> Owner {
        Owner::of(1000, "/home/walter/.local/share/delonix")
    }

    #[test]
    fn a_pool_without_a_cephpool_is_unavailable_with_a_remedy() {
        let script = Script::new();
        let d = RbdDriver::new(Arc::new(script));
        let entry = Entry {
            driver: ID.into(),
            ..Default::default()
        };
        let pool = PoolRef {
            name: "block",
            entry: &entry,
        };
        assert!(matches!(d.probe(&pool), PoolProbe::Unavailable { .. }));
    }

    #[test]
    fn probe_is_available_when_the_pool_lists() {
        let script = Script::new();
        script.on(&["rbd"], 0, "testimg\n", "");
        let d = RbdDriver::new(Arc::new(script));
        let entry = pool_entry();
        let pool = PoolRef {
            name: "block",
            entry: &entry,
        };
        assert_eq!(d.probe(&pool), PoolProbe::Available);
    }

    #[test]
    fn allocate_refuses_a_filesystem_shape() {
        let d = RbdDriver::new(Arc::new(Script::new()));
        let entry = pool_entry();
        let pool = PoolRef {
            name: "block",
            entry: &entry,
        };
        let req = VolumeRequest {
            name: "db".into(),
            shape: VolumeShape::Filesystem,
            size_bytes: 1 << 30,
        };
        assert!(matches!(
            d.allocate(&pool, &req, &owner()),
            Err(Error::PoolUnavailable(_))
        ));
    }

    #[test]
    fn allocate_creates_stamps_and_maps_a_new_image() {
        let script = Arc::new(Script::new());
        // image-meta list on a missing image → ENOENT
        script.on(
            &["rbd"],
            2,
            "",
            "rbd: error opening image db: (2) No such file or directory",
        );
        // create succeeds (later rule wins for the create prefix)
        script.on(
            &[
                "rbd",
                "-c",
                "/etc/ceph/ceph.conf",
                "--id",
                "admin",
                "create",
            ],
            0,
            "",
            "",
        );
        script.on(
            &[
                "rbd",
                "-c",
                "/etc/ceph/ceph.conf",
                "--id",
                "admin",
                "image-meta",
                "set",
            ],
            0,
            "",
            "",
        );
        script.on(
            &[
                "rbd",
                "-c",
                "/etc/ceph/ceph.conf",
                "--id",
                "admin",
                "device",
                "list",
            ],
            0,
            "[]",
            "",
        );
        script.on(
            &[
                "rbd",
                "-c",
                "/etc/ceph/ceph.conf",
                "--id",
                "admin",
                "device",
                "map",
            ],
            0,
            "/dev/rbd0\n",
            "",
        );
        let d = RbdDriver::new(script.clone());
        let entry = pool_entry();
        let pool = PoolRef {
            name: "block",
            entry: &entry,
        };
        let req = VolumeRequest {
            name: "db".into(),
            shape: VolumeShape::Block,
            size_bytes: 256 * (1 << 20),
        };
        let a = d.allocate(&pool, &req, &owner()).unwrap();
        assert_eq!(a.path, PathBuf::from("/dev/rbd0"));
        assert!(!a.adopted);
        // The create carried the size in MiB.
        let created = script
            .calls()
            .into_iter()
            .find(|c| c.contains(&"create".to_string()))
            .unwrap();
        assert!(created.contains(&"256".to_string()), "{created:?}");
        assert!(created.contains(&"rbdpool/db".to_string()));
    }

    #[test]
    fn an_image_without_this_stamp_is_never_adopted() {
        let script = Arc::new(Script::new());
        script.on(&["rbd"], 0, r#"{"delonix.io/owner":"999:/other"}"#, "");
        let d = RbdDriver::new(script);
        let entry = pool_entry();
        let pool = PoolRef {
            name: "block",
            entry: &entry,
        };
        let req = VolumeRequest {
            name: "db".into(),
            shape: VolumeShape::Block,
            size_bytes: 1 << 20,
        };
        assert!(matches!(
            d.allocate(&pool, &req, &owner()),
            Err(Error::PoolConflict(_))
        ));
    }

    #[test]
    fn release_of_a_missing_image_is_ok() {
        let script = Arc::new(Script::new());
        script.on(
            &["rbd"],
            2,
            "",
            "rbd: error opening image db: (2) No such file or directory",
        );
        let d = RbdDriver::new(script);
        let entry = pool_entry();
        let pool = PoolRef {
            name: "block",
            entry: &entry,
        };
        assert!(d.release(&pool, "db", &owner()).is_ok());
    }

    #[test]
    fn usage_reads_stored_and_max_avail() {
        let json =
            r#"{"pools":[{"name":"rbdpool","stats":{"stored":1048576,"max_avail":9437184}}]}"#;
        let u = parse_pool_df(json, "rbdpool");
        assert_eq!(u.used_bytes, Some(1_048_576));
        assert_eq!(u.capacity_bytes, Some(1_048_576 + 9_437_184));
        assert_eq!(parse_pool_df(json, "absent"), PoolUsage::default());
    }
}

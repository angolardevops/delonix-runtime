//! Native Ceph support for the Delonix Runtime (ADR-0081).
//!
//! Four surfaces of one cluster, each reached by running the native Ceph client
//! tools through the storage [`Run`](delonix_storage::exec::Run) port — never by
//! linking librados:
//!
//! - **RADOS** is the substrate. [`cluster_health`] reads it (`ceph status`):
//!   the state a node reports and the capacity a pool offers.
//! - **RBD** hands out block volumes. [`RbdDriver`] is a
//!   [`StoragePoolDriver`](delonix_storage::pool::StoragePoolDriver) of block shape.
//! - **CephFS** hands out filesystem volumes. [`CephFsDriver`] is a
//!   [`StoragePoolDriver`](delonix_storage::pool::StoragePoolDriver) of filesystem shape.
//! - **RGW** is object storage. [`Rgw`] administers tenant S3 identities and
//!   their buckets; it is not a pool driver — a bucket has no volume shape.

pub mod ceph;
pub mod cephfs;
pub mod rbd;
pub mod rgw;

pub use ceph::CephCtx;
pub use cephfs::CephFsDriver;
pub use rbd::RbdDriver;
pub use rgw::{BucketInfo, Rgw, RgwError, RgwResult, RgwUser};

use std::sync::Arc;

use delonix_storage::exec::Run;

/// The RADOS cluster's state, as one node sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ClusterHealth {
    /// `HEALTH_OK`, `HEALTH_WARN`, `HEALTH_ERR`, or empty when unreadable.
    pub status: String,
    pub fsid: String,
    pub mons: u32,
    pub osds_up: u32,
    pub osds_in: u32,
    pub pools: u32,
}

/// Reads `ceph status` for the cluster the identity reaches. Returns `None`
/// when the tool is missing or the cluster cannot be reached — unknown is
/// never a healthy-looking zero.
pub fn cluster_health(run: &dyn Run, ctx: &CephCtx) -> Option<ClusterHealth> {
    let argv = ctx.argv("ceph", &["status", "--format", "json"]);
    let out = run.run(&argv).ok()?;
    if !out.success() {
        return None;
    }
    parse_status(&out.stdout)
}

fn parse_status(json: &str) -> Option<ClusterHealth> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let status = v
        .pointer("/health/status")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let osdmap = v.get("osdmap");
    Some(ClusterHealth {
        status,
        fsid: v
            .get("fsid")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        mons: v
            .pointer("/monmap/num_mons")
            .and_then(|n| n.as_u64())
            .or_else(|| {
                v.pointer("/quorum")
                    .and_then(|q| q.as_array())
                    .map(|a| a.len() as u64)
            })
            .unwrap_or(0) as u32,
        osds_up: osdmap
            .and_then(|o| o.get("num_up_osds"))
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as u32,
        osds_in: osdmap
            .and_then(|o| o.get("num_in_osds"))
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as u32,
        pools: v
            .pointer("/pgmap/num_pools")
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as u32,
    })
}

/// Builds the two storage-pool drivers Ceph adds, for a composition root to
/// register. `mount_root` is where CephFS subvolumes are mounted.
pub fn drivers(
    run: Arc<dyn Run>,
    mount_root: std::path::PathBuf,
) -> Vec<Arc<dyn delonix_storage::pool::StoragePoolDriver>> {
    vec![
        Arc::new(RbdDriver::new(run.clone())),
        Arc::new(CephFsDriver::new(run, mount_root)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use delonix_storage::exec::testing::Script;

    #[test]
    fn cluster_health_reads_status_osds_and_pools() {
        let script = Script::new();
        script.on(
            &["ceph"],
            0,
            r#"{"fsid":"abc","health":{"status":"HEALTH_OK"},"monmap":{"num_mons":1},"osdmap":{"num_up_osds":3,"num_in_osds":3},"pgmap":{"num_pools":11}}"#,
            "",
        );
        let ctx = CephCtx {
            conf: None,
            user: "admin".into(),
        };
        let h = cluster_health(&script, &ctx).unwrap();
        assert_eq!(h.status, "HEALTH_OK");
        assert_eq!(h.osds_up, 3);
        assert_eq!(h.pools, 11);
        assert_eq!(h.fsid, "abc");
    }

    #[test]
    fn cluster_health_is_none_when_the_tool_is_missing() {
        let ctx = CephCtx {
            conf: None,
            user: "admin".into(),
        };
        assert!(cluster_health(&Script::new(), &ctx).is_none());
    }

    #[test]
    fn two_drivers_are_built_with_the_expected_ids() {
        let ds = drivers(Arc::new(Script::new()), "/tmp/m".into());
        let ids: Vec<&str> = ds.iter().map(|d| d.id()).collect();
        assert_eq!(ids, vec![rbd::ID, cephfs::ID]);
    }
}

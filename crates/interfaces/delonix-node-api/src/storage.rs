//! The node's window onto its Ceph cluster (ADR-0081): the logic behind
//! `StorageService`, the one place both the gRPC and the REST encodings call.
//!
//! A storage pool's client identity (its `ceph.conf` and cephx user) comes from
//! the administrator's allowlist and from there only — the same boundary the
//! storage-pool drivers hold (ADR-0067 D3). The Ceph tools run through the same
//! `SystemRun` the local drivers use.

use std::path::PathBuf;
use std::sync::Arc;

use delonix_provider_ceph::ceph::CephCtx;
use delonix_provider_ceph::{cephfs, rbd, rgw::Rgw};
use delonix_storage::allowlist::{self, Allowlist, Entry};
use delonix_storage::exec::Run;
use tonic::Status;

use crate::proto::v1::{
    CephStatus, CreateObjectStoreUserRequest, GetCephStatusRequest, GetObjectStoreBucketRequest,
    ListCephfsSubvolumesRequest, ListCephfsSubvolumesResponse, ListObjectStoreBucketsRequest,
    ListObjectStoreBucketsResponse, ListRbdImagesRequest, ListRbdImagesResponse, ObjectStoreBucket,
    ObjectStoreUser,
};

fn run() -> Arc<dyn Run> {
    Arc::new(delonix_volume::pool_exec::SystemRun)
}

/// The default Ceph identity: the node's `/etc/ceph/ceph.conf` as `client.admin`.
fn default_ctx() -> CephCtx {
    CephCtx {
        conf: None,
        user: "admin".into(),
    }
}

/// Reads the allowlist and returns the entry for `pool`, or a refusal.
fn entry_of(pool: &str) -> Result<Entry, Status> {
    if pool.is_empty() {
        return Err(Status::invalid_argument("a storage pool name is required"));
    }
    let path = allowlist::path();
    let text = std::fs::read_to_string(&path).map_err(|e| {
        Status::failed_precondition(format!(
            "no storage pool allowlist at {}: {e}",
            path.display()
        ))
    })?;
    let list: Allowlist = serde_yaml::from_str(&text)
        .map_err(|e| Status::failed_precondition(format!("{}: {e}", path.display())))?;
    list.pools
        .get(pool)
        .cloned()
        .ok_or_else(|| Status::not_found(format!("storage pool '{pool}' is not in the allowlist")))
}

/// `GetCephStatus`: the RADOS substrate's health.
pub fn ceph_status(req: &GetCephStatusRequest) -> Result<CephStatus, Status> {
    let ctx = if req.pool.is_empty() {
        default_ctx()
    } else {
        let entry = entry_of(&req.pool)?;
        CephCtx::from_entry(&entry)
    };
    match delonix_provider_ceph::cluster_health(&*run(), &ctx) {
        Some(h) => Ok(CephStatus {
            health: h.status,
            fsid: h.fsid,
            mons: h.mons,
            osds_up: h.osds_up,
            osds_in: h.osds_in,
            pools: h.pools,
            reachable: true,
        }),
        None => Ok(CephStatus {
            reachable: false,
            ..Default::default()
        }),
    }
}

/// `ListRbdImages`: the images a `ceph-rbd` pool holds.
pub fn list_rbd_images(req: &ListRbdImagesRequest) -> Result<ListRbdImagesResponse, Status> {
    let entry = entry_of(&req.pool)?;
    if entry.driver != rbd::ID {
        return Err(Status::failed_precondition(format!(
            "storage pool '{}' is a '{}' pool, not 'ceph-rbd'",
            req.pool, entry.driver
        )));
    }
    let rados = entry.ceph_pool.as_deref().ok_or_else(|| {
        Status::failed_precondition(format!("storage pool '{}' has no cephPool", req.pool))
    })?;
    let ctx = CephCtx::from_entry(&entry);
    let images = rbd::list_images(&*run(), &ctx, rados).map_err(Status::unavailable)?;
    Ok(ListRbdImagesResponse { images })
}

/// `ListCephfsSubvolumes`: the subvolumes a `cephfs` pool holds.
pub fn list_cephfs_subvolumes(
    req: &ListCephfsSubvolumesRequest,
) -> Result<ListCephfsSubvolumesResponse, Status> {
    let entry = entry_of(&req.pool)?;
    if entry.driver != cephfs::ID {
        return Err(Status::failed_precondition(format!(
            "storage pool '{}' is a '{}' pool, not 'cephfs'",
            req.pool, entry.driver
        )));
    }
    let fs = entry.ceph_fs.as_deref().ok_or_else(|| {
        Status::failed_precondition(format!("storage pool '{}' has no cephFs", req.pool))
    })?;
    let ctx = CephCtx::from_entry(&entry);
    let subvolumes = cephfs::list_subvolumes(&*run(), &ctx, fs).map_err(Status::unavailable)?;
    Ok(ListCephfsSubvolumesResponse { subvolumes })
}

/// The object gateway, reached with the node's default Ceph configuration.
fn gateway() -> Rgw {
    Rgw::new(run(), ceph_conf())
}

/// The node's default `ceph.conf`, when one is set in the environment; else the
/// system default the tools find on their own.
fn ceph_conf() -> Option<PathBuf> {
    std::env::var_os("CEPH_CONF").map(PathBuf::from)
}

fn rgw_status(e: delonix_provider_ceph::RgwError) -> Status {
    use delonix_provider_ceph::RgwError::*;
    match e {
        Unavailable(m) => Status::unavailable(m),
        Undetermined(m) => Status::unavailable(m),
        NotFound(m) => Status::not_found(m),
    }
}

/// `ListObjectStoreBuckets`.
pub fn list_buckets(
    req: &ListObjectStoreBucketsRequest,
) -> Result<ListObjectStoreBucketsResponse, Status> {
    let uid = (!req.uid.is_empty()).then_some(req.uid.as_str());
    let buckets = gateway().list_buckets(uid).map_err(rgw_status)?;
    Ok(ListObjectStoreBucketsResponse { buckets })
}

/// `GetObjectStoreBucket`.
pub fn get_bucket(req: &GetObjectStoreBucketRequest) -> Result<ObjectStoreBucket, Status> {
    if req.bucket.is_empty() {
        return Err(Status::invalid_argument("a bucket name is required"));
    }
    let b = gateway().bucket_stats(&req.bucket).map_err(rgw_status)?;
    Ok(ObjectStoreBucket {
        name: b.name,
        owner: b.owner,
        objects: b.objects,
        size_bytes: b.size_bytes,
    })
}

/// `CreateObjectStoreUser`: provisions (or reads back) a tenant's S3 identity.
pub fn create_user(req: &CreateObjectStoreUserRequest) -> Result<ObjectStoreUser, Status> {
    if req.uid.is_empty() {
        return Err(Status::invalid_argument("a uid is required"));
    }
    let display = if req.display_name.is_empty() {
        req.uid.as_str()
    } else {
        req.display_name.as_str()
    };
    let u = gateway()
        .ensure_user(&req.uid, display)
        .map_err(rgw_status)?;
    Ok(ObjectStoreUser {
        uid: u.uid,
        display_name: u.display_name,
        access_key: u.access_key,
        secret_key: u.secret_key,
    })
}

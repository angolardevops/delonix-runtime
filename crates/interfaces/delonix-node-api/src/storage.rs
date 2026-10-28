//! The node's window onto its Ceph cluster (ADR-0081): the logic behind
//! `StorageService`, the one place both the gRPC and the REST encodings call.
//!
//! A storage pool's client identity (its `ceph.conf` and cephx user) comes from
//! the administrator's allowlist and from there only — the same boundary the
//! storage-pool drivers hold (ADR-0067 D3). The Ceph tools run through the same
//! `SystemRun` the local drivers use.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use delonix_provider_ceph::ceph::CephCtx;
use delonix_provider_ceph::{cephfs, rbd, rgw::Rgw};
use delonix_storage::allowlist::{self, Allowlist, Entry};
use delonix_storage::exec::Run;
use delonix_storage::ownership::Owner;
use delonix_storage::pool::{Allocation, PoolRef, StoragePoolDriver, VolumeRequest, VolumeShape};
use delonix_storage::{helper, registry};
use tonic::Status;

use crate::proto::v1::{
    CephStatus, CreateObjectStoreUserRequest, GetCephStatusRequest, GetObjectStoreBucketRequest,
    ListCephfsSubvolumesRequest, ListCephfsSubvolumesResponse, ListObjectStoreBucketsRequest,
    ListObjectStoreBucketsResponse, ListRbdImagesRequest, ListRbdImagesResponse, ObjectStoreBucket,
    ObjectStoreUser, ProvisionRequest,
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

/// Reads the administrator's allowlist, or a refusal. The file is read on every
/// use — there is no cache to go stale (ADR-0067 D3).
fn load_allowlist() -> Result<Allowlist, Status> {
    let path = allowlist::path();
    let text = std::fs::read_to_string(&path).map_err(|e| {
        Status::failed_precondition(format!(
            "no storage pool allowlist at {}: {e}",
            path.display()
        ))
    })?;
    serde_yaml::from_str(&text)
        .map_err(|e| Status::failed_precondition(format!("{}: {e}", path.display())))
}

/// Reads the allowlist and returns the entry for `pool`, or a refusal.
fn entry_of(pool: &str) -> Result<Entry, Status> {
    if pool.is_empty() {
        return Err(Status::invalid_argument("a storage pool name is required"));
    }
    load_allowlist()?
        .pools
        .remove(pool)
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

// ----------------------------------------------------------------- provisioning
//
// `CreateVolume`/`DeleteVolume` provision (and release) a Ceph pool volume
// through the same storage-pool driver the CLI composes (ADR-0067, ADR-0081):
// `spec.provision.provider` names a pool of the administrator's allowlist, its
// `driver` says whether it hands out block (`ceph-rbd`) or filesystem
// (`cephfs`) volumes, and the operation is routed to the helper for the parts
// that need root (map/mount). Pool creation stays the administrator's; the
// engine only carves a volume out of a pool that already exists.

/// Registers the Ceph storage-pool drivers this engine provisions through, once
/// (the composition root calls it). Registering does no I/O (ADR-0008): a
/// driver's probe runs when a pool is used. The local pool drivers (`dir`,
/// `zfs`, …) stay in the CLI's composition until a node-API caller needs them.
pub fn register_pool_drivers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mounts = PathBuf::from(helper::MOUNT_ROOT);
        for d in delonix_provider_ceph::drivers(run(), mounts) {
            // The ids are literals and differ; the only refusal cannot happen.
            let _ = registry::register(d);
        }
    });
}

/// The volume shape a Ceph pool driver hands out: `ceph-rbd` a block device,
/// `cephfs` a mounted filesystem. A driver the node API does not provision
/// through is `None`.
fn provision_shape(driver: &str) -> Option<VolumeShape> {
    match driver {
        rbd::ID => Some(VolumeShape::Block),
        cephfs::ID => Some(VolumeShape::Filesystem),
        _ => None,
    }
}

/// Who this engine is to a pool: this user, this state root — the stamp a pool
/// volume carries (ADR-0067 D5), the same shape the CLI's `owner()` makes.
fn owner(root: &Path) -> Owner {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    Owner::new(format!("{uid}:{}", root.display()))
}

/// The privileged helper's client, when its socket is on this node. Nothing is
/// dialled here: the first operation that needs it connects.
fn helper_client() -> Option<Arc<dyn helper::HelperCall>> {
    let path = helper::socket_path();
    path.exists()
        .then(|| Arc::new(helper::SocketClient::new(path)) as Arc<dyn helper::HelperCall>)
}

/// Resolves the pool `prov.provider` names, the driver that serves it (routed
/// for this node), and the shape it hands out — refusing, as the error and
/// before anything is touched, a provider that is empty, a pool not in the
/// allowlist, a driver the node API does not provision through, or one not
/// built into this engine.
fn resolve_provision(
    prov: &ProvisionRequest,
) -> Result<(String, Entry, Arc<dyn StoragePoolDriver>, VolumeShape), Status> {
    register_pool_drivers();
    let pool = prov.provider.trim();
    if pool.is_empty() {
        return Err(Status::invalid_argument(
            "CreateVolume: spec.provision.provider names the storage pool to provision in, and is \
             required",
        ));
    }
    let list = load_allowlist()?;
    let entry = list.pools.get(pool).cloned().ok_or_else(|| {
        Status::not_found(format!("storage pool '{pool}' is not in the allowlist"))
    })?;
    let shape = provision_shape(&entry.driver).ok_or_else(|| {
        Status::unimplemented(format!(
            "CreateVolume: storage pool '{pool}' is a '{}' pool; the node API provisions Ceph \
             pools (ceph-rbd, cephfs) — a '{}' pool is provisioned by `delonix volume create \
             <name> --pool {pool}`",
            entry.driver, entry.driver
        ))
    })?;
    let driver = registry::driver(&entry.driver).ok_or_else(|| {
        Status::failed_precondition(format!(
            "storage pool '{pool}': driver '{}' is not built into this engine",
            entry.driver
        ))
    })?;
    let driver = helper::route(driver, &list, helper_client());
    Ok((pool.to_string(), entry, driver, shape))
}

/// What `CreateVolume` checks about `spec.provision` before it acknowledges an
/// operation: the pool resolves to a Ceph driver this engine has, and the
/// request carries a size to ask of it. Nothing is touched.
pub fn check_provision(prov: &ProvisionRequest, quota_bytes: i64) -> Result<(), Status> {
    resolve_provision(prov)?;
    if quota_bytes <= 0 {
        return Err(Status::invalid_argument(
            "CreateVolume: a provisioned volume needs spec.quota_bytes — the size to ask of the \
             pool",
        ));
    }
    Ok(())
}

/// Allocates the provisioned volume in its pool and returns what the driver
/// handed out (its path, and whether an existing object stamped for this engine
/// was adopted). Runs AFTER the operation is acknowledged, in `create_work`.
pub fn provision_allocate(
    root: &Path,
    name: &str,
    size_bytes: u64,
    prov: &ProvisionRequest,
) -> delonix_model::Result<Allocation> {
    let (pool, entry, driver, shape) = resolve_provision(prov).map_err(status_to_model)?;
    let pool_ref = PoolRef {
        name: &pool,
        entry: &entry,
    };
    let req = VolumeRequest {
        name: name.to_string(),
        shape,
        size_bytes,
    };
    Ok(driver.allocate(&pool_ref, &req, &owner(root))?)
}

/// Gives a pool volume's data back to its pool — the data and the stamp go
/// (ADR-0067 D5). Called by `DeleteVolume` BEFORE the record is removed: the
/// record is what says which pool holds it. `pool` is the allowlist name the
/// record kept.
pub fn provision_release(root: &Path, pool: &str, volume: &str) -> delonix_model::Result<()> {
    let prov = ProvisionRequest {
        provider: pool.to_string(),
        extensions: None,
    };
    let (pool, entry, driver, _shape) = resolve_provision(&prov).map_err(status_to_model)?;
    let pool_ref = PoolRef {
        name: &pool,
        entry: &entry,
    };
    driver.release(&pool_ref, volume, &owner(root))?;
    Ok(())
}

/// A pre-flight refusal becomes a `delonix_model::Error` so it can travel back
/// out of the work closure the operation ledger runs; the code is kept so the
/// REST edge rebuilds the same status.
fn status_to_model(s: Status) -> delonix_model::Error {
    use delonix_model::Error as E;
    let msg = s.message().to_string();
    match s.code() {
        tonic::Code::NotFound => E::NotFound(msg),
        tonic::Code::InvalidArgument => E::Invalid(msg),
        _ => E::Unavailable(msg),
    }
}

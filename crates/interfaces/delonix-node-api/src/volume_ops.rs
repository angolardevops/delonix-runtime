//! `VolumeService`, the write half: `CreateVolume` and `DeleteVolume`
//! (ADR-0042 step E).
//!
//! Each answers with an [`Operation`] persisted before the work starts
//! ([`crate::operations`]), the same ledger the network and VM mutations use.
//! What can be decided without touching anything is refused BEFORE an
//! operation exists, as the error itself: a name already taken, an etag that
//! does not match, a volume something still uses.
//!
//! **What is served, and what is refused by name.** A LOCAL volume is created
//! in the node's unscoped store — where `delonix volume create` puts it, and
//! which the read half reports in `default` — with its quota (measured, the
//! rootless model), alert threshold, labels and annotations. A PROVISIONED
//! volume (`spec.provision`) is carved out of a Ceph storage pool the
//! administrator declared: the pool name is resolved in the allowlist, its
//! driver (`ceph-rbd`, `cephfs`) allocates and maps the backing object, and
//! the record names the pool (ADR-0067, ADR-0081; see [`crate::storage`]).
//! A remote share, a share of another volume, and any namespace but `default`
//! answer `UNIMPLEMENTED` with the CLI command that does it: each needs a piece
//! (a credentials file, the share's carving rules) that lives in the CLI's
//! composition today.
//!
//! A delete removes a local volume or a share's record, and gives a pool
//! volume's data back to its pool (the driver releases it) BEFORE removing the
//! record, after the same in-use check `delonix volume rm` makes. **A delete
//! that reports success has deleted**: the store removes the data before the
//! record and stops at the first failure, and a volume whose data this process
//! cannot read (owned by a mapped subuid) is refused before anything is removed
//! (DX-6205) — removing what is readable would leave it half deleted. A network
//! share, and `destroy_remote`, are refused by name: their release goes through
//! drivers the CLI composes.

use std::path::Path;

use delonix_volume::VolumeStore;
use tonic::Status;

use crate::network_ops::{stale_etag, status_of};
use crate::operations::{self, Begun};
use crate::proto::v1::{volume_spec, CreateVolumeRequest, DeleteVolumeRequest, Operation};
use crate::volumes::{self, Entry, DEFAULT_NAMESPACE};

/// What `CreateVolume` does once it is acknowledged.
type CreateWork<'a> = &'a dyn Fn(&Path, &CreateVolumeRequest) -> delonix_model::Result<()>;
/// What `DeleteVolume` does once it is acknowledged: `(root, entry)`.
type DeleteWork<'a> = &'a dyn Fn(&Path, &Entry) -> delonix_model::Result<()>;

fn target_href(namespace: &str, name: &str) -> String {
    format!(
        "/v1/namespaces/{}/volumes/{}",
        crate::page::escape(namespace),
        crate::page::escape(name)
    )
}

/// `(label, Some(value))` pairs, the shape `VolumeStore::set_metadata` takes.
fn pairs(map: &std::collections::HashMap<String, String>) -> Vec<(String, Option<String>)> {
    let mut v: Vec<_> = map
        .iter()
        .map(|(k, val)| (k.clone(), Some(val.clone())))
        .collect();
    v.sort();
    v
}

/// The quota and alert threshold a request asks for, validated.
fn quota_of(spec: &crate::proto::v1::VolumeSpec) -> Result<(Option<u64>, Option<u8>), Status> {
    let quota = match spec.quota_bytes {
        0 => None,
        q if q < 0 => {
            return Err(Status::invalid_argument(format!(
                "CreateVolume: quota_bytes {q} is negative"
            )))
        }
        q => Some(q as u64),
    };
    let alert = match spec.alert_percent {
        0 => None,
        a if (1..=100).contains(&a) => Some(a as u8),
        a => {
            return Err(Status::invalid_argument(format!(
                "CreateVolume: alert_percent {a} is outside 1..100"
            )))
        }
    };
    Ok((quota, alert))
}

/// [`operations::fingerprint_of`] of a `CreateVolume` request's content. The
/// provisioning pool is part of it: the same name asked of a different pool is
/// a different request, not a replay of the first.
fn create_fingerprint(
    quota: Option<u64>,
    alert: Option<u8>,
    provider: &str,
    labels: &std::collections::HashMap<String, String>,
    annotations: &std::collections::HashMap<String, String>,
) -> String {
    let quota = quota.map(|q| q.to_string()).unwrap_or_default();
    let alert = alert.map(|a| a.to_string()).unwrap_or_default();
    operations::fingerprint_of(&[
        ("quota_bytes", &quota),
        ("alert_percent", &alert),
        ("provision", provider),
        ("labels", &operations::canonical_map(labels)),
        ("annotations", &operations::canonical_map(annotations)),
    ])
}

/// The real create: a local volume, then its quota, then its metadata. A
/// failure after the volume exists removes it again — a volume handed back
/// without the quota or labels it was asked with would be reported as a
/// success it is not.
pub fn create_work(root: &Path, req: &CreateVolumeRequest) -> delonix_model::Result<()> {
    let store = VolumeStore::open(root).map_err(engine)?;
    let spec = req.spec.clone().unwrap_or_default();
    // Validated before the operation was acknowledged.
    let (quota, alert) = quota_of(&spec).unwrap_or_default();
    // A provisioned volume: the pool driver allocates its data (resolved and
    // sized before this ran), then the record names the pool so delete
    // releases it and the reader reports where it lives.
    if let Some(prov) = &spec.provision {
        let size = quota.unwrap_or_default();
        let alloc = crate::storage::provision_allocate(root, &req.name, size, prov)?;
        store
            .register_in_pool(&req.name, &alloc.path, size, alert, &prov.provider)
            .map_err(engine)?;
        if !req.labels.is_empty() || !req.annotations.is_empty() {
            store
                .set_metadata(&req.name, &pairs(&req.labels), &pairs(&req.annotations))
                .map_err(engine)?;
        }
        delonix_node::events::emit(
            root,
            "volume",
            "create",
            &req.name,
            &req.name,
            Some(&format!("pool={}", prov.provider)),
        );
        return Ok(());
    }
    store
        .create_with(&req.name, "local", None, None)
        .map_err(engine)?;
    let rest = || -> delonix_model::Result<()> {
        if quota.is_some() || alert.is_some() {
            store
                .set_quota(&req.name, quota, alert, false)
                .map_err(engine)?;
        }
        if !req.labels.is_empty() || !req.annotations.is_empty() {
            store
                .set_metadata(&req.name, &pairs(&req.labels), &pairs(&req.annotations))
                .map_err(engine)?;
        }
        Ok(())
    };
    if let Err(e) = rest() {
        let _ = store.remove(&req.name);
        return Err(e);
    }
    delonix_node::events::emit(root, "volume", "create", &req.name, &req.name, None);
    Ok(())
}

fn engine(e: delonix_volume::Error) -> delonix_model::Error {
    e.into()
}

/// The real delete: the store removes the data first and the record last,
/// and stops at the first failure — so an `Ok` here is a volume gone.
pub(crate) fn delete_work(root: &Path, entry: &Entry) -> delonix_model::Result<()> {
    let store = if entry.scoped {
        VolumeStore::open_scoped(root, &entry.namespace)
    } else {
        VolumeStore::open(root)
    }
    .map_err(engine)?;
    let name = &entry.volume.name;
    // A pool volume's data is given back to its pool FIRST — the record is what
    // says which pool holds it, so it goes last (ADR-0067). A release that
    // fails stops the delete: a record removed with the object still in the
    // pool would orphan it.
    if let Some(pool) = entry.volume.pool.as_deref() {
        crate::storage::provision_release(root, pool, name)?;
    }
    store.remove_with(name, None).map_err(engine)?;
    delonix_node::events::emit(root, "volume", "remove", name, name, None);
    Ok(())
}

/// Who uses `entry`: containers that mount its data, and shares carved out
/// of it. Fails closed — a store that cannot be read is an error, never
/// "nobody". The same two checks `delonix volume rm` makes.
pub(crate) fn users_of(root: &Path, entry: &Entry) -> Result<Vec<String>, Status> {
    let mount = Path::new(&entry.volume.mountpoint);
    let mut out = Vec::new();
    let containers = delonix_state::Store::open(root.join("containers"))
        .and_then(|s| s.list())
        .map_err(|e| Status::internal(format!("reading the containers: {e}")))?;
    for c in containers {
        let uses = c
            .mounts
            .iter()
            .any(|m| Path::new(&m.source).starts_with(mount));
        if uses {
            out.push(format!("container {}", c.name));
        }
    }
    for other in volumes::entries(root)? {
        // Self is excluded by mountpoint: a name is not unique across
        // namespaces, the path on disk is.
        if other.volume.mountpoint == entry.volume.mountpoint {
            continue;
        }
        if Path::new(&other.volume.mountpoint).starts_with(mount) {
            out.push(format!("share {}/{}", other.namespace, other.volume.name));
        }
    }
    Ok(out)
}

/// `CreateVolume` under `root`.
pub fn create_in(root: &Path, req: &CreateVolumeRequest) -> Result<Operation, Status> {
    create_with(root, req, &create_work)
}

/// [`create_in`] with the work injected (the tests' seam).
pub fn create_with(
    root: &Path,
    req: &CreateVolumeRequest,
    work: CreateWork<'_>,
) -> Result<Operation, Status> {
    if req.name.is_empty() {
        return Err(Status::invalid_argument("CreateVolume: name is required"));
    }
    VolumeStore::validate_name(&req.name).map_err(|e| status_of(&e.into()))?;
    let spec = req.spec.clone().unwrap_or_default();
    match &spec.source {
        None | Some(volume_spec::Source::Local(_)) => {}
        Some(volume_spec::Source::Remote(_)) => {
            return Err(Status::unimplemented(
                "CreateVolume: a remote share (nfs, smb, webdav) is in the contract and this \
                 engine does not create one on the node API yet (`delonix volume create \
                 --driver nfs --opt server=… --opt share=…` does)",
            ))
        }
        Some(volume_spec::Source::ShareOf(_)) => {
            return Err(Status::unimplemented(
                "CreateVolume: a share of another volume is in the contract and this engine \
                 does not create one on the node API yet (`delonix volume create <name> \
                 --parent <volume> [--namespace <ns>]` does)",
            ))
        }
    }
    // A provisioned volume is carved out of a storage pool the administrator
    // declared (ADR-0067, ADR-0081): resolved and sized here, before an
    // operation exists; allocated by `create_work` once it does.
    if let Some(prov) = &spec.provision {
        crate::storage::check_provision(prov, spec.quota_bytes)?;
    }
    if !matches!(req.namespace.as_str(), "" | DEFAULT_NAMESPACE) {
        return Err(Status::unimplemented(format!(
            "CreateVolume: namespace '{}': a local volume lives in the node's unscoped store, \
             reported in '{DEFAULT_NAMESPACE}'; a volume of another namespace is a share, \
             which this engine does not create on the node API yet",
            req.namespace
        )));
    }
    let (quota, alert) = quota_of(&spec)?;
    let provider = spec
        .provision
        .as_ref()
        .map(|p| p.provider.as_str())
        .unwrap_or_default();
    let target = format!("Volume/{DEFAULT_NAMESPACE}/{}", req.name);
    let fp = create_fingerprint(quota, alert, provider, &req.labels, &req.annotations);
    // The same request sent again is answered with the first answer — before
    // the "already exists" check, which the first request itself made true.
    if let Some(found) = operations::replay(root, "create", &target, &req.request_id, &fp)? {
        return Ok(operations::message(&found));
    }
    // Any volume reported under this name in `default` — the unscoped root or
    // the namespace's own sub-tree — makes the name taken: a second one would
    // be the "exists twice" state every reader refuses.
    if volumes::entries(root)?
        .iter()
        .any(|e| e.namespace == DEFAULT_NAMESPACE && e.volume.name == req.name)
    {
        return Err(Status::already_exists(format!(
            "volume '{}' already exists in namespace '{DEFAULT_NAMESPACE}'",
            req.name
        )));
    }
    let rec = match operations::begin(root, "create", &target, &req.request_id, &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = work(root, req).map(|()| target_href(DEFAULT_NAMESPACE, &req.name));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

/// `DeleteVolume` under `root`.
pub fn delete_in(root: &Path, req: &DeleteVolumeRequest) -> Result<Operation, Status> {
    delete_with(root, req, &delete_work)
}

/// [`delete_in`] with the work injected (the tests' seam).
pub(crate) fn delete_with(
    root: &Path,
    req: &DeleteVolumeRequest,
    work: DeleteWork<'_>,
) -> Result<Operation, Status> {
    let namespace = volumes::wanted(&req.namespace).unwrap_or("*");
    let target = format!("Volume/{namespace}/{}", req.name);
    // Before "not found", which the first request itself made true.
    if let Some(found) = operations::replay(root, "delete", &target, &req.request_id, "")? {
        return Ok(operations::message(&found));
    }
    let entry = volumes::find(root, &req.namespace, &req.name)?;
    if req.destroy_remote {
        return Err(Status::unimplemented(
            "DeleteVolume: destroy_remote is in the contract and this engine does not destroy \
             provisioned storage on the node API yet (`delonix volume rm <name> \
             --destroy-remote` does)",
        ));
    }
    let v = &entry.volume;
    if delonix_volume::is_network_driver(&v.driver) {
        return Err(Status::unimplemented(format!(
            "DeleteVolume: volume '{}' is a {} share, whose mount and credentials file the CLI \
             removes with it; this engine does not do that on the node API yet (`delonix volume \
             rm {}` does)",
            v.name, v.driver, v.name
        )));
    }
    if !req.etag.is_empty() {
        let current = volumes::etag_of(&entry);
        if current != req.etag {
            return Err(stale_etag(format!(
                "volume '{}' changed since etag '{}' was read (it is now '{current}')",
                v.name, req.etag
            )));
        }
    }
    if !req.force {
        let users = users_of(root, &entry)?;
        if !users.is_empty() {
            return Err(status_of(&delonix_model::Error::coded(
                5203,
                delonix_model::Error::Conflict(format!(
                    "volume '{}' is in use by {} — remove them first, or delete with force to \
                     destroy the data anyway",
                    v.name,
                    users.join(", ")
                )),
            )));
        }
    }
    // A share's data is not under the record and is never removed here, and a
    // pool volume's data is in the pool (its driver releases it, not this
    // process); a volume that owns its own data directory has to be readable
    // to the end, or the delete would stop half way.
    if v.parent.is_none() && v.pool.is_none() {
        let usage = delonix_volume::measure(Path::new(&v.mountpoint));
        if !usage.is_complete() {
            return Err(status_of(&delonix_model::Error::coded(
                6205,
                delonix_model::Error::Unavailable(format!(
                    "volume '{}' holds data owned by a mapped subuid that this process cannot \
                     read; nothing was removed — `delonix volume rm {}` removes it",
                    v.name, v.name
                )),
            )));
        }
    }
    let rec = match operations::begin(root, "delete", &target, &req.request_id, "")? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = work(root, &entry).map(|()| String::new());
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::v1::{
        GetVolumeRequest, ListOperationsRequest, OperationState, RemoteShare, ShareOf, VolumeSpec,
    };
    use std::path::PathBuf;

    fn create_req(name: &str, request_id: &str) -> CreateVolumeRequest {
        CreateVolumeRequest {
            name: name.into(),
            request_id: request_id.into(),
            labels: [("app".to_string(), "web".to_string())].into(),
            spec: Some(VolumeSpec {
                quota_bytes: 1 << 30,
                alert_percent: 80,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn delete_req(name: &str) -> DeleteVolumeRequest {
        DeleteVolumeRequest {
            name: name.into(),
            ..Default::default()
        }
    }

    fn operations_on_disk(root: &Path) -> usize {
        std::fs::read_dir(root.join("operations"))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    fn get(root: &Path, name: &str) -> Result<crate::proto::v1::Volume, Status> {
        volumes::get_in(
            root,
            &GetVolumeRequest {
                namespace: String::new(),
                name: name.into(),
            },
        )
    }

    #[test]
    fn create_is_persisted_running_before_the_work_and_the_volume_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let seen_running = std::cell::Cell::new(false);
        let work = |r: &Path, req: &CreateVolumeRequest| {
            let list = operations::list_in(
                r,
                &ListOperationsRequest {
                    page: None,
                    active_only: true,
                },
            )
            .unwrap();
            seen_running.set(list.operations.len() == 1);
            create_work(r, req)
        };
        let op = create_with(root, &create_req("data", ""), &work).unwrap();
        assert!(
            seen_running.get(),
            "the operation was not on disk during the work"
        );
        assert_eq!(op.state, OperationState::Succeeded as i32, "{op:?}");
        assert_eq!(op.target, "Volume/default/data");
        let target = op.links.iter().find(|l| l.rel == "target").unwrap();
        assert_eq!(target.href, "/v1/namespaces/default/volumes/data");
        let v = get(root, "data").unwrap();
        let spec = v.spec.unwrap();
        assert_eq!(spec.quota_bytes, 1 << 30);
        assert_eq!(spec.alert_percent, 80);
        assert_eq!(
            v.meta.unwrap().labels.get("app").map(String::as_str),
            Some("web")
        );
    }

    #[test]
    fn the_same_create_sent_twice_is_answered_once_and_a_reused_request_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let first = create_in(root, &create_req("data", "req-1")).unwrap();
        let again = create_in(root, &create_req("data", "req-1")).unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(operations_on_disk(root), 1);
        let mut other = create_req("data", "req-1");
        other.spec.as_mut().unwrap().quota_bytes = 2 << 30;
        let refused = create_in(root, &other).unwrap_err();
        assert_eq!(refused.code(), tonic::Code::AlreadyExists);
        // Without a request_id, the second create finds the name taken.
        let taken = create_in(root, &create_req("data", "")).unwrap_err();
        assert_eq!(taken.code(), tonic::Code::AlreadyExists);
        assert_eq!(operations_on_disk(root), 1);
    }

    #[test]
    fn what_is_not_served_or_wrong_is_refused_before_anything_is_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let with = |f: &dyn Fn(&mut CreateVolumeRequest)| {
            let mut r = create_req("data", "");
            f(&mut r);
            create_in(root, &r).unwrap_err().code()
        };
        use tonic::Code;
        assert_eq!(with(&|r| r.name = "../x".into()), Code::InvalidArgument);
        assert_eq!(with(&|r| r.name = String::new()), Code::InvalidArgument);
        assert_eq!(
            with(&|r| r.spec.as_mut().unwrap().quota_bytes = -1),
            Code::InvalidArgument
        );
        assert_eq!(
            with(&|r| r.spec.as_mut().unwrap().alert_percent = 150),
            Code::InvalidArgument
        );
        assert_eq!(
            with(&|r| r.namespace = "team-a".into()),
            Code::Unimplemented
        );
        assert_eq!(
            with(&|r| {
                r.spec.as_mut().unwrap().source =
                    Some(volume_spec::Source::Remote(RemoteShare::default()))
            }),
            Code::Unimplemented
        );
        assert_eq!(
            with(&|r| {
                r.spec.as_mut().unwrap().source = Some(volume_spec::Source::ShareOf(ShareOf {
                    parent_volume: "p".into(),
                }))
            }),
            Code::Unimplemented
        );
        // An empty `provision.provider` names no pool: refused as the error,
        // before any operation (and before the allowlist is even read), like
        // the other undecided requests here.
        assert_eq!(
            with(&|r| r.spec.as_mut().unwrap().provision = Some(Default::default())),
            Code::InvalidArgument
        );
        assert_eq!(operations_on_disk(root), 0);
        assert!(
            !root.join("volumes").exists(),
            "a refusal created the store"
        );
    }

    #[test]
    fn a_delete_that_succeeds_has_removed_the_volume_and_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        create_in(root, &create_req("data", "")).unwrap();
        let mount = PathBuf::from(
            VolumeStore::open(root)
                .unwrap()
                .inspect("data")
                .unwrap()
                .mountpoint,
        );
        std::fs::write(mount.join("f"), b"12345").unwrap();
        let op = delete_in(root, &delete_req("data")).unwrap();
        assert_eq!(op.state, OperationState::Succeeded as i32, "{op:?}");
        assert_eq!(get(root, "data").unwrap_err().code(), tonic::Code::NotFound);
        assert!(
            !mount.exists(),
            "the delete reported success and left the data"
        );
        // Sent again, a delete of what is gone is NotFound, not a second success.
        let gone = delete_in(root, &delete_req("data")).unwrap_err();
        assert_eq!(gone.code(), tonic::Code::NotFound);
    }

    #[test]
    fn a_volume_a_container_mounts_or_a_share_uses_is_not_deleted_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        create_in(root, &create_req("data", "")).unwrap();
        let mount = VolumeStore::open(root)
            .unwrap()
            .inspect("data")
            .unwrap()
            .mountpoint;
        let mut c = delonix_compute::Container::new(
            "c1".into(),
            "web".into(),
            "/tmp/rootfs".into(),
            vec!["/bin/sh".into()],
            "64M".into(),
        );
        c.mounts = vec![serde_json::from_value(serde_json::json!({
            "source": mount,
            "target": "/data",
            "readonly": false,
        }))
        .unwrap()];
        delonix_state::Store::open(root.join("containers"))
            .unwrap()
            .save(&c)
            .unwrap();
        let refused = delete_in(root, &delete_req("data")).unwrap_err();
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            refused
                .metadata()
                .get(crate::network_ops::DX_METADATA)
                .unwrap(),
            "5203"
        );
        assert!(
            refused.message().contains("container web"),
            "{}",
            refused.message()
        );
        assert!(get(root, "data").is_ok());
        assert_eq!(
            operations_on_disk(root),
            1,
            "the refusal wrote an operation"
        );
        // With force, it goes.
        let mut forced = delete_req("data");
        forced.force = true;
        let op = delete_in(root, &forced).unwrap();
        assert_eq!(op.state, OperationState::Succeeded as i32, "{op:?}");
        assert!(get(root, "data").is_err());
    }

    #[test]
    fn a_stale_etag_is_refused_and_the_current_one_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        create_in(root, &create_req("data", "")).unwrap();
        let etag = get(root, "data").unwrap().meta.unwrap().etag;
        let mut stale = delete_req("data");
        stale.etag = "not-it".into();
        let refused = delete_in(root, &stale).unwrap_err();
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        assert!(refused
            .metadata()
            .get(crate::network_ops::STALE_ETAG_METADATA)
            .is_some());
        let mut current = delete_req("data");
        current.etag = etag;
        let op = delete_in(root, &current).unwrap();
        assert_eq!(op.state, OperationState::Succeeded as i32);
    }

    /// The data of a volume a container wrote under a mapped subuid is not
    /// readable here; removing what IS readable would leave the volume half
    /// deleted, so nothing is removed.
    #[test]
    fn a_volume_with_data_this_process_cannot_read_is_refused_and_left_whole() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        create_in(root, &create_req("data", "")).unwrap();
        let mount = PathBuf::from(
            VolumeStore::open(root)
                .unwrap()
                .inspect("data")
                .unwrap()
                .mountpoint,
        );
        std::fs::write(mount.join("readable"), b"x").unwrap();
        let locked = mount.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("inside"), b"y").unwrap();
        // Restored on every exit, a failed assert included, or the TempDir
        // could not remove it.
        struct Unlock(PathBuf);
        impl Drop for Unlock {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        let unlock = Unlock(locked.clone());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads anything: the test only means something unprivileged.
        // A library test does not print (the `library_prints` ratchet); the skip
        // is this branch, and only a root run takes it.
        if std::fs::read_dir(&locked).is_ok() {
            return;
        }
        let refused = delete_in(root, &delete_req("data")).unwrap_err();
        drop(unlock);
        assert_eq!(refused.code(), tonic::Code::Unavailable, "{refused:?}");
        assert_eq!(
            refused
                .metadata()
                .get(crate::network_ops::DX_METADATA)
                .unwrap(),
            "6205"
        );
        assert!(mount.join("readable").exists(), "the refusal removed data");
        assert!(locked.join("inside").exists());
        assert!(get(root, "data").is_ok());
    }

    #[test]
    fn a_network_volume_and_destroy_remote_are_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        create_in(root, &create_req("data", "")).unwrap();
        let mut remote = delete_req("data");
        remote.destroy_remote = true;
        assert_eq!(
            delete_in(root, &remote).unwrap_err().code(),
            tonic::Code::Unimplemented
        );
        let store = VolumeStore::open(root).unwrap();
        let mut nas = store.inspect("data").unwrap();
        nas.name = "nas".into();
        nas.driver = "nfs".into();
        nas.device = Some("10.0.0.9:/export".into());
        let d = store.volume_dir("nas");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("meta.json"), serde_json::to_vec(&nas).unwrap()).unwrap();
        let refused = delete_in(root, &delete_req("nas")).unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Unimplemented);
        assert!(
            refused.message().contains("delonix volume rm nas"),
            "{}",
            refused.message()
        );
        assert!(get(root, "nas").is_ok());
    }

    /// The whole round trip through the write half against the live MicroCeph:
    /// `CreateVolume` with `spec.provision` carves an RBD image out of a
    /// `ceph-rbd` pool and maps it, and `DeleteVolume` releases it — the image
    /// is on the cluster between the two and gone after (ADR-0081).
    ///
    /// `#[ignore]`: it needs a cluster, so it runs only with `--ignored`, never
    /// in the default suite (see `reports/ceph-live/cluster-access.md`). It
    /// drives the same `create_in`/`delete_in` the gRPC and REST edges call,
    /// through the real pool resolution and routing; only the `Run` is swapped
    /// for one that runs the root map/mount through `sudo -n` (the helper's job
    /// in production), the way the provider crate's own live tests do.
    #[test]
    #[ignore = "needs a live Ceph cluster with an 'rbdpool' RADOS pool: run with \
                --ignored --test-threads=1 (see reports/ceph-live/cluster-access.md)"]
    fn create_provisions_and_delete_releases_an_rbd_image_on_live_ceph() {
        use delonix_storage::exec::{Output, Run};
        use std::sync::Arc;

        struct SudoRun;
        impl Run for SudoRun {
            fn run(&self, argv: &[String]) -> std::io::Result<Output> {
                let out = std::process::Command::new("sudo")
                    .arg("-n")
                    .args(argv)
                    .output()?;
                Ok(Output {
                    code: out.status.code(),
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                })
            }
        }

        /// Does `rbd info rbdpool/<name>` find an image? (Through sudo, like the
        /// driver.)
        fn image_exists(name: &str) -> bool {
            std::process::Command::new("sudo")
                .args([
                    "-n",
                    "rbd",
                    "--conf",
                    "/etc/ceph/ceph.conf",
                    "info",
                    &format!("rbdpool/{name}"),
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // One `ceph-rbd` pool over the session's `rbdpool`, with
        // `mode: in-process` so the routed driver calls ours directly instead
        // of dialling a helper socket this test does not run.
        let pools = root.join("storage-pools.yaml");
        std::fs::write(
            &pools,
            "mode: in-process\n\
             pools:\n  \
             ceph-rbd:\n    \
             driver: ceph-rbd\n    \
             cephPool: rbdpool\n    \
             cephConf: /etc/ceph/ceph.conf\n",
        )
        .unwrap();
        std::env::set_var(delonix_storage::allowlist::PATH_ENV, &pools);

        // Consume the registration `Once` with the production `SystemRun`, then
        // replace `ceph-rbd` with a driver that runs the tools through sudo.
        crate::storage::register_pool_drivers();
        delonix_storage::registry::register(Arc::new(delonix_provider_ceph::RbdDriver::new(
            Arc::new(SudoRun),
        )))
        .unwrap();

        let name = "dlx-nodeapi-live";
        let req = CreateVolumeRequest {
            name: name.into(),
            spec: Some(VolumeSpec {
                quota_bytes: 64 * (1 << 20),
                provision: Some(crate::proto::v1::ProvisionRequest {
                    provider: "ceph-rbd".into(),
                    extensions: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Clear anything a prior aborted run left: the record, then the image.
        let _ = delete_in(root, &delete_req(name));
        let _ = std::process::Command::new("sudo")
            .args([
                "-n",
                "rbd",
                "--conf",
                "/etc/ceph/ceph.conf",
                "rm",
                &format!("rbdpool/{name}"),
            ])
            .output();

        // --- provision ---
        let op = create_in(root, &req).unwrap();
        assert_eq!(
            op.state,
            OperationState::Succeeded as i32,
            "provision did not succeed: {op:?}"
        );

        // The record names the pool and points at a mapped block device.
        let rec = VolumeStore::open(root).unwrap().inspect(name).unwrap();
        assert_eq!(
            rec.pool.as_deref(),
            Some("ceph-rbd"),
            "record keeps its pool"
        );
        assert!(
            rec.mountpoint.starts_with("/dev/rbd"),
            "the allocation is a mapped RBD device: {}",
            rec.mountpoint
        );
        assert!(
            image_exists(name),
            "the RBD image is on the cluster after create"
        );
        // And it reads back through the read half.
        assert!(get(root, name).is_ok(), "the volume reads back");

        // --- release ---
        let op = delete_in(root, &delete_req(name)).unwrap();
        assert_eq!(
            op.state,
            OperationState::Succeeded as i32,
            "release did not succeed: {op:?}"
        );
        assert_eq!(
            get(root, name).unwrap_err().code(),
            tonic::Code::NotFound,
            "the volume is gone from the store"
        );
        assert!(
            !image_exists(name),
            "the RBD image was released from the cluster"
        );
    }
}

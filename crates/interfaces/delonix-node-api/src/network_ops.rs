//! `NetworkService`, the write half: `CreateNetwork` and `DeleteNetwork`
//! (ADR-0042 step E).
//!
//! Each answers with an [`Operation`] persisted before the work starts
//! ([`crate::operations`]). What can be decided without touching anything is
//! refused BEFORE an operation exists — a name already taken, an etag that
//! does not match, a network something is attached to — as the error itself.
//! A failure of the work is the operation, `FAILED`, with the engine's code.
//!
//! The work is `delonix_sdn::netops` — the same two writes, rollback and
//! in-use check the CLI goes through.

use std::path::Path;

use tonic::Status;

use crate::networks::NAMESPACE;
use crate::operations::{self, Begun};
use crate::proto::v1::{CreateNetworkRequest, DeleteNetworkRequest, NetworkTopology, Operation};

/// The gRPC metadata key that carries a failure's dictionary number, so the
/// REST encoding answers with the same `DX-` code the CLI prints.
pub const DX_METADATA: &str = "dx-number";

/// Marks a `FAILED_PRECONDITION` as a stale `etag`, which the REST encoding
/// answers with 412 instead of the 400 of any other failed precondition.
pub const STALE_ETAG_METADATA: &str = "stale-etag";

/// A stale `etag`: `FAILED_PRECONDITION`, as `ResourceMeta.etag` says in the
/// contract, never a silent overwrite.
pub fn stale_etag(msg: String) -> Status {
    let mut status = Status::failed_precondition(msg);
    status.metadata_mut().insert(
        STALE_ETAG_METADATA,
        tonic::metadata::MetadataValue::from_static("1"),
    );
    status
}

/// An engine error as the status both encodings answer with: the gRPC code of
/// its class, and its dictionary number in [`DX_METADATA`].
pub fn status_of(e: &delonix_model::Error) -> Status {
    use delonix_model::codes::Class;
    // The text without the class prefix its `Display` adds ("conflict: …"):
    // the REST encoding rebuilds the error from the class, and the prefix
    // would be there twice.
    let text = e.to_string();
    let msg = [
        "conflict: ",
        "invalid argument: ",
        "unavailable: ",
        "timed out: ",
        "permission denied: ",
        "no such ",
    ]
    .iter()
    .find_map(|p| text.strip_prefix(p))
    .unwrap_or(&text)
    .to_string();
    let mut status = match e.class() {
        Class::NotFound => Status::not_found(msg),
        Class::Conflict | Class::NotRunning => Status::failed_precondition(msg),
        Class::InvalidArgument | Class::Usage => Status::invalid_argument(msg),
        Class::Unavailable => Status::unavailable(msg),
        Class::PermissionDenied => Status::permission_denied(msg),
        Class::Timeout => Status::deadline_exceeded(msg),
        Class::Success | Class::SystemFailure => Status::internal(msg),
    };
    if let Ok(v) = e.number().to_string().parse() {
        status.metadata_mut().insert(DX_METADATA, v);
    }
    status
}

/// A network belongs to the node: it is created and removed in `default`.
fn check_namespace(namespace: &str) -> Result<(), Status> {
    if matches!(namespace, "" | NAMESPACE) {
        return Ok(());
    }
    Err(Status::invalid_argument(format!(
        "namespace '{namespace}': a network belongs to the node and lives in '{NAMESPACE}'"
    )))
}

fn store(root: &Path) -> Result<delonix_sdn::NetworkStore, Status> {
    delonix_sdn::NetworkStore::open(root)
        .map_err(|e| Status::internal(format!("opening the network store: {e}")))
}

/// Whether `root` has a declarative record for `name`. Read without opening
/// the store, which would create its directory.
fn declared(root: &Path, name: &str) -> bool {
    root.join("networks").join(name).is_file()
}

/// [`operations::fingerprint_of`] of a `CreateNetwork` request's content —
/// everything that distinguishes what gets created, not just the name
/// `target` already carries.
///
/// `topology` is NOT a part here, on purpose: by the time this is called,
/// `create_with` has already refused `Overlay` and any unknown value, so
/// only `Bridge` and `Unspecified` (0 and 1, both meaning "bridge" per the
/// enum's own comment) can reach this function — and those two carry no
/// distinguishing information between two requests, so fingerprinting the
/// raw wire value would wrongly flag `topology: 0` and `topology:
/// NETWORK_TOPOLOGY_BRIDGE` (1) as different requests. If `Overlay` is ever
/// served here, its own fields belong in this fingerprint at that time —
/// not folded in blind today.
fn create_fingerprint(
    spec: &crate::proto::v1::NetworkSpec,
    labels: &std::collections::HashMap<String, String>,
    annotations: &std::collections::HashMap<String, String>,
) -> String {
    operations::fingerprint_of(&[
        ("ipv4_cidr", &spec.ipv4_cidr),
        ("labels", &operations::canonical_map(labels)),
        ("annotations", &operations::canonical_map(annotations)),
    ])
}

/// `(label, Some(value))` pairs, the shape `NetworkStore::set_metadata` takes.
fn pairs(map: &std::collections::HashMap<String, String>) -> Vec<(String, Option<String>)> {
    let mut v: Vec<_> = map
        .iter()
        .map(|(k, val)| (k.clone(), Some(val.clone())))
        .collect();
    v.sort();
    v
}

/// What `CreateNetwork` does once it is acknowledged.
type CreateWork<'a> = &'a dyn Fn(&Path, &CreateNetworkRequest) -> delonix_model::Result<()>;
/// What `DeleteNetwork` does once it is acknowledged.
type DeleteWork<'a> = &'a dyn Fn(&Path, &str) -> delonix_model::Result<()>;
/// Who is attached to a network: `(root, name, subnet)`.
type Attached<'a> = &'a dyn Fn(&Path, &str, &str) -> delonix_model::Result<Vec<String>>;

/// The real create: the bridge network, then its labels and annotations. A
/// failure to write those removes the network again — a network created
/// without the labels it was asked with would be handed back as a success.
pub fn create_work(root: &Path, req: &CreateNetworkRequest) -> delonix_model::Result<()> {
    let store = delonix_sdn::NetworkStore::open(root)?;
    let cidr = req
        .spec
        .as_ref()
        .map(|s| s.ipv4_cidr.as_str())
        .filter(|c| !c.is_empty());
    delonix_sdn::netops::create_bridge(&store, &req.name, cidr, |_| Ok(None))?;
    if req.labels.is_empty() && req.annotations.is_empty() {
        return Ok(());
    }
    if let Err(e) = store.set_metadata(&req.name, &pairs(&req.labels), &pairs(&req.annotations)) {
        let _ = delonix_sdn::netops::remove(&store, &req.name);
        return Err(e.into());
    }
    Ok(())
}

/// The real delete.
pub fn delete_work(root: &Path, name: &str) -> delonix_model::Result<()> {
    let store = delonix_sdn::NetworkStore::open(root)?;
    Ok(delonix_sdn::netops::remove(&store, name)?)
}

/// Everything recorded as attached to `name`. Fails closed: a store that
/// cannot be read is an error, never "nothing is attached".
pub fn dependents(root: &Path, name: &str, subnet: &str) -> delonix_model::Result<Vec<String>> {
    let containers = delonix_state::Store::open(root.join("containers"))?.list()?;
    let vms: Vec<(String, String)> = delonix_vm::list(root)?
        .into_iter()
        .map(|v| (v.name, v.network))
        .collect();
    Ok(delonix_sdn::netops::dependents_of(
        name,
        subnet,
        &containers,
        &vms,
    ))
}

/// `CreateNetwork` under `root`.
pub fn create_in(root: &Path, req: &CreateNetworkRequest) -> Result<Operation, Status> {
    create_with(root, req, &create_work)
}

/// [`create_in`] with the work injected (the tests' seam).
pub fn create_with(
    root: &Path,
    req: &CreateNetworkRequest,
    work: CreateWork<'_>,
) -> Result<Operation, Status> {
    check_namespace(&req.namespace)?;
    if req.name.is_empty() {
        return Err(Status::invalid_argument("CreateNetwork: name is required"));
    }
    // What is wrong with the request itself is refused before it is
    // acknowledged: a name or a subnet the engine would never accept.
    let invalid = |e: delonix_sdn::Error| status_of(&e.into());
    delonix_sdn::NetworkStore::validate_name(&req.name).map_err(invalid)?;
    let spec = req.spec.clone().unwrap_or_default();
    match NetworkTopology::try_from(spec.topology) {
        Ok(NetworkTopology::Unspecified | NetworkTopology::Bridge) => {}
        Ok(NetworkTopology::Overlay) => {
            return Err(Status::unimplemented(
                "CreateNetwork: the overlay topology is in the contract and this engine \
                 does not serve it on the node API yet (`delonix network create --driver \
                 overlay` does)",
            ))
        }
        Err(_) => {
            return Err(Status::invalid_argument(format!(
                "CreateNetwork: unknown topology {}",
                spec.topology
            )))
        }
    }
    if spec.overlay.is_some() {
        return Err(Status::invalid_argument(
            "CreateNetwork: spec.overlay is only read for the overlay topology",
        ));
    }
    if spec
        .extensions
        .as_ref()
        .is_some_and(|e| !e.by_provider.is_empty())
    {
        return Err(Status::invalid_argument(
            "CreateNetwork: spec.extensions is reported by the node and not accepted on create",
        ));
    }
    if !spec.ipv4_cidr.is_empty() {
        delonix_sdn::NetworkStore::validate_subnet(&spec.ipv4_cidr).map_err(invalid)?;
    }
    let target = format!("Network/{}", req.name);
    let fp = create_fingerprint(&spec, &req.labels, &req.annotations);
    // The same request sent again is answered with the first answer — before
    // the "already exists" check, which the first request itself made true.
    if let Some(found) = operations::replay(root, "create", &target, &req.request_id, &fp)? {
        return Ok(operations::message(&found));
    }
    if declared(root, &req.name) {
        return Err(Status::already_exists(format!(
            "network '{}' already exists",
            req.name
        )));
    }
    let rec = match operations::begin(root, "create", &target, &req.request_id, &fp)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome =
        work(root, req).map(|()| format!("/v1/namespaces/{NAMESPACE}/networks/{}", req.name));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

/// `DeleteNetwork` under `root`.
pub fn delete_in(root: &Path, req: &DeleteNetworkRequest) -> Result<Operation, Status> {
    delete_with(root, req, &dependents, &delete_work)
}

/// [`delete_in`] with the in-use check and the work injected.
pub fn delete_with(
    root: &Path,
    req: &DeleteNetworkRequest,
    attached: Attached<'_>,
    work: DeleteWork<'_>,
) -> Result<Operation, Status> {
    check_namespace(&req.namespace)?;
    if req.force {
        return Err(Status::invalid_argument(
            "DeleteNetwork: force stops a running resource first, and a network has nothing \
             to stop — disconnect or remove what is attached to it",
        ));
    }
    let target = format!("Network/{}", req.name);
    // Before "not found", which the first request itself made true.
    if let Some(found) = operations::replay(root, "delete", &target, &req.request_id, "")? {
        return Ok(operations::message(&found));
    }
    let missing =
        || Status::not_found(format!("network '{}' in namespace '{NAMESPACE}'", req.name));
    if req.name.is_empty() || !declared(root, &req.name) {
        return Err(missing());
    }
    let net = store(root)?.get(&req.name).map_err(|_| missing())?;
    if !req.etag.is_empty() {
        let realized = delonix_sdn::infra::network_list_in(root)
            .iter()
            .any(|d| d.name == net.name);
        let current = crate::networks::message(&net, realized)
            .meta
            .map(|m| m.etag)
            .unwrap_or_default();
        if current != req.etag {
            return Err(stale_etag(format!(
                "network '{}' changed since etag '{}' was read (it is now '{current}')",
                req.name, req.etag
            )));
        }
    }
    let users = attached(root, &req.name, &net.subnet).map_err(|e| status_of(&e))?;
    if !users.is_empty() {
        return Err(status_of(&delonix_model::Error::coded(
            5307,
            delonix_model::Error::Conflict(format!(
                "network '{}' is in use by {} — remove or disconnect them first",
                req.name,
                users.join(", ")
            )),
        )));
    }
    let rec = match operations::begin(root, "delete", &target, &req.request_id, "")? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = work(root, &req.name).map(|()| String::new());
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::v1::{NetworkSpec, OperationState};
    use std::cell::Cell;

    /// The store half of the real create, without the dataplane: the tests'
    /// state root is not the process's, and the dataplane record follows the
    /// process's.
    fn record_only(root: &Path, req: &CreateNetworkRequest) -> delonix_model::Result<()> {
        let store = delonix_sdn::NetworkStore::open(root)?;
        store.create(&req.name)?;
        store.set_metadata(&req.name, &pairs(&req.labels), &pairs(&req.annotations))?;
        Ok(())
    }

    fn forget(root: &Path, name: &str) -> delonix_model::Result<()> {
        delonix_sdn::NetworkStore::open(root)?.remove(name)?;
        Ok(())
    }

    fn nobody(_: &Path, _: &str, _: &str) -> delonix_model::Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn create_req(name: &str, request_id: &str) -> CreateNetworkRequest {
        CreateNetworkRequest {
            name: name.into(),
            request_id: request_id.into(),
            labels: [("app".to_string(), "web".to_string())].into(),
            ..Default::default()
        }
    }

    fn delete_req(name: &str) -> DeleteNetworkRequest {
        DeleteNetworkRequest {
            name: name.into(),
            ..Default::default()
        }
    }

    fn operations_on_disk(root: &Path) -> usize {
        std::fs::read_dir(root.join("operations"))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    #[test]
    fn create_is_persisted_running_before_the_work_and_succeeds_with_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let seen_running = Cell::new(false);
        let work = |r: &Path, req: &CreateNetworkRequest| {
            // While the work runs, the operation is already on disk, RUNNING.
            let list = operations::list_in(
                r,
                &crate::proto::v1::ListOperationsRequest {
                    page: None,
                    active_only: true,
                },
            )
            .unwrap();
            seen_running.set(list.operations.len() == 1);
            record_only(r, req)
        };
        let op = create_with(&root, &create_req("lab", ""), &work).unwrap();
        assert!(
            seen_running.get(),
            "the operation was not on disk during the work"
        );
        assert_eq!(op.state, OperationState::Succeeded as i32);
        assert_eq!(op.verb, "create");
        assert_eq!(op.target, "Network/lab");
        let target = op.links.iter().find(|l| l.rel == "target").unwrap();
        assert_eq!(target.href, "/v1/namespaces/default/networks/lab");
        // And the network reads back with the labels it was asked with.
        let net = crate::networks::get_in(
            &root,
            &crate::proto::v1::GetNetworkRequest {
                name: "lab".into(),
                namespace: String::new(),
            },
        )
        .unwrap();
        assert_eq!(net.meta.unwrap().labels["app"], "web");
    }

    #[test]
    fn a_name_already_taken_is_refused_and_no_operation_is_written() {
        let dir = tempfile::tempdir().unwrap();
        create_with(dir.path(), &create_req("lab", ""), &record_only).unwrap();
        let before = operations_on_disk(dir.path());
        let err = create_with(dir.path(), &create_req("lab", ""), &record_only).unwrap_err();
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
        assert_eq!(operations_on_disk(dir.path()), before);
    }

    #[test]
    fn the_same_create_sent_twice_does_the_work_once() {
        let dir = tempfile::tempdir().unwrap();
        let runs = Cell::new(0);
        let work = |r: &Path, req: &CreateNetworkRequest| {
            runs.set(runs.get() + 1);
            record_only(r, req)
        };
        let first = create_with(dir.path(), &create_req("lab", "rid-1"), &work).unwrap();
        let again = create_with(dir.path(), &create_req("lab", "rid-1"), &work).unwrap();
        assert_eq!(runs.get(), 1);
        assert_eq!(first.id, again.id);
        assert_eq!(again.state, OperationState::Succeeded as i32);
        // The same request_id for another network is not a retry.
        let err = create_with(dir.path(), &create_req("other", "rid-1"), &work).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_failed_create_is_the_operation_failed_with_the_engines_code() {
        let dir = tempfile::tempdir().unwrap();
        let work = |_: &Path, _: &CreateNetworkRequest| -> delonix_model::Result<()> {
            Err(delonix_model::Error::Unavailable(
                "the holder is down".into(),
            ))
        };
        let op = create_with(dir.path(), &create_req("lab", ""), &work).unwrap();
        assert_eq!(op.state, OperationState::Failed as i32);
        let detail = op.error.unwrap();
        assert_eq!(detail.reason, "DX_UNAVAILABLE");
        assert!(detail.metadata["message"].contains("the holder is down"));
        assert!(op.links.iter().all(|l| l.rel != "target"));
    }

    #[test]
    fn what_the_node_api_does_not_create_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut overlay = create_req("lab", "");
        overlay.spec = Some(NetworkSpec {
            topology: NetworkTopology::Overlay as i32,
            ..Default::default()
        });
        let err = create_with(dir.path(), &overlay, &record_only).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);

        let mut elsewhere = create_req("lab", "");
        elsewhere.namespace = "team-a".into();
        let err = create_with(dir.path(), &elsewhere, &record_only).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        for bad in ["", "../evil", "a/b", "bridge"] {
            let err = create_with(dir.path(), &create_req(bad, ""), &record_only).unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{bad}");
        }
        let mut subnet = create_req("lab", "");
        subnet.spec = Some(NetworkSpec {
            ipv4_cidr: "not-a-cidr".into(),
            ..Default::default()
        });
        let err = create_with(dir.path(), &subnet, &record_only).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        // Nothing was created and nothing was acknowledged.
        assert!(!dir.path().join("networks").exists());
        assert_eq!(operations_on_disk(dir.path()), 0);
    }

    #[test]
    fn delete_removes_the_network_and_is_refused_when_it_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        create_with(dir.path(), &create_req("lab", ""), &record_only).unwrap();
        let op = delete_with(dir.path(), &delete_req("lab"), &nobody, &forget).unwrap();
        assert_eq!(op.state, OperationState::Succeeded as i32);
        assert_eq!(op.verb, "delete");
        assert!(op.links.iter().all(|l| l.rel != "target"));
        assert!(!declared(dir.path(), "lab"));

        let err = delete_with(dir.path(), &delete_req("lab"), &nobody, &forget).unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn a_network_in_use_is_refused_with_dx_5307_and_stays() {
        let dir = tempfile::tempdir().unwrap();
        create_with(dir.path(), &create_req("lab", ""), &record_only).unwrap();
        let before = operations_on_disk(dir.path());
        let in_use = |_: &Path, _: &str, _: &str| Ok(vec!["container web".to_string()]);
        let err = delete_with(dir.path(), &delete_req("lab"), &in_use, &forget).unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("container web"));
        // The class prefix is the REST encoding's to add, once.
        assert!(
            err.message().starts_with("network 'lab' is in use"),
            "{}",
            err.message()
        );
        assert_eq!(err.metadata().get(DX_METADATA).unwrap(), "5307");
        assert!(declared(dir.path(), "lab"));
        assert_eq!(operations_on_disk(dir.path()), before);
    }

    #[test]
    fn a_store_that_cannot_be_read_does_not_read_as_nothing_attached() {
        let dir = tempfile::tempdir().unwrap();
        create_with(dir.path(), &create_req("lab", ""), &record_only).unwrap();
        let unreadable = |_: &Path, _: &str, _: &str| -> delonix_model::Result<Vec<String>> {
            Err(delonix_model::Error::PermissionDenied("containers/".into()))
        };
        let err = delete_with(dir.path(), &delete_req("lab"), &unreadable, &forget).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(declared(dir.path(), "lab"));
    }

    #[test]
    fn a_stale_etag_is_a_failed_precondition_and_the_current_one_deletes() {
        let dir = tempfile::tempdir().unwrap();
        create_with(dir.path(), &create_req("lab", ""), &record_only).unwrap();
        let mut stale = delete_req("lab");
        stale.etag = "0000000000000000".into();
        let err = delete_with(dir.path(), &stale, &nobody, &forget).unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.metadata().contains_key(STALE_ETAG_METADATA));
        assert!(declared(dir.path(), "lab"));

        let current = crate::networks::get_in(
            dir.path(),
            &crate::proto::v1::GetNetworkRequest {
                name: "lab".into(),
                namespace: String::new(),
            },
        )
        .unwrap()
        .meta
        .unwrap()
        .etag;
        let mut fresh = delete_req("lab");
        fresh.etag = current;
        let op = delete_with(dir.path(), &fresh, &nobody, &forget).unwrap();
        assert_eq!(op.state, OperationState::Succeeded as i32);
    }

    #[test]
    fn the_same_delete_sent_twice_is_answered_once_not_with_not_found() {
        let dir = tempfile::tempdir().unwrap();
        create_with(dir.path(), &create_req("lab", ""), &record_only).unwrap();
        let mut req = delete_req("lab");
        req.request_id = "del-1".into();
        let first = delete_with(dir.path(), &req, &nobody, &forget).unwrap();
        let again = delete_with(dir.path(), &req, &nobody, &forget).unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(again.state, OperationState::Succeeded as i32);
    }

    #[test]
    fn force_is_refused_for_a_network() {
        let dir = tempfile::tempdir().unwrap();
        create_with(dir.path(), &create_req("lab", ""), &record_only).unwrap();
        let mut req = delete_req("lab");
        req.force = true;
        let err = delete_with(dir.path(), &req, &nobody, &forget).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(declared(dir.path(), "lab"));
    }
}

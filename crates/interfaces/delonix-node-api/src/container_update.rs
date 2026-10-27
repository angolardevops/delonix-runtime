//! `ContainerService::UpdateContainer`: a LIVE resource resize.
//!
//! The write half's one hot-path: rewrite a running container's cgroup limits
//! without restarting it — `memory.max`, `cpu.max`, `cpu.weight`, `cpuset.cpus`.
//! The write is [`delonix_linux::update_kube_resources`], the SAME function the
//! CRI's `UpdateContainerResources` drives (ADR 0038 item 4), reused unchanged:
//! a node-API container carries no kubelet systemd scope, so it takes that
//! function's direct-cgroup-write branch, exactly as `container run` does.
//!
//! Two things make this NOT the CRI's call, though it shares the writer:
//!
//! * The field mask is authoritative. `UpdateContainerRequest.update_mask` is
//!   REQUIRED (an empty mask is `INVALID_ARGUMENT`, per the proto); it names
//!   which fields this call touches. That is cleaner than the CRI's "a zero
//!   means not-touched" convention — the mask disambiguates "leave it" from
//!   "set it" without overloading a sentinel. Only `spec.resources.*` is served
//!   on this path; a mask that reaches for anything else is refused, not
//!   silently ignored (the same honesty [`crate::container_launch`] creates with).
//!
//! * This pass does not CLEAR a limit. A leaf named by the mask whose value is
//!   unset (0/empty) is `INVALID_ARGUMENT`, never a write of `max` to the live
//!   cgroup — lowering a limit is the resize this serves; lifting one back to
//!   unbounded is a recreate.
//!
//! `memory_reservation_bytes` and `pids_limit` are refused with the same
//! `UNIMPLEMENTED` [`crate::container_launch::spec_to_run_opts`] refuses them at
//! create time: a field the node-API create path cannot set is one the update
//! path has no coherent prior state to mutate.

use std::path::Path;

use tonic::Status;

use crate::network_ops::{stale_etag, status_of};
use crate::proto::v1::{Container, Resources, UpdateContainerRequest};

/// The engine's persisted container record (not the proto message).
type Record = delonix_compute::Container;

fn requested_namespace(namespace: &str) -> &str {
    if namespace.is_empty() {
        "default"
    } else {
        namespace
    }
}

/// A field in the contract this path does not resize yet: `UNIMPLEMENTED`,
/// naming what does — the same shape `container_launch`'s own `not_served_yet`
/// refuses the create-time fields with.
fn not_served_yet(what: &str, how: &str) -> Status {
    Status::unimplemented(format!(
        "UpdateContainer: {what} is in the contract and this engine does not resize it on the \
         node API yet ({how})"
    ))
}

/// The engine strings a resize writes, lowered from the contract's [`Resources`]
/// exactly as [`crate::container_launch::spec_to_run_opts`] lowers them at
/// create time. Each is `Some` only when this update touches it (the mask named
/// it AND it carries a value) — the partial-update contract.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub memory: Option<String>,
    pub cpus: Option<String>,
    pub cpu_weight: Option<String>,
    pub cpuset: Option<String>,
}

impl Plan {
    fn is_empty(&self) -> bool {
        self.memory.is_none()
            && self.cpus.is_none()
            && self.cpu_weight.is_none()
            && self.cpuset.is_none()
    }
}

/// A single resource leaf named by the field mask applied to `r` (`Some`) — or
/// a refusal. Returns `Ok(false)` for a leaf this pass leaves untouched because
/// its value is unset, so the caller can tell "masked, nothing to do" from
/// "masked, but you cannot clear it here" (`INVALID_ARGUMENT`).
fn apply_leaf(plan: &mut Plan, leaf: &str, r: &Resources, explicit: bool) -> Result<bool, Status> {
    // An explicitly-named leaf with no value is a clear request, which this
    // pass does not serve; the same leaf reached via the `spec.resources`
    // parent is simply skipped (nothing to resize there).
    let unclearable = |field: &str| {
        if explicit {
            Err(Status::invalid_argument(format!(
                "UpdateContainer: '{field}' is in update_mask but its value is unset — this pass \
                 resizes a limit, it does not clear one (recreate the container to lift it)"
            )))
        } else {
            Ok(false)
        }
    };
    match leaf {
        "memory_limit_bytes" => {
            if r.memory_limit_bytes > 0 {
                plan.memory = Some(r.memory_limit_bytes.to_string());
                Ok(true)
            } else {
                unclearable("spec.resources.memory_limit_bytes")
            }
        }
        "cpu_limit_millis" => {
            if r.cpu_limit_millis > 0 {
                plan.cpus = Some(format!("{:.3}", r.cpu_limit_millis as f64 / 1000.0));
                Ok(true)
            } else {
                unclearable("spec.resources.cpu_limit_millis")
            }
        }
        "cpu_weight" => {
            if r.cpu_weight > 0 {
                plan.cpu_weight = Some(r.cpu_weight.to_string());
                Ok(true)
            } else {
                unclearable("spec.resources.cpu_weight")
            }
        }
        "cpuset" => {
            if !r.cpuset.is_empty() {
                plan.cpuset = Some(r.cpuset.clone());
                Ok(true)
            } else {
                unclearable("spec.resources.cpuset")
            }
        }
        "memory_reservation_bytes" => Err(not_served_yet(
            "spec.resources.memory_reservation_bytes",
            "the cgroup `memory.low` path is not on the node API",
        )),
        "pids_limit" => Err(not_served_yet(
            "spec.resources.pids_limit",
            "`delonix run --pids-limit` is not on the node API",
        )),
        other => Err(Status::invalid_argument(format!(
            "UpdateContainer: unknown resource field 'spec.resources.{other}' in update_mask"
        ))),
    }
}

/// PURE. The mask (relative to the `Container` message) and the request's
/// `spec.resources` → the [`Plan`] of live writes, or the refusal to make
/// instead. Every decision this RPC makes about WHAT to touch is here, before
/// any store or cgroup is opened.
pub fn plan_update(paths: &[String], resources: Option<&Resources>) -> Result<Plan, Status> {
    if paths.is_empty() {
        return Err(Status::invalid_argument(
            "UpdateContainer: update_mask is required and must not be empty",
        ));
    }
    // Every served path lives under `spec.resources`; resources must be carried.
    let r = resources.ok_or_else(|| {
        Status::invalid_argument(
            "UpdateContainer: update_mask names spec.resources fields but \
             container.spec.resources is absent",
        )
    })?;
    let mut plan = Plan::default();
    for raw in paths {
        let path = raw.trim();
        if path == "spec.resources" {
            // The parent: every supported leaf that carries a value.
            for leaf in ["memory_limit_bytes", "cpu_limit_millis", "cpu_weight", "cpuset"] {
                apply_leaf(&mut plan, leaf, r, false)?;
            }
        } else if let Some(leaf) = path.strip_prefix("spec.resources.") {
            apply_leaf(&mut plan, leaf, r, true)?;
        } else {
            return Err(not_served_yet(
                &format!("update_mask path '{path}'"),
                "this pass resizes only spec.resources.* — recreate the container to change the rest",
            ));
        }
    }
    if plan.is_empty() {
        return Err(Status::invalid_argument(
            "UpdateContainer: update_mask selected no resource to resize",
        ));
    }
    Ok(plan)
}

/// Load a container by name under `root`, requiring its recorded namespace to
/// match — the identity rule the whole service reads with.
fn load(root: &Path, name: &str, namespace: &str) -> Result<(delonix_state::Store, Record), Status> {
    let wanted = requested_namespace(namespace);
    let missing = || Status::not_found(format!("container '{name}' in namespace '{wanted}'"));
    if wanted == "*" || !root.join("containers").is_dir() {
        return Err(missing());
    }
    let store =
        delonix_state::Store::open(root.join("containers")).map_err(|e| status_of(&e.into()))?;
    let c = store.load(name).map_err(|e| {
        if e.is_not_found() {
            missing()
        } else {
            status_of(&e.into())
        }
    })?;
    if c.namespace != wanted {
        return Err(missing());
    }
    Ok((store, c))
}

/// `UpdateContainer`: resize the live cgroup from `container.spec.resources`,
/// masked by `update_mask`, then answer with the updated [`Container`].
pub fn update_in(root: &Path, req: &UpdateContainerRequest) -> Result<Container, Status> {
    let mask = req
        .update_mask
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("UpdateContainer: update_mask is required"))?;

    // Identity: `container.meta.name`/`namespace`, when carried, must equal the
    // path — a mismatch is a rename, which this RPC is not (per the proto).
    let meta = req.container.as_ref().and_then(|c| c.meta.as_ref());
    if let Some(m) = meta {
        if !m.name.is_empty() && m.name != req.name {
            return Err(Status::invalid_argument(format!(
                "UpdateContainer: container.meta.name '{}' does not match the path name '{}' — \
                 this is not a rename",
                m.name, req.name
            )));
        }
        if !m.namespace.is_empty()
            && requested_namespace(&m.namespace) != requested_namespace(&req.namespace)
        {
            return Err(Status::invalid_argument(format!(
                "UpdateContainer: container.meta.namespace '{}' does not match the path namespace \
                 '{}'",
                m.namespace, req.namespace
            )));
        }
    }

    let resources = req
        .container
        .as_ref()
        .and_then(|c| c.spec.as_ref())
        .and_then(|s| s.resources.as_ref());
    let plan = plan_update(&mask.paths, resources)?;

    let (store, mut c) = load(root, &req.name, &req.namespace)?;

    // Optimistic concurrency: `container.meta.etag`, when carried, must still be
    // the record's current etag — the same check `DeleteContainer` makes.
    if let Some(etag) = meta.map(|m| m.etag.as_str()).filter(|e| !e.is_empty()) {
        let current = crate::containers::message(&c)
            .meta
            .map(|m| m.etag)
            .unwrap_or_default();
        if current != etag {
            return Err(stale_etag(format!(
                "container '{}' changed since etag '{etag}' was read (it is now '{current}')",
                req.name
            )));
        }
    }

    // Refresh liveness so the write targets the pid's real cgroup (and so a
    // container that died since the record was written is seen as stopped).
    delonix_linux::reconcile_status(&mut c);

    let update = delonix_linux::ResourceUpdate {
        memory: plan.memory.as_deref(),
        cpus: plan.cpus.as_deref(),
        cpu_weight: plan.cpu_weight.as_deref(),
        cpuset: plan.cpuset.as_deref(),
        cpuset_mems: None,
        hugepage_limits: &[],
        unified: &[],
        oom_score_adj: None,
    };
    match delonix_linux::update_kube_resources(&c, &update).map_err(|e| status_of(&e.into()))? {
        // The container looked alive but its cgroup was gone by the time we
        // wrote: the request is not in force and will not become so. The proto
        // has no field to carry that back, so it is logged (as the CRI's own
        // path does) rather than passed off as a clean success in silence.
        delonix_linux::LimitUpdate::NotEnforced => tracing::warn!(
            container = %req.name,
            "UpdateContainer did not reach a live cgroup — the container looked alive and its \
             cgroup is gone; the record is updated but no live limit changed"
        ),
        // Applied (live) or Deferred (stopped: the next start rebuilds the
        // cgroup from the record this call is about to persist). Both keep the
        // record as the full desired state.
        delonix_linux::LimitUpdate::Applied | delonix_linux::LimitUpdate::Deferred => {}
    }

    // The record is the full desired state: persist what this call changed so a
    // later `start` (which rebuilds the cgroup from the record) keeps it.
    if let Some(m) = &plan.memory {
        c.memory_max = m.clone();
    }
    if let Some(cp) = &plan.cpus {
        c.cpus = cp.clone();
    }
    if plan.cpu_weight.is_some() {
        c.cpu_weight = plan.cpu_weight.clone();
    }
    if plan.cpuset.is_some() {
        c.cpuset = plan.cpuset.clone();
    }
    store.save(&c).map_err(|e| status_of(&e.into()))?;

    Ok(crate::containers::message(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res() -> Resources {
        Resources {
            memory_limit_bytes: 64 * 1024 * 1024,
            cpu_limit_millis: 500,
            cpu_weight: 200,
            cpuset: "0-1".into(),
            ..Default::default()
        }
    }

    fn mask(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_empty_mask_is_invalid() {
        let err = plan_update(&[], Some(&res())).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_single_leaf_lowers_only_that_one() {
        let plan =
            plan_update(&mask(&["spec.resources.memory_limit_bytes"]), Some(&res())).unwrap();
        assert_eq!(plan.memory.as_deref(), Some("67108864"));
        assert_eq!(plan.cpus, None);
        assert_eq!(plan.cpu_weight, None);
        assert_eq!(plan.cpuset, None);
    }

    #[test]
    fn cpu_millis_lower_to_the_same_core_string_create_uses() {
        let plan = plan_update(&mask(&["spec.resources.cpu_limit_millis"]), Some(&res())).unwrap();
        assert_eq!(plan.cpus.as_deref(), Some("0.500"));
    }

    #[test]
    fn the_parent_path_takes_every_set_leaf_and_skips_the_unset() {
        let r = Resources {
            memory_limit_bytes: 32 * 1024 * 1024,
            cpu_limit_millis: 0, // unset: skipped via the parent, not an error
            cpu_weight: 0,
            cpuset: String::new(),
            ..Default::default()
        };
        let plan = plan_update(&mask(&["spec.resources"]), Some(&r)).unwrap();
        assert_eq!(plan.memory.as_deref(), Some("33554432"));
        assert_eq!(plan.cpus, None);
        assert_eq!(plan.cpu_weight, None);
        assert_eq!(plan.cpuset, None);
    }

    #[test]
    fn an_explicitly_masked_but_unset_leaf_is_invalid_not_a_clear() {
        let r = Resources {
            memory_limit_bytes: 0,
            ..Default::default()
        };
        let err =
            plan_update(&mask(&["spec.resources.memory_limit_bytes"]), Some(&r)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn reservation_and_pids_are_refused_like_create() {
        for leaf in [
            "spec.resources.memory_reservation_bytes",
            "spec.resources.pids_limit",
        ] {
            let r = Resources {
                memory_reservation_bytes: 1,
                pids_limit: 1,
                ..Default::default()
            };
            let err = plan_update(&mask(&[leaf]), Some(&r)).unwrap_err();
            assert_eq!(err.code(), tonic::Code::Unimplemented, "{leaf}");
        }
    }

    #[test]
    fn a_non_resource_path_is_refused_not_ignored() {
        for path in ["spec.image", "spec.command", "meta.labels", "spec", "status"] {
            let err = plan_update(&mask(&[path]), Some(&res())).unwrap_err();
            assert_eq!(err.code(), tonic::Code::Unimplemented, "{path}");
        }
    }

    #[test]
    fn a_mask_that_only_touches_resources_but_carries_none_is_invalid() {
        let err = plan_update(&mask(&["spec.resources.memory_limit_bytes"]), None).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn an_unknown_resource_leaf_is_invalid() {
        let err = plan_update(&mask(&["spec.resources.nope"]), Some(&res())).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}

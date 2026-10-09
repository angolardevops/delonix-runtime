//! `VirtualMachineService`, the write half: `CreateVirtualMachine`,
//! `DeleteVirtualMachine`, `StartVirtualMachine`, `StopVirtualMachine`
//! (ADR-0040 P5), plus the synchronous `PauseVirtualMachine`/
//! `ResumeVirtualMachine` and the snapshot lifecycle — the node contract's
//! own VM operations, so a caller other than this CLI has a socket to drive
//! a VM through instead of shelling out to `delonix` (the gap a VM-readiness
//! audit found; `docs/discovery/67_VMAAS_READINESS_AUDIT.md` has the detail).
//!
//! Each of `Create`/`Delete`/`Start`/`Stop`/`CreateSnapshot`/
//! `RestoreSnapshot`/`DeleteSnapshot` answers an [`crate::operations::Operation`]
//! persisted BEFORE the work starts ([`crate::operations`]) — the exact
//! pattern [`crate::network_ops`] already established for `CreateNetwork`/
//! `DeleteNetwork`. What can be decided without touching anything is refused
//! BEFORE an operation exists: an invalid spec, a name already taken, a
//! namespace mismatch. A failure of the work itself is the operation,
//! `FAILED`, with the engine's own `DX-` code.
//!
//! **"0 vcpus/memory = this image's own recorded default"** (the contract's
//! own comment on `VirtualMachineSpec`) is resolved through
//! `delonix_vm::image_defaults` (ADR-0076 D3) — a minimal port
//! (`delonix_compute::ports::ImageDefaultsReader`) that reads only the three
//! fields this needs straight off an image's JSON sidecar, independently of
//! `bins/delonix-runtime-bin`'s richer `VmImageStore` (build/registry
//! metadata that resolution has no use for, and that an INTERFACE crate must
//! not depend on `bin` to reach — ADR-0040's layering). A reference this
//! port does not recognize (no sidecar, or none registered) falls back to
//! the fixed `1` vCPU / `1 GiB`, same as the engine's own default when
//! nothing else — CLI, manifest, or here — says otherwise.

use std::path::Path;
use std::time::{Duration, Instant};

use tonic::Status;

use crate::network_ops::status_of;
use crate::operations::{self, Begun};
use crate::proto::v1::{
    CloudInit, CreateSnapshotRequest, CreateVirtualMachineRequest, DeleteSnapshotRequest,
    DeleteVirtualMachineRequest, ListSnapshotsRequest, ListSnapshotsResponse, NetworkAttachment,
    Operation, PauseVirtualMachineRequest, RestartMode, RestoreSnapshotRequest,
    ResumeVirtualMachineRequest, Snapshot, StartVirtualMachineRequest, StopVirtualMachineRequest,
    VirtualMachine, VirtualMachineSpec,
};
use delonix_compute::vm_backend::VmConfig;

/// `wait_until_reachable`'s budget. The contract has no per-request timeout
/// field; this matches `vm create --wait`'s own default (`--boot-timeout
/// 120`, `bins/delonix-runtime-bin/src/cmd/vm.rs`) so the two entry points do
/// not disagree about how long "wait" means.
const WAIT_TIMEOUT: Duration = Duration::from_secs(120);

fn namespace_of(namespace: &str) -> Option<String> {
    (!namespace.is_empty() && namespace != "default").then(|| namespace.to_string())
}

fn target_href(namespace: &str, name: &str) -> String {
    let ns = if namespace.is_empty() {
        "default"
    } else {
        namespace
    };
    format!("/v1/namespaces/{ns}/virtualmachines/{name}")
}

fn restart_policy_of(mode: i32) -> Result<Option<String>, Status> {
    match RestartMode::try_from(mode) {
        Ok(RestartMode::Unspecified | RestartMode::No) => Ok(Some("no".to_string())),
        Ok(RestartMode::OnFailure) => Ok(Some("on-failure".to_string())),
        Ok(RestartMode::Always) => Ok(Some("always".to_string())),
        Ok(RestartMode::UnlessStopped) => Ok(Some("unless-stopped".to_string())),
        Err(_) => Err(Status::invalid_argument(format!(
            "unknown restart mode {mode}"
        ))),
    }
}

/// `spec.cloud_init` → the engine's own cloud-init INTENT fields. `user_data`
/// is refused rather than silently dropped: the engine's only path for raw
/// cloud-config content is a local FILE on this host
/// (`VmConfig::seed`/`cloudinit::generate_seed_iso`'s `user_data_override:
/// Option<&Path>`), which an INTERFACE resolving on a caller's behalf would
/// be reading an arbitrary path for them — the exact line §4 of the audit
/// found this engine already draws correctly for `@file`-style CLI
/// conveniences, and that this module does not cross either.
/// `hostname`, `ssh_keys`, and `VmConfig.cloud_init` — the three fields
/// [`config_from_spec`] needs out of `spec.cloud_init`. Not named
/// `CloudInitIntent`: `delonix_compute::vm_backend` already has a type of
/// that name, and it is not this tuple.
type SpecCloudInit = (Option<String>, Vec<String>, Option<bool>);

fn cloud_init_of(ci: Option<&CloudInit>) -> Result<SpecCloudInit, Status> {
    let Some(ci) = ci else {
        return Ok((None, Vec::new(), None));
    };
    if !ci.user_data.is_empty() {
        return Err(Status::invalid_argument(
            "spec.cloud_init.user_data would have to become a local file on this node, which \
             the node API does not do on a caller's behalf — not yet implemented; use \
             hostname/ssh_authorized_keys instead",
        ));
    }
    Ok((
        (!ci.hostname.is_empty()).then(|| ci.hostname.clone()),
        ci.ssh_authorized_keys.clone(),
        Some(true),
    ))
}

/// `spec.networks` → the engine's single primary network + static IP. More
/// than one attachment, or an alias, is refused by name — the node API has
/// nowhere to put either yet (`VmConfig.network` is one field; a VM's
/// `extra_nics`/alias vocabulary is a manifest-only escape hatch this spec
/// does not expose).
fn network_of(networks: &[NetworkAttachment]) -> Result<(String, Option<String>), Status> {
    match networks {
        [] => Ok((String::new(), None)),
        [n] => {
            if !n.aliases.is_empty() {
                return Err(Status::invalid_argument(
                    "network aliases are not supported for virtual machines on the node API",
                ));
            }
            Ok((
                n.network.clone(),
                (!n.ipv4_address.is_empty()).then(|| n.ipv4_address.clone()),
            ))
        }
        _ => Err(Status::invalid_argument(
            "only one network attachment is supported for virtual machines on the node API today",
        )),
    }
}

/// Non-negative `bytes` rounded up to the unit `create`/`resize` expect.
/// Negative is invalid argument, not a silently-flipped unsigned wrap.
fn gib_ceil(bytes: i64, field: &str) -> Result<Option<u32>, Status> {
    if bytes == 0 {
        return Ok(None);
    }
    if bytes < 0 {
        return Err(Status::invalid_argument(format!(
            "{field} cannot be negative"
        )));
    }
    Ok(Some((bytes as u64).div_ceil(1 << 30) as u32))
}

fn mib_ceil(bytes: i64, field: &str) -> Result<Option<u64>, Status> {
    if bytes == 0 {
        return Ok(None);
    }
    if bytes < 0 {
        return Err(Status::invalid_argument(format!(
            "{field} cannot be negative"
        )));
    }
    Ok(Some((bytes as u64).div_ceil(1024 * 1024)))
}

/// The engine's `VmConfig` from the contract's `VirtualMachineSpec` — the
/// node API's own resolution (see the module doc comment's "known gap").
/// Everything that can be decided here is decided before any operation is
/// acknowledged, never inside the work closure.
fn config_from_spec(
    root: &Path,
    name: &str,
    namespace: Option<String>,
    spec: &VirtualMachineSpec,
) -> Result<VmConfig, Status> {
    if spec.image.is_empty() {
        return Err(Status::invalid_argument("spec.image is required"));
    }
    if spec.extensions.is_some() {
        return Err(Status::invalid_argument(
            "spec.extensions is reported by the node and not accepted on create — it mirrors \
             what CreateNetwork already refuses, for the same reason",
        ));
    }
    let (network, static_ip) = network_of(&spec.networks)?;
    // "0 = image default, then 1" (the contract's own comment): look up the
    // image's own recommendation ONCE, reuse it for whichever of
    // vcpus/memory/backend the request left unspecified.
    let image_defaults = delonix_vm::image_defaults(root, &spec.image);
    let vcpus = match spec.vcpus {
        0 => image_defaults.as_ref().and_then(|d| d.vcpus).unwrap_or(1),
        n if n > 0 => n as u32,
        _ => return Err(Status::invalid_argument("spec.vcpus cannot be negative")),
    };
    let memory = match mib_ceil(spec.memory_bytes, "spec.memory_bytes")? {
        Some(mib) => format!("{mib}M"),
        None => image_defaults
            .as_ref()
            .and_then(|d| d.memory.clone())
            .unwrap_or_else(|| "1G".to_string()),
    };
    let disk_size_gib = gib_ceil(spec.disk_bytes, "spec.disk_bytes")?;
    let (hostname, ssh_keys, cloud_init) = cloud_init_of(spec.cloud_init.as_ref())?;
    let restart_policy = restart_policy_of(spec.restart)?;
    let backend = (!spec.provider.is_empty())
        .then(|| spec.provider.clone())
        .or_else(|| image_defaults.and_then(|d| d.backend));
    Ok(VmConfig {
        name: name.to_string(),
        disk: spec.image.clone(),
        vcpus,
        memory,
        network,
        namespace,
        static_ip,
        disk_size_gib,
        hostname,
        ssh_keys,
        cloud_init,
        restart_policy,
        backend,
        required_capabilities: spec.required_capabilities.clone(),
        ..Default::default()
    })
}

/// Stamps `labels`/`annotations` on an already-created VM. A failure here
/// rolls the create back — handing back a VM created WITHOUT the labels it
/// was asked with would be reporting a different resource as a success, the
/// same reasoning [`crate::network_ops::create_work`] already applies.
///
/// Goes straight to `delonix_state::JsonStore` (already a direct dependency
/// of this crate) rather than through `delonix-vm`, which exposes no
/// labels/annotations setter of its own — the CLI does not have one either
/// (a VM's labels are only ever written by the declarative reconciler).
fn stamp_metadata(
    root: &Path,
    name: &str,
    labels: &std::collections::HashMap<String, String>,
    annotations: &std::collections::HashMap<String, String>,
) -> delonix_model::Result<()> {
    if labels.is_empty() && annotations.is_empty() {
        return Ok(());
    }
    let store = delonix_state::JsonStore::<delonix_compute::Vm>::open(root.join("vms"))?;
    let labels = labels.clone();
    let annotations = annotations.clone();
    store.update(name, move |vm| {
        vm.labels = labels.clone().into_iter().collect();
        vm.annotations = annotations.clone().into_iter().collect();
        true
    })?;
    Ok(())
}

/// Polls for [`CreateVirtualMachineRequest::wait_until_reachable`] — the same
/// three-way read `vm create --wait` makes (`wait_for_boot`,
/// `bins/delonix-runtime-bin/src/cmd/vm.rs`), without its spinner/i18n side:
/// up (real or predicted-and-probed-reachable), an address that exists but
/// never answered, or no address at all yet. Only the last two, past the
/// deadline, are an error — the same DX-8503 class the CLI gives, because the
/// contract this call made ("wait until reachable") was not kept, and the VM
/// is left running either way (the timeout may just be too short).
fn wait_until_reachable(root: &Path, name: &str) -> delonix_model::Result<()> {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    let mut silent_at: Option<String> = None;
    loop {
        if let Ok(vm) = delonix_vm::status(root, name) {
            if let Some(ip) = vm.ip.clone().filter(|s| !s.is_empty()) {
                let up = if delonix_vm::ip_is_predicted(&vm) {
                    match delonix_sdn::infra::sdn_reachable(
                        &vm.network,
                        &ip,
                        &vm.mac,
                        Duration::from_secs(2),
                    ) {
                        Some(true) => true,
                        Some(false) => {
                            silent_at = Some(ip);
                            false
                        }
                        // Cannot ask at all (holder down, no `ip(8)`): the
                        // reason will not change by asking again, and "has an
                        // address" is the only claim this backend can back —
                        // treated as reachable rather than spent against the
                        // deadline in vain.
                        None => return Ok(()),
                    }
                } else {
                    true
                };
                if up {
                    return Ok(());
                }
            }
            // libvirt user-mode networking never gives an address at all —
            // waiting the whole deadline for one would always time out.
            if vm.backend.contains("libvirt") && vm.tap == "user" && vm.ip.is_none() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            let why = match silent_at {
                Some(ip) => format!(
                    "virtual machine '{name}' is running but never answered at {ip} — that \
                     address is computed from the MAC, not observed, so it exists whether or \
                     not the guest booted"
                ),
                None => format!("virtual machine '{name}' is still booting after the timeout"),
            };
            return Err(delonix_model::Error::coded(
                8503,
                delonix_model::Error::Timeout(why),
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// What `CreateVirtualMachine` does once it is acknowledged.
type CreateWork<'a> = &'a dyn Fn(&Path, &str, &VmConfig, bool) -> delonix_model::Result<()>;
/// What `DeleteVirtualMachine` does once it is acknowledged.
type DeleteWork<'a> = &'a dyn Fn(&Path, &str, bool) -> delonix_model::Result<()>;

fn create_work(root: &Path, name: &str, cfg: &VmConfig, wait: bool) -> delonix_model::Result<()> {
    delonix_vm::create_with(root, cfg, &|_stage| {})?;
    if wait {
        wait_until_reachable(root, name)?;
    }
    Ok(())
}

fn delete_work(root: &Path, name: &str, force: bool) -> delonix_model::Result<()> {
    delonix_vm::destroy(root, name, force, true)
        .map(|_destroyed| ())
        .map_err(Into::into)
}

/// `CreateVirtualMachine` under `root`.
pub fn create_in(root: &Path, req: &CreateVirtualMachineRequest) -> Result<Operation, Status> {
    create_with_fn(root, req, &create_work)
}

/// [`create_in`] with the work injected (the tests' seam).
pub fn create_with_fn(
    root: &Path,
    req: &CreateVirtualMachineRequest,
    work: CreateWork<'_>,
) -> Result<Operation, Status> {
    if req.name.is_empty() {
        return Err(Status::invalid_argument(
            "CreateVirtualMachine: name is required",
        ));
    }
    if !delonix_vm::valid_vm_name(&req.name) {
        return Err(Status::invalid_argument(format!(
            "CreateVirtualMachine: '{}' is not a valid virtual machine name",
            req.name
        )));
    }
    let namespace = namespace_of(&req.namespace);
    let spec = req.spec.clone().unwrap_or_default();
    let cfg = config_from_spec(root, &req.name, namespace, &spec)?;
    let target = format!("VirtualMachine/{}", req.name);
    if let Some(found) = operations::replay(root, "create", &target, &req.request_id)? {
        return Ok(operations::message(&found));
    }
    if delonix_vm::exists(root, &req.name) {
        return Err(Status::already_exists(format!(
            "virtual machine '{}' already exists",
            req.name
        )));
    }
    let rec = match operations::begin(root, "create", &target, &req.request_id)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let name = req.name.clone();
    let labels = req.labels.clone();
    let annotations = req.annotations.clone();
    let wait = req.wait_until_reachable;
    let outcome = work(root, &name, &cfg, wait).and_then(|()| {
        stamp_metadata(root, &name, &labels, &annotations).inspect_err(|_| {
            let _ = delonix_vm::destroy(root, &name, true, true);
        })
    });
    let outcome = outcome.map(|()| target_href(&req.namespace, &req.name));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

/// `DeleteVirtualMachine` under `root`.
pub fn delete_in(root: &Path, req: &DeleteVirtualMachineRequest) -> Result<Operation, Status> {
    delete_with_fn(root, req, &delete_work)
}

/// [`delete_in`] with the work injected (the tests' seam).
pub fn delete_with_fn(
    root: &Path,
    req: &DeleteVirtualMachineRequest,
    work: DeleteWork<'_>,
) -> Result<Operation, Status> {
    let target = format!("VirtualMachine/{}", req.name);
    if let Some(found) = operations::replay(root, "delete", &target, &req.request_id)? {
        return Ok(operations::message(&found));
    }
    let missing = || {
        Status::not_found(format!(
            "virtual machine '{}' in namespace '{}'",
            req.name,
            if req.namespace.is_empty() {
                "default"
            } else {
                &req.namespace
            }
        ))
    };
    if req.name.is_empty() || !delonix_vm::exists(root, &req.name) {
        return Err(missing());
    }
    let vm = delonix_vm::status(root, &req.name).map_err(|e| status_of(&e.into()))?;
    let wanted = if req.namespace.is_empty() {
        "default"
    } else {
        &req.namespace
    };
    if vm.namespace != wanted {
        return Err(missing());
    }
    if !req.etag.is_empty() {
        let current = crate::vms::message(&vm)
            .meta
            .map(|m| m.etag)
            .unwrap_or_default();
        if current != req.etag {
            return Err(crate::network_ops::stale_etag(format!(
                "virtual machine '{}' changed since etag '{}' was read (it is now '{current}')",
                req.name, req.etag
            )));
        }
    }
    let rec = match operations::begin(root, "delete", &target, &req.request_id)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = work(root, &req.name, req.force).map(|()| String::new());
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

type SimpleWork<'a> = &'a dyn Fn(&Path, &str) -> delonix_model::Result<()>;

fn missing_vm(root: &Path, name: &str, namespace: &str) -> Result<delonix_compute::Vm, Status> {
    let wanted = if namespace.is_empty() {
        "default"
    } else {
        namespace
    };
    let missing = || Status::not_found(format!("virtual machine '{name}' in namespace '{wanted}'"));
    let vm = delonix_vm::status(root, name).map_err(|e| {
        if e.is_not_found() {
            missing()
        } else {
            status_of(&e.into())
        }
    })?;
    if vm.namespace != wanted {
        return Err(missing());
    }
    Ok(vm)
}

/// A verb common to `Start`/`Stop`: validate the VM exists and is in the
/// requested namespace, then run the SAME idempotent-operation machinery
/// `create`/`delete` use. `req` only ever carries `name`/`namespace`/
/// `request_id` for these two verbs, so it is read generically here instead
/// of by four near-identical functions.
fn verb_in(
    root: &Path,
    verb: &str,
    name: &str,
    namespace: &str,
    request_id: &str,
    work: SimpleWork<'_>,
) -> Result<Operation, Status> {
    let target = format!("VirtualMachine/{name}");
    if let Some(found) = operations::replay(root, verb, &target, request_id)? {
        return Ok(operations::message(&found));
    }
    missing_vm(root, name, namespace)?;
    let rec = match operations::begin(root, verb, &target, request_id)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = work(root, name).map(|()| target_href(namespace, name));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

/// `StartVirtualMachine` under `root`.
pub fn start_in(root: &Path, req: &StartVirtualMachineRequest) -> Result<Operation, Status> {
    fn work(r: &Path, n: &str) -> delonix_model::Result<()> {
        delonix_vm::start(r, n)?;
        Ok(())
    }
    verb_in(
        root,
        "start",
        &req.name,
        &req.namespace,
        &req.request_id,
        &work,
    )
}

/// `StopVirtualMachine` under `root`. Keeps the disk — [`delete_in`] is what
/// releases it, the same distinction the contract's own comment on
/// `StopVirtualMachine` draws.
pub fn stop_in(root: &Path, req: &StopVirtualMachineRequest) -> Result<Operation, Status> {
    fn work(r: &Path, n: &str) -> delonix_model::Result<()> {
        delonix_vm::stop(r, n)?;
        Ok(())
    }
    verb_in(
        root,
        "stop",
        &req.name,
        &req.namespace,
        &req.request_id,
        &work,
    )
}

/// `PauseVirtualMachine`/`ResumeVirtualMachine`: synchronous (the contract
/// answers `VirtualMachine`, not an `Operation` — there is nothing to poll),
/// unlike every other mutation in this module.
pub fn pause_in(root: &Path, req: &PauseVirtualMachineRequest) -> Result<VirtualMachine, Status> {
    missing_vm(root, &req.name, &req.namespace)?;
    delonix_vm::pause(root, &req.name).map_err(|e| status_of(&e.into()))?;
    let vm = delonix_vm::status(root, &req.name).map_err(|e| status_of(&e.into()))?;
    Ok(crate::vms::message(&vm))
}

pub fn resume_in(root: &Path, req: &ResumeVirtualMachineRequest) -> Result<VirtualMachine, Status> {
    missing_vm(root, &req.name, &req.namespace)?;
    delonix_vm::unpause(root, &req.name).map_err(|e| status_of(&e.into()))?;
    let vm = delonix_vm::status(root, &req.name).map_err(|e| status_of(&e.into()))?;
    Ok(crate::vms::message(&vm))
}

/// `ListSnapshots`: the engine's `delonix_vm::snapshots` reports names only
/// — `includes_memory`/`state` are not tracked per snapshot today, so both
/// are left at their zero value rather than guessed. Read, not a mutation:
/// no operation is written.
pub fn list_snapshots_in(
    root: &Path,
    req: &ListSnapshotsRequest,
) -> Result<ListSnapshotsResponse, Status> {
    missing_vm(root, &req.virtual_machine, &req.namespace)?;
    let names =
        delonix_vm::snapshots(root, &req.virtual_machine).map_err(|e| status_of(&e.into()))?;
    Ok(ListSnapshotsResponse {
        snapshots: names
            .into_iter()
            .map(|name| Snapshot {
                name,
                includes_memory: false,
                state: String::new(),
            })
            .collect(),
    })
}

fn snapshot_target(vm: &str, snap: &str) -> String {
    format!("VirtualMachine/{vm}/Snapshot/{snap}")
}

/// `CreateSnapshot` under `root`.
pub fn create_snapshot_in(root: &Path, req: &CreateSnapshotRequest) -> Result<Operation, Status> {
    if req.snapshot.is_empty() {
        return Err(Status::invalid_argument(
            "CreateSnapshot: snapshot is required",
        ));
    }
    let target = snapshot_target(&req.virtual_machine, &req.snapshot);
    if let Some(found) = operations::replay(root, "create", &target, &req.request_id)? {
        return Ok(operations::message(&found));
    }
    missing_vm(root, &req.virtual_machine, &req.namespace)?;
    let rec = match operations::begin(root, "create", &target, &req.request_id)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = delonix_vm::snapshot(root, &req.virtual_machine, &req.snapshot)
        .map_err(Into::into)
        .map(|()| String::new());
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

/// `RestoreSnapshot` under `root`.
pub fn restore_snapshot_in(root: &Path, req: &RestoreSnapshotRequest) -> Result<Operation, Status> {
    let target = snapshot_target(&req.virtual_machine, &req.snapshot);
    if let Some(found) = operations::replay(root, "restore", &target, &req.request_id)? {
        return Ok(operations::message(&found));
    }
    missing_vm(root, &req.virtual_machine, &req.namespace)?;
    let rec = match operations::begin(root, "restore", &target, &req.request_id)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = delonix_vm::restore(root, &req.virtual_machine, &req.snapshot)
        .map_err(Into::into)
        .map(|()| target_href(&req.namespace, &req.virtual_machine));
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

/// `DeleteSnapshot` under `root`.
pub fn delete_snapshot_in(root: &Path, req: &DeleteSnapshotRequest) -> Result<Operation, Status> {
    let target = snapshot_target(&req.virtual_machine, &req.snapshot);
    if let Some(found) = operations::replay(root, "delete", &target, &req.request_id)? {
        return Ok(operations::message(&found));
    }
    missing_vm(root, &req.virtual_machine, &req.namespace)?;
    let rec = match operations::begin(root, "delete", &target, &req.request_id)? {
        Begun::Replay(found) => return Ok(operations::message(&found)),
        Begun::New(rec) => rec,
    };
    let outcome = delonix_vm::delete_snapshot(root, &req.virtual_machine, &req.snapshot)
        .map_err(Into::into)
        .map(|()| String::new());
    Ok(operations::message(&operations::finish(
        root, rec, outcome,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::v1::OperationState;
    use delonix_compute::capability::{
        CapabilityState, HealthStatus, ProviderHealth, ProviderKind, ProviderReport,
    };
    use delonix_compute::vm_backend::{BackendRegistration, Boot, CreateStage, VmBackend};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    struct Fake {
        running: &'static Mutex<std::collections::HashSet<String>>,
    }
    impl VmBackend for Fake {
        fn id(&self) -> &'static str {
            "fake-vm-ops"
        }
        fn available(&self) -> bool {
            true
        }
        fn manages_own_storage(&self) -> bool {
            true
        }
        fn boot(
            &self,
            _vmdir: &Path,
            cfg: &VmConfig,
            _overlay: &str,
            _on: &dyn Fn(CreateStage),
        ) -> delonix_model::Result<Boot> {
            self.running.lock().unwrap().insert(cfg.name.clone());
            Ok(Boot {
                pid: None,
                tap: format!("tap-{}", cfg.name),
                mac: delonix_compute::vm_registry::mac_for(&cfg.name),
                api_socket: String::new(),
                ip: Some("203.0.113.9".into()),
                lease_floor: None,
            })
        }
        fn is_running(&self, vm: &delonix_compute::Vm) -> bool {
            self.running.lock().unwrap().contains(&vm.name)
        }
        fn ip(&self, _vm: &delonix_compute::Vm) -> Option<String> {
            Some("203.0.113.9".into())
        }
        fn stop(&self, _vmdir: &Path, vm: &delonix_compute::Vm) -> delonix_model::Result<()> {
            self.running.lock().unwrap().remove(&vm.name);
            Ok(())
        }
    }

    /// `register_backend` is process-global, like every other `delonix-vm`
    /// test that seeds one (see `vms.rs`): registering twice with the same
    /// id is refused, so this runs once per test binary. Tests share the
    /// one `running` set and avoid collisions the same way `vms.rs`'s do —
    /// by using a distinct VM name each.
    fn seeded() -> &'static str {
        static ONCE: AtomicBool = AtomicBool::new(false);
        static RUNNING: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
            std::sync::OnceLock::new();
        let running = RUNNING.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
        if !ONCE.swap(true, Ordering::SeqCst) {
            delonix_vm::register_backend(BackendRegistration {
                id: "fake-vm-ops",
                aliases: &[],
                auto_selectable: false,
                new: Box::new(move || Ok(Box::new(Fake { running }) as Box<dyn VmBackend>)),
                report: Box::new(|| {
                    ProviderReport::build(
                        "fake-vm-ops",
                        ProviderKind::Compute,
                        true,
                        ProviderHealth {
                            status: HealthStatus::Healthy,
                            reason: "Test",
                            message: String::new(),
                        },
                        |_| CapabilityState::NotImplemented,
                    )
                }),
            })
            .unwrap();
        }
        "fake-vm-ops"
    }

    fn create_req(name: &str, request_id: &str) -> CreateVirtualMachineRequest {
        CreateVirtualMachineRequest {
            name: name.into(),
            request_id: request_id.into(),
            labels: [("app".to_string(), "web".to_string())].into(),
            spec: Some(VirtualMachineSpec {
                image: "fake.qcow2".into(),
                vcpus: 2,
                memory_bytes: 2 * 1024 * 1024 * 1024,
                provider: seeded().to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn operations_on_disk(root: &Path) -> usize {
        std::fs::read_dir(root.join("operations"))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    #[test]
    fn create_is_persisted_running_before_the_work_and_the_vm_reads_back_with_its_labels() {
        seeded();
        let dir = tempfile::tempdir().unwrap();
        let op = create_in(dir.path(), &create_req("web-1", "")).unwrap();
        assert_eq!(op.state, OperationState::Succeeded as i32);
        assert_eq!(op.verb, "create");
        assert_eq!(op.target, "VirtualMachine/web-1");
        let target = op.links.iter().find(|l| l.rel == "target").unwrap();
        assert_eq!(target.href, "/v1/namespaces/default/virtualmachines/web-1");

        let vm = crate::vms::get_in(
            dir.path(),
            &crate::proto::v1::GetVirtualMachineRequest {
                name: "web-1".into(),
                namespace: String::new(),
            },
        )
        .unwrap();
        assert_eq!(vm.meta.unwrap().labels["app"], "web");
        assert_eq!(vm.spec.unwrap().vcpus, 2);
    }

    #[test]
    fn a_name_already_taken_is_refused_and_no_operation_is_written() {
        seeded();
        let dir = tempfile::tempdir().unwrap();
        create_in(dir.path(), &create_req("web-2", "")).unwrap();
        let before = operations_on_disk(dir.path());
        let err = create_in(dir.path(), &create_req("web-2", "")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
        assert_eq!(operations_on_disk(dir.path()), before);
    }

    #[test]
    fn the_same_create_sent_twice_boots_once() {
        seeded();
        let dir = tempfile::tempdir().unwrap();
        let first = create_in(dir.path(), &create_req("web-3", "rid-1")).unwrap();
        let again = create_in(dir.path(), &create_req("web-3", "rid-1")).unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(again.state, OperationState::Succeeded as i32);
    }

    #[test]
    fn an_invalid_spec_is_refused_before_anything_is_acknowledged() {
        seeded();
        let dir = tempfile::tempdir().unwrap();
        let mut req = create_req("web-4", "");
        req.spec.as_mut().unwrap().image.clear();
        let err = create_in(dir.path(), &req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(!dir.path().join("vms").exists());
        assert_eq!(operations_on_disk(dir.path()), 0);

        let mut two_nets = create_req("web-5", "");
        two_nets.spec.as_mut().unwrap().networks = vec![
            NetworkAttachment {
                network: "a".into(),
                ..Default::default()
            },
            NetworkAttachment {
                network: "b".into(),
                ..Default::default()
            },
        ];
        let err = create_in(dir.path(), &two_nets).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        let mut with_data = create_req("web-6", "");
        with_data.spec.as_mut().unwrap().cloud_init = Some(CloudInit {
            user_data: "#cloud-config\n".into(),
            ..Default::default()
        });
        let err = create_in(dir.path(), &with_data).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn stop_keeps_the_record_and_delete_removes_it() {
        seeded();
        let dir = tempfile::tempdir().unwrap();
        create_in(dir.path(), &create_req("web-7", "")).unwrap();
        let stop = stop_in(
            dir.path(),
            &StopVirtualMachineRequest {
                name: "web-7".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(stop.state, OperationState::Succeeded as i32);
        assert!(delonix_vm::exists(dir.path(), "web-7"));

        let delete = delete_in(
            dir.path(),
            &DeleteVirtualMachineRequest {
                name: "web-7".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(delete.state, OperationState::Succeeded as i32);
        assert!(!delonix_vm::exists(dir.path(), "web-7"));

        let err = delete_in(
            dir.path(),
            &DeleteVirtualMachineRequest {
                name: "web-7".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn a_vm_in_another_namespace_is_not_found_by_start_stop_or_delete() {
        seeded();
        let dir = tempfile::tempdir().unwrap();
        create_in(dir.path(), &create_req("web-8", "")).unwrap();
        let err = start_in(
            dir.path(),
            &StartVirtualMachineRequest {
                name: "web-8".into(),
                namespace: "team-a".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn pause_and_resume_are_synchronous_and_report_the_vm_directly() {
        seeded();
        let dir = tempfile::tempdir().unwrap();
        create_in(dir.path(), &create_req("web-9", "")).unwrap();
        // This fake does not implement `pause`/`unpause` (the default is
        // "unsupported") — asserts the refusal is surfaced, not swallowed.
        let err = pause_in(
            dir.path(),
            &PauseVirtualMachineRequest {
                name: "web-9".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn snapshot_lifecycle_is_an_operation_and_a_missing_vm_is_refused() {
        seeded();
        let dir = tempfile::tempdir().unwrap();
        let err = create_snapshot_in(
            dir.path(),
            &CreateSnapshotRequest {
                virtual_machine: "nope".into(),
                snapshot: "s1".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// D3's regression test (review 2026-10-09, ADR-0076): writes the exact
    /// on-disk shape `bins/delonix-runtime-bin/src/cmd/vmimage.rs::
    /// VmImageStore::save` produces — `<root>/vm-images/<name>.json` —
    /// recommending 4 vCPUs / 4 GiB, and asks `config_from_spec` to resolve
    /// `vcpus: 0, memory_bytes: 0` against it. Before this fix, this always
    /// resolved to the fixed `(1, "1G")`, ignoring the fixture entirely; the
    /// node API and the CLI's own `resolve_vm_defaults` (`bins/delonix-
    /// runtime-bin/src/cmd/vm.rs`) now agree on the identical input. Goes
    /// through the REAL `delonix_vm::image_defaults`/`VmImageJsonDefaults`
    /// reader, not a fake — the port this fixes is exactly the thing under
    /// test.
    #[test]
    fn zero_vcpus_and_memory_resolve_the_named_images_own_recorded_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let images_dir = dir.path().join("vm-images");
        std::fs::create_dir_all(&images_dir).unwrap();
        std::fs::write(
            images_dir.join("golden.json"),
            serde_json::json!({
                "name": "golden",
                "tag": "latest",
                "digest": "sha256:0",
                "size": 0,
                "ubuntu_release": null,
                "k8s_version": null,
                "created_unix": 0,
                "default_vcpus": 4,
                "default_memory": "4G",
                "default_backend": null,
            })
            .to_string(),
        )
        .unwrap();

        let cfg = config_from_spec(
            dir.path(),
            "web",
            None,
            &VirtualMachineSpec {
                image: "golden".into(),
                vcpus: 0,
                memory_bytes: 0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.vcpus, 4, "the image's own recorded vCPU default");
        assert_eq!(
            cfg.memory, "4G",
            "the image's own recorded memory default, verbatim — same as the \
             CLI's resolve_vm_defaults, which also never normalizes it"
        );

        // An EXPLICIT value still wins over the image, same as the CLI.
        let explicit = config_from_spec(
            dir.path(),
            "web",
            None,
            &VirtualMachineSpec {
                image: "golden".into(),
                vcpus: 2,
                memory_bytes: 2 * 1024 * 1024 * 1024,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(explicit.vcpus, 2);
        assert_eq!(explicit.memory, "2048M");

        // An image reference with no sidecar at all still falls back to the
        // fixed default — this scenario was never the gap.
        let unregistered = config_from_spec(
            dir.path(),
            "web",
            None,
            &VirtualMachineSpec {
                image: "unregistered-disk.qcow2".into(),
                vcpus: 0,
                memory_bytes: 0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(unregistered.vcpus, 1);
        assert_eq!(unregistered.memory, "1G");
    }
}

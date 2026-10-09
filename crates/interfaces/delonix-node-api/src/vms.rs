//! `VirtualMachineService`, the read half: `GetVirtualMachine` and
//! `ListVirtualMachines` (ADR-0040 P5, closing the VMaaS audit's §4 gap —
//! `docs/discovery/67_VMAAS_READINESS_AUDIT.md`).
//!
//! A VM's name is unique at the NODE level (`delonix_vm::list`/`status` read
//! one flat store, never one per isolation namespace — a `kind: Vm` with the
//! same name in two namespaces is a `Conflict`, not two resources). So a read
//! here is: find the record by name anywhere, then require its recorded
//! `namespace` to match what was asked — exactly how a Container's isolation
//! namespace is read, and the opposite of how `Network`'s single shared
//! namespace is read in [`crate::networks`].
//!
//! What a VM IS and whether it is alive come from the SAME call —
//! `delonix_vm::status`/`list` already reconcile the record against the
//! backend before returning it, so there is no second "is it really running"
//! question for this module to ask.

use std::path::Path;

use tonic::Status;

use crate::page::fnv;
pub use crate::page::{DEFAULT_PAGE, MAX_PAGE};
use crate::proto::v1::{
    CloudInit, Condition, ConditionStatus, GetVirtualMachineRequest, ListVirtualMachinesRequest,
    ListVirtualMachinesResponse, NetworkAttachment, PageResponse, ProviderExtensions, ResourceMeta,
    RestartMode, RestartPolicy, VirtualMachine, VirtualMachinePhase, VirtualMachineSpec,
    VirtualMachineStatus,
};
use crate::selector::Selector;

/// Whether `namespace` is one a VM can be reported under: any explicit name
/// (checked against the record itself below), the empty string (→ `default`),
/// or `*` (every namespace).
fn requested_namespace(namespace: &str) -> &str {
    if namespace.is_empty() {
        "default"
    } else {
        namespace
    }
}

fn text(s: &str) -> pbjson_types::Value {
    pbjson_types::Value {
        kind: Some(pbjson_types::value::Kind::StringValue(s.to_string())),
    }
}

fn timestamp_secs(secs: u64) -> pbjson_types::Timestamp {
    pbjson_types::Timestamp {
        seconds: secs as i64,
        nanos: 0,
    }
}

/// All records under `root`. A root where no VM was ever created has no
/// `vms/` directory — that is an empty list, and reading must not create it
/// (the same convention [`crate::networks::records`] follows).
fn records(root: &Path) -> Result<Vec<delonix_compute::Vm>, Status> {
    if !root.join("vms").is_dir() {
        return Ok(Vec::new());
    }
    let mut all = delonix_vm::list(root).map_err(|e| crate::network_ops::status_of(&e.into()))?;
    all.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(all)
}

/// `restart_policy` (the engine's own string convention) as the contract's
/// enum. An unrecognized string (a record from a future build, or hand-edited
/// state) reports `UNSPECIFIED` rather than fail the whole read.
fn restart_mode_of(policy: Option<&str>) -> i32 {
    (match policy {
        Some("on-failure") => RestartMode::OnFailure,
        Some("always") => RestartMode::Always,
        Some("unless-stopped") => RestartMode::UnlessStopped,
        Some("no") | None => RestartMode::No,
        Some(_) => RestartMode::Unspecified,
    }) as i32
}

/// A reconciled record as the contract's `VirtualMachine`. `ip_predicted`
/// came out of this very audit (§3/§7 item 1 of
/// `docs/discovery/67_VMAAS_READINESS_AUDIT.md`): a Cloud Hypervisor address
/// is computed from the MAC before the guest ever runs, so "has an address"
/// is only "is up" when this is `false` — reported here instead of letting a
/// caller rediscover it.
pub fn message(vm: &delonix_compute::Vm) -> VirtualMachine {
    let predicted = delonix_vm::ip_is_predicted(vm);
    let phase = match &vm.status {
        delonix_model::records::Status::Created => VirtualMachinePhase::Provisioning,
        delonix_model::records::Status::Running => VirtualMachinePhase::Running,
        delonix_model::records::Status::Paused => VirtualMachinePhase::Paused,
        delonix_model::records::Status::Stopped => VirtualMachinePhase::Stopped,
        delonix_model::records::Status::Failed(_) | delonix_model::records::Status::Crashed => {
            VirtualMachinePhase::Failed
        }
    };
    let mut conditions = vec![match phase {
        VirtualMachinePhase::Running => Condition {
            r#type: "Ready".into(),
            status: ConditionStatus::True as i32,
            reason: "Running".into(),
            message: "the backend reports the guest running".into(),
            last_transition_time: None,
        },
        _ => Condition {
            r#type: "Ready".into(),
            status: ConditionStatus::False as i32,
            reason: format!("{phase:?}"),
            message: format!("the guest is not running ({})", vm.status),
            last_transition_time: None,
        },
    }];
    if let Some(ip) = vm.ip.as_deref().filter(|s| !s.is_empty()) {
        conditions.push(if predicted {
            Condition {
                r#type: "AddressReachable".into(),
                status: ConditionStatus::Unknown as i32,
                reason: "AddressPredicted".into(),
                message: format!(
                    "{ip} is computed from the MAC, not observed from the guest — it exists \
                     whether or not the guest answers on it"
                ),
                last_transition_time: None,
            }
        } else {
            Condition {
                r#type: "AddressReachable".into(),
                status: ConditionStatus::True as i32,
                reason: "AddressObserved".into(),
                message: format!("{ip} was observed from the guest (a real DHCP lease)"),
                last_transition_time: None,
            }
        });
    }
    let mut by_provider = std::collections::HashMap::new();
    if !vm.tap.is_empty() || !vm.mac.is_empty() {
        by_provider.insert(
            "linux".to_string(),
            pbjson_types::Struct {
                fields: std::collections::HashMap::from([
                    ("tap".to_string(), text(&vm.tap)),
                    ("mac".to_string(), text(&vm.mac)),
                ]),
            },
        );
    }
    VirtualMachine {
        meta: Some(ResourceMeta {
            name: vm.name.clone(),
            namespace: vm.namespace.clone(),
            labels: vm.labels.clone().into_iter().collect(),
            annotations: vm.annotations.clone().into_iter().collect(),
            etag: fnv(&format!("{vm:?}")),
            create_time: Some(timestamp_secs(vm.created_unix)),
            ..Default::default()
        }),
        spec: Some(VirtualMachineSpec {
            image: vm.disk.clone(),
            vcpus: vm.vcpus as i32,
            // Measuring the overlay's logical size needs `qemu-img info`, not
            // a `stat()` (the file is sparse) — not done in this pass; 0 here
            // means "not measured", the same meaning the proto gives create.
            memory_bytes: (delonix_compute::vm_backend::mem_mib(&vm.memory) as i64) * 1024 * 1024,
            disk_bytes: 0,
            networks: vec![NetworkAttachment {
                network: vm.network.clone(),
                ipv4_address: vm.ip.clone().unwrap_or_default(),
                aliases: vec![],
            }],
            cloud_init: None,
            provider: vm.backend.clone(),
            required_capabilities: vec![],
            extensions: (!by_provider.is_empty()).then_some(ProviderExtensions { by_provider }),
            restart: restart_mode_of(vm.restart_policy.as_deref()),
        }),
        status: Some(VirtualMachineStatus {
            phase: phase as i32,
            provider: vm.backend.clone(),
            ip_addresses: vm.ip.clone().into_iter().collect(),
            ip_predicted: predicted,
            conditions,
        }),
    }
}

/// `GetVirtualMachine` under `root`.
pub fn get_in(root: &Path, req: &GetVirtualMachineRequest) -> Result<VirtualMachine, Status> {
    let wanted = requested_namespace(&req.namespace);
    let missing = || {
        Status::not_found(format!(
            "virtual machine '{}' in namespace '{wanted}'",
            req.name
        ))
    };
    if wanted == "*" {
        return Err(missing());
    }
    let vm = delonix_vm::status(root, &req.name).map_err(|e| {
        if e.is_not_found() {
            missing()
        } else {
            crate::network_ops::status_of(&e.into())
        }
    })?;
    if vm.namespace != wanted {
        return Err(missing());
    }
    Ok(message(&vm))
}

/// `ListVirtualMachines` under `root`: filtered by `label_selector` and the
/// requested namespace, ordered by name, one page at a time.
pub fn list_in(
    root: &Path,
    req: &ListVirtualMachinesRequest,
) -> Result<ListVirtualMachinesResponse, Status> {
    let selector = Selector::parse(&req.label_selector).map_err(Status::invalid_argument)?;
    let page = req.page.clone().unwrap_or_default();
    let size = crate::page::size(&page)?;
    let after = crate::page::after(&page)?;
    let ns = requested_namespace(&req.namespace);
    let all = records(root)?;
    let mut matching = all
        .iter()
        .filter(|v| ns == "*" || v.namespace == ns)
        .filter(|v| selector.matches(&v.labels))
        .filter(|v| after.as_ref().is_none_or(|a| v.name.as_str() > a.as_str()));
    let virtual_machines: Vec<VirtualMachine> = matching.by_ref().take(size).map(message).collect();
    let next = match (matching.next(), virtual_machines.last()) {
        (Some(_), Some(last)) => crate::page::encode_token(
            last.meta
                .as_ref()
                .map(|m| m.name.as_str())
                .unwrap_or_default(),
        ),
        _ => String::new(),
    };
    Ok(ListVirtualMachinesResponse {
        virtual_machines,
        page: Some(PageResponse {
            next_page_token: next,
        }),
    })
}

/// `RestartPolicy` is the contract's companion message for a container; a VM
/// only reports the mode (no `max_retries` — this engine's VM supervision is
/// per-backend, not a retry counter). Kept here so a future caller that wants
/// the shared message does not have to guess the mapping again.
#[allow(dead_code)]
fn as_restart_policy(policy: Option<&str>) -> RestartPolicy {
    RestartPolicy {
        mode: restart_mode_of(policy),
        max_retries: 0,
    }
}

/// Not reported on create: the engine resolves intent from `hostname`/
/// `ssh_authorized_keys`, never echoes `user_data` back (it is not persisted
/// on the record at all — see [`crate::vm_ops`]).
#[allow(dead_code)]
fn unused_cloud_init_marker() -> Option<CloudInit> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::v1::PageRequest;
    use delonix_compute::vm_backend::VmConfig;

    /// A state root with three VMs, two in `default`, one in `team-a`, built
    /// through the real `delonix_vm::create` (a fake backend, registered once
    /// per test process) so the reconciled read is exercised, not a record
    /// written by hand that `status()` would never produce on its own.
    fn root() -> (tempfile::TempDir, &'static str) {
        use delonix_compute::capability::{
            CapabilityState, HealthStatus, ProviderHealth, ProviderKind, ProviderReport,
        };
        use delonix_compute::vm_backend::{BackendRegistration, Boot, CreateStage, VmBackend};
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Fake;
        impl VmBackend for Fake {
            fn id(&self) -> &'static str {
                "fake-vms-read"
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
                Ok(Boot {
                    pid: None,
                    tap: format!("tap-{}", cfg.name),
                    mac: delonix_compute::vm_registry::mac_for(&cfg.name),
                    api_socket: String::new(),
                    ip: Some("203.0.113.9".into()),
                    lease_floor: None,
                })
            }
            fn is_running(&self, _vm: &delonix_compute::Vm) -> bool {
                true
            }
            fn ip(&self, _vm: &delonix_compute::Vm) -> Option<String> {
                Some("203.0.113.9".into())
            }
            // Cloud Hypervisor-like, deliberately: exercises the exact
            // nuance this audit exists to surface — see `message`'s
            // `AddressReachable` condition below.
            fn ip_is_predicted(&self) -> bool {
                true
            }
            fn stop(&self, _vmdir: &Path, _vm: &delonix_compute::Vm) -> delonix_model::Result<()> {
                Ok(())
            }
        }
        static ONCE: AtomicBool = AtomicBool::new(false);
        if !ONCE.swap(true, Ordering::SeqCst) {
            delonix_vm::register_backend(BackendRegistration {
                id: "fake-vms-read",
                aliases: &[],
                auto_selectable: false,
                new: Box::new(|| Ok(Box::new(Fake))),
                report: Box::new(|| {
                    ProviderReport::build(
                        "fake-vms-read",
                        ProviderKind::Compute,
                        true,
                        ProviderHealth {
                            status: HealthStatus::Healthy,
                            reason: "Test",
                            message: String::new(),
                        },
                        |c| {
                            if c == delonix_compute::capability::Capability::VmNamespaceIsolation {
                                CapabilityState::Supported {
                                    evidence: "test:fixture",
                                }
                            } else {
                                CapabilityState::NotImplemented
                            }
                        },
                    )
                }),
            })
            .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        for (name, ns) in [("web-a", "default"), ("web-b", "default"), ("db", "team-a")] {
            delonix_vm::create(
                dir.path(),
                &VmConfig {
                    name: name.into(),
                    disk: "fake.qcow2".into(),
                    vcpus: 2,
                    memory: "2G".into(),
                    network: "ingress".into(),
                    namespace: (ns != "default").then(|| ns.to_string()),
                    backend: Some("fake-vms-read".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        (dir, "fake-vms-read")
    }

    #[test]
    fn a_vm_is_reported_reconciled_with_its_namespace_and_predicted_address() {
        let (dir, _backend) = root();
        let vm = get_in(
            dir.path(),
            &GetVirtualMachineRequest {
                name: "web-a".into(),
                namespace: String::new(),
            },
        )
        .unwrap();
        let meta = vm.meta.unwrap();
        assert_eq!(
            (meta.name.as_str(), meta.namespace.as_str()),
            ("web-a", "default")
        );
        let status = vm.status.unwrap();
        assert_eq!(status.phase, VirtualMachinePhase::Running as i32);
        assert_eq!(status.ip_addresses, vec!["203.0.113.9".to_string()]);
        // This fake backend opts into "predicted" (like Cloud Hypervisor) —
        // the exact distinction this audit exists to surface, exercised here
        // rather than assumed away by a test fixture that never says either
        // way (the trait's own default is "observed", libvirt-like).
        assert!(status.ip_predicted);
        assert_eq!(
            status
                .conditions
                .iter()
                .find(|c| c.r#type == "AddressReachable")
                .unwrap()
                .reason,
            "AddressPredicted"
        );

        let other_ns = get_in(
            dir.path(),
            &GetVirtualMachineRequest {
                name: "web-a".into(),
                namespace: "team-a".into(),
            },
        )
        .unwrap_err();
        assert_eq!(other_ns.code(), tonic::Code::NotFound);

        let db = get_in(
            dir.path(),
            &GetVirtualMachineRequest {
                name: "db".into(),
                namespace: "team-a".into(),
            },
        )
        .unwrap();
        assert_eq!(db.meta.unwrap().namespace, "team-a");
    }

    #[test]
    fn list_filters_by_namespace_and_label_and_pages_by_name() {
        let (dir, _backend) = root();
        let default_ns = list_in(
            dir.path(),
            &ListVirtualMachinesRequest {
                namespace: String::new(),
                label_selector: String::new(),
                page: None,
            },
        )
        .unwrap();
        assert_eq!(
            default_ns
                .virtual_machines
                .iter()
                .map(|v| v.meta.as_ref().unwrap().name.clone())
                .collect::<Vec<_>>(),
            vec!["web-a".to_string(), "web-b".to_string()]
        );

        let all = list_in(
            dir.path(),
            &ListVirtualMachinesRequest {
                namespace: "*".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.virtual_machines.len(), 3);

        let first = list_in(
            dir.path(),
            &ListVirtualMachinesRequest {
                namespace: "*".into(),
                page: Some(PageRequest {
                    page_size: 2,
                    page_token: String::new(),
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(first.virtual_machines.len(), 2);
        let token = first.page.unwrap().next_page_token;
        assert!(!token.is_empty());
        let second = list_in(
            dir.path(),
            &ListVirtualMachinesRequest {
                namespace: "*".into(),
                page: Some(PageRequest {
                    page_size: 2,
                    page_token: token,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(second.virtual_machines.len(), 1);

        // A root where nothing was ever created: empty, and still not created.
        let empty = tempfile::tempdir().unwrap();
        assert!(
            list_in(empty.path(), &ListVirtualMachinesRequest::default())
                .unwrap()
                .virtual_machines
                .is_empty()
        );
        assert!(!empty.path().join("vms").exists());
    }
}

//! The VM provider port (ADR-0044 D1–D4, landed by P4b slice 1): what every VM
//! provider receives (`VmSpec` + `Extensions`), what it is (`Provider`: an id and
//! a capability report), and the lifecycle it answers (`VmProvider`).
//!
//! This is the port the provider crates implement and the compute use cases
//! call. It lives in the compute context per ADR-0040 D2.2; the two local
//! implementations (Cloud Hypervisor, libvirt) are still in `delonix-vm` until
//! the crate split of P4b slice 2, and Proxmox joins in P4c.
//!
//! Three things the shape decides, each measured before being written:
//!
//! - **`VmSpec` holds only what every provider can honour**, already resolved
//!   (`memory_mib`, never a `"2G"` string a provider parses a second time —
//!   ADR-0008's own finding). What one provider needs and another cannot use
//!   goes into `Extensions`, keyed by provider, typed, and closed: a provider
//!   reads its own key and nothing else, and the field a provider cannot
//!   receive is the field it cannot silently ignore (the `network`/`namespace`
//!   leak ADR-0044's Context measured on `VmConfig`).
//! - **`capabilities()` is ADR-0050's `ProviderReport`**, not a new set type:
//!   the catalog, the six states and the host probe already exist, and a
//!   second vocabulary here would be the drift the catalog was built to end.
//! - **Every method takes the state root**, as the substitution spike
//!   (`docs/discovery/59_P4_D1_D3_SUBSTITUTION_SPIKE.md`) had to: today the
//!   root is a per-call argument all the way down (`create_with(base, …)`),
//!   and a handle that carried it would be a second place for it to be wrong.
//!   When the compute use cases own a `StateRepository<Vm>` (D6), the root
//!   leaves these signatures with them.
//!
//! What a provider cannot do is asked, never guessed: pause, snapshot,
//! resume and the rest are catalog entries a caller checks on the report and
//! the provider refuses by name — never a default method that quietly does
//! nothing (ADR-0044 D3).

use crate::capability::{ProviderHealth, ProviderReport};
use crate::{CpuTopology, ExtraDisk, ExtraNic, VmVolume};
use delonix_model::Result;
use std::path::Path;

/// Everything a `VmProvider` needs regardless of which one it is (ADR-0044
/// D1). Built at the call site from whichever entry point is starting a VM;
/// never persisted on its own — `Vm`/`VmBootSpec` keep doing that job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VmSpec {
    pub name: String,
    /// Opaque to the spec, by decision: a local provider reads a filesystem
    /// path, a remote one its own addressing form (`template:<id>`,
    /// `<storage>:<gib>`), and each refuses what it does not recognise. A
    /// typed `DiskSource` is a named follow-on, not this port's job.
    pub disk: String,
    pub vcpus: u32,
    /// Already resolved: parsed once, at the call site, with the engine's one
    /// `mem_mib`, so no provider re-implements the suffix table.
    pub memory_mib: u32,
    pub disk_size_gib: Option<u32>,
    /// The engine network the VM attaches to. Universal by design: a provider
    /// that cannot put a VM on an engine network refuses a name it cannot
    /// honour, the way Proxmox does today — it never accepts and ignores.
    pub network: String,
    /// The isolation namespace; `"default"` is the open SDN, as everywhere
    /// else in the engine. Same rule as `network`: honoured or refused.
    pub namespace: String,
    /// The host bridge for a provider whose VMs live on one (libvirt `bridge`
    /// mode, Proxmox `vmbr*`). Read by a remote provider, hence universal —
    /// ADR-0044 D1 corrected `VmBootSpec`'s "local-only" reading of it.
    pub bridge: Option<String>,
    pub hostname: Option<String>,
    pub ci_user: Option<String>,
    pub ssh_keys: Vec<String>,
    /// `Some(false)` for an appliance image that runs no cloud-init: the
    /// provider then seeds nothing and refuses the intent fields above.
    pub cloud_init: Option<bool>,
    pub restart_policy: Option<String>,
    /// Capture the console to a file instead of exposing it interactively —
    /// read identically by both local providers (spike correction, 2026-09-18).
    pub serial_capture: bool,
}

/// Per-provider knobs, namespaced and typed (ADR-0044 D2). A new provider adds
/// a field here and nowhere else in this crate; the provider that owns a key
/// is the only thing that reads or validates it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extensions {
    pub cloud_hypervisor: Option<CloudHypervisorExt>,
    pub libvirt: Option<LibvirtExt>,
}

/// What only Cloud Hypervisor reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CloudHypervisorExt {
    /// Direct kernel boot (vmlinux/bzImage), with `initrd`/`cmdline`.
    pub kernel: Option<String>,
    pub initrd: Option<String>,
    pub firmware: Option<String>,
    pub cmdline: Option<String>,
    /// A NoCloud seed the caller built; without it the provider builds one
    /// from the intent fields of `VmSpec`.
    pub seed: Option<String>,
    pub hugepages: bool,
    pub cpu_affinity: Option<String>,
    /// VFIO passthrough, as sysfs paths. Read identically by both local
    /// providers (spike correction, 2026-09-18) and refused by Proxmox, so it
    /// is neither universal nor one provider's: it appears on both local
    /// extensions rather than on `VmSpec`.
    pub devices: Vec<String>,
}

/// What only libvirt reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibvirtExt {
    /// `nat` | `bridge` | `user`; the provider picks when absent.
    pub net_mode: Option<String>,
    /// A DHCP reservation on the libvirt network (`nat` mode only).
    pub static_ip: Option<String>,
    /// Opts out of the anti-spoofing nwfilter on the primary NIC (ADR-0055).
    pub allow_mac_spoofing: bool,
    pub machine: Option<String>,
    pub cpu_model: Option<String>,
    pub cpu_topology: Option<CpuTopology>,
    pub tpm: bool,
    pub video: Option<String>,
    pub vnc: bool,
    pub boot_order: Vec<String>,
    pub extra_disks: Vec<ExtraDisk>,
    pub extra_nics: Vec<ExtraNic>,
    /// 9p mounts — libvirt only; Cloud Hypervisor never had them to refuse.
    pub volumes: Vec<VmVolume>,
    /// Raw `<device>` fragments — UNVALIDATED, trusted manifests only, the
    /// same trust model as running an arbitrary disk image (ADR-0050 D7: it
    /// must never travel over the node contract).
    pub libvirt_xml_overlay: Vec<String>,
    /// A whole `<domain>` — same trust tier as `libvirt_xml_overlay`.
    pub libvirt_xml: Option<String>,
    /// See `CloudHypervisorExt::devices`.
    pub devices: Vec<String>,
}

/// A provider's stable id — the value persisted in a VM record's `backend`
/// and the key of the registry. Fixed once, at construction; never re-derived
/// by comparing it against a literal inside a lifecycle method (ADR-0044 D3
/// rule 3: no provider-name matching outside a composition root).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderId(pub &'static str);

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// ADR-0040 D3's skeleton: identity, capabilities, health.
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;

    /// The provider's answer to every catalog entry, on THIS host when the
    /// provider is local (it probes) and as declared when it is remote (it
    /// never connects to answer this). ADR-0050's report, verbatim: a caller
    /// that needs `vm.snapshot.memory` finds it here and refuses before any
    /// effect when it is not usable — the provider never has to guess.
    fn capabilities(&self) -> ProviderReport;

    /// The report's health, for a caller that asks only "can I select you".
    fn health(&self) -> ProviderHealth {
        self.capabilities().health
    }
}

/// How much to trust `VmObservation::ip`. Was `VmBackend::ip_is_predicted()`;
/// a three-state enum because a caller has to tell "not running yet" from
/// "running, address unknown", and a `bool` collapsed both into `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpConfidence {
    /// A real lease or agent answer — evidence the guest asked for it.
    Observed,
    /// Computed from the MAC before the guest ran at all (Cloud Hypervisor's
    /// deterministic lease). `--wait` must go and look (ADR-0044 Context).
    Predicted,
    /// Not running, or running with nothing to report yet.
    Unknown,
}

/// What a provider hands back from `create`: enough to address the VM again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmHandle {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmObservation {
    pub running: bool,
    pub ip: Option<String>,
    pub ip_confidence: IpConfidence,
}

/// The VM lifecycle every provider answers (ADR-0044 D3).
///
/// `stop` keeps the disk; `destroy` releases it. The two are one operation on
/// a local provider and NOT on a remote one — ADR-0008 paid for conflating
/// them with a Proxmox disk deleted by `vm stop`. Everything else a provider
/// may or may not do (pause, snapshot, resume, disk health) is a capability
/// (`Provider::capabilities`) and a use case of its own, never a default here.
pub trait VmProvider: Provider {
    fn create(&self, root: &Path, spec: &VmSpec, ext: &Extensions) -> Result<VmHandle>;
    fn start(&self, root: &Path, h: &VmHandle) -> Result<()>;
    fn stop(&self, root: &Path, h: &VmHandle) -> Result<()>;
    fn destroy(&self, root: &Path, h: &VmHandle) -> Result<()>;
    fn observe(&self, root: &Path, h: &VmHandle) -> Result<VmObservation>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, CapabilityState, HealthStatus, ProviderKind};

    struct Declaring;

    impl Provider for Declaring {
        fn id(&self) -> ProviderId {
            ProviderId("declaring")
        }
        fn capabilities(&self) -> ProviderReport {
            ProviderReport::build(
                "declaring",
                ProviderKind::Compute,
                false,
                ProviderHealth {
                    status: HealthStatus::Unavailable,
                    reason: "TestOnly",
                    message: "never selectable".into(),
                },
                |c| match c {
                    Capability::VmCreate => CapabilityState::Partial { detail: "test" },
                    _ => CapabilityState::NotImplemented,
                },
            )
        }
    }

    /// `health()` is derived from the report, so the two can never disagree —
    /// a provider that overrides one without the other would be the bug.
    #[test]
    fn health_is_the_reports_health() {
        let p = Declaring;
        assert_eq!(p.health().reason, p.capabilities().health.reason);
        assert_eq!(p.health().status, HealthStatus::Unavailable);
        assert_eq!(p.id().to_string(), "declaring");
    }

    /// An empty `Extensions` is the honest default: nothing provider-specific
    /// was asked for, so nothing provider-specific can be silently applied.
    #[test]
    fn extensions_default_to_nothing() {
        let e = Extensions::default();
        assert!(e.cloud_hypervisor.is_none());
        assert!(e.libvirt.is_none());
        assert_eq!(VmSpec::default().namespace, "");
    }
}

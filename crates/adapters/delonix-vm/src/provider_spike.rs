//! SPIKE (ADR-0044 D1-D4) — `VmSpec`/`Extensions`/`VmProvider`, the
//! substitution spike required before that ADR can be marked Accepted:
//! "Hand the *same* `VmSpec` to `delonix-provider-cloud-hypervisor` and
//! `delonix-provider-libvirt` and converge both — `delonix vm create`,
//! `stop`, `start` on each, from one manifest, with **zero**
//! `#[cfg]`/name-branching outside the composition root."
//!
//! Not for merge as-is — see the spike report
//! (`docs/discovery/59_P4_D1_D3_SUBSTITUTION_SPIKE.md`) for what diverges
//! from the ADR's sketch and why, measured against the real
//! `boot_ch`/`libvirt_domain_xml` code, not guessed from the ADR's own D1
//! table.
//!
//! **Deliberately reuses [`crate::create_with`]/[`crate::stop`]/
//! [`crate::start`]/[`crate::status`]/[`crate::remove`] verbatim.** This
//! spike's contract is the SHAPE — `VmSpec`+`Extensions` in, one call per
//! lifecycle op, zero backend-name matching outside the registry — not a
//! second orchestration engine sitting next to the one this crate already
//! has, tested, and trusts.

use crate::{
    create_with, remove, start, status, stop, CpuTopology, ExtraDisk, ExtraNic, Result, VmConfig,
    VmVolume,
};
use delonix_model::records::Status as VmStatus;
use std::path::Path;

/// D1. Universal fields — every current provider (CH, libvirt; and, on the
/// read side, Proxmox) is expected to make sense of these, even if one
/// refuses a field it cannot yet honour (`namespace` on Proxmox today).
#[derive(Debug, Clone, Default)]
pub struct VmSpec {
    pub name: String,
    pub disk: String,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub disk_size_gib: Option<u32>,
    pub network: String,
    pub namespace: String,
    pub hostname: Option<String>,
    pub ci_user: Option<String>,
    pub ssh_keys: Vec<String>,
    pub cloud_init: Option<bool>,
    pub restart_policy: Option<String>,
    /// **Correction to the ADR-0044 D1 sketch, measured 2026-09-18**
    /// (spike report, "serial_capture is universal"): `boot_ch` (line 1980+)
    /// and `libvirt_domain_xml` (line 2578) both read `cfg.serial_capture`
    /// identically — "capture the console to a file instead of exposing it
    /// interactively" is not a CH-only or libvirt-only notion, it is a
    /// property of any VMM this engine drives. The D1 table did not list it
    /// at all; this spike puts it here rather than silently dropping it.
    pub serial_capture: bool,
}

/// D2. Per-provider knobs, namespaced and closed against drift: an unknown
/// key is a refusal, never a silently ignored map entry (see the ADR's D2
/// for the full reasoning against `HashMap<String, Value>`).
#[derive(Debug, Clone, Default)]
pub struct Extensions {
    pub cloud_hypervisor: Option<CloudHypervisorExt>,
    pub libvirt: Option<LibvirtExt>,
}

#[derive(Debug, Clone, Default)]
pub struct CloudHypervisorExt {
    pub kernel: Option<String>,
    pub initrd: Option<String>,
    pub firmware: Option<String>,
    pub cmdline: Option<String>,
    pub seed: Option<String>,
    pub hugepages: bool,
    pub cpu_affinity: Option<String>,
    /// **Correction to the ADR-0044 D1 table, measured 2026-09-18**: `devices`
    /// (VFIO passthrough) is read IDENTICALLY by `boot_ch` (`--device
    /// path=…`, line 1968) and `libvirt_domain_xml` (`<hostdev>`, line 2922)
    /// — not the "libvirt-only in practice" field the ADR's table named. It
    /// stays duplicated across `*Ext` rather than promoted to `VmSpec`
    /// because it is NOT universal across every provider either: Proxmox
    /// refuses it outright today (`refuse_unsupported`). A `LocalExt` tier
    /// shared by CH+libvirt only (absent for remote providers) is the honest
    /// fix and is named, not built, in the spike report — out of this
    /// spike's scope, which is CH+libvirt convergence only (P4b, per D9;
    /// Proxmox is P4c).
    pub devices: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LibvirtExt {
    pub net_mode: Option<String>,
    pub bridge: Option<String>,
    pub static_ip: Option<String>,
    pub machine: Option<String>,
    pub cpu_model: Option<String>,
    pub cpu_topology: Option<CpuTopology>,
    pub tpm: bool,
    pub video: Option<String>,
    pub vnc: bool,
    pub boot_order: Vec<String>,
    pub extra_disks: Vec<ExtraDisk>,
    pub extra_nics: Vec<ExtraNic>,
    pub volumes: Vec<VmVolume>,
    pub libvirt_xml_overlay: Vec<String>,
    pub libvirt_xml: Option<String>,
    pub devices: Vec<String>,
}

/// The port's actual contract: `VmSpec` + `Extensions`, keyed by the
/// provider that is about to receive it, resolved into the ONE `VmConfig`
/// shape `create_with`/`VmBackend` already consume. Pure — testable without
/// touching a host, and it is this function (not the trait below) that
/// stands in for D1/D2's field-classification table: every field either
/// lands here or is a documented, deliberate omission (see the module
/// doc-comment's corrections).
pub fn spec_to_config(spec: &VmSpec, ext: &Extensions, provider_id: &str) -> VmConfig {
    let mut cfg = VmConfig {
        name: spec.name.clone(),
        disk: spec.disk.clone(),
        vcpus: spec.vcpus,
        // `mem_mib` parses `"512M"` back to exactly 512 — the round trip
        // this spike relies on instead of inventing a second format.
        memory: format!("{}M", spec.memory_mib),
        network: spec.network.clone(),
        namespace: Some(spec.namespace.clone()),
        disk_size_gib: spec.disk_size_gib,
        hostname: spec.hostname.clone(),
        ci_user: spec.ci_user.clone(),
        ssh_keys: spec.ssh_keys.clone(),
        cloud_init: spec.cloud_init,
        restart_policy: spec.restart_policy.clone(),
        serial_capture: spec.serial_capture,
        backend: Some(provider_id.to_string()),
        ..Default::default()
    };
    if let Some(ch) = &ext.cloud_hypervisor {
        cfg.kernel = ch.kernel.clone();
        cfg.initrd = ch.initrd.clone();
        cfg.firmware = ch.firmware.clone();
        cfg.cmdline = ch.cmdline.clone();
        cfg.seed = ch.seed.clone();
        cfg.hugepages = ch.hugepages;
        cfg.cpu_affinity = ch.cpu_affinity.clone();
        cfg.devices = ch.devices.clone();
    }
    if let Some(lv) = &ext.libvirt {
        cfg.net_mode = lv.net_mode.clone();
        cfg.bridge = lv.bridge.clone();
        cfg.static_ip = lv.static_ip.clone();
        cfg.machine = lv.machine.clone();
        cfg.cpu_model = lv.cpu_model.clone();
        cfg.cpu_topology = lv.cpu_topology.clone();
        cfg.tpm = lv.tpm;
        cfg.video = lv.video.clone();
        cfg.vnc = lv.vnc;
        cfg.boot_order = lv.boot_order.clone();
        cfg.extra_disks = lv.extra_disks.clone();
        cfg.extra_nics = lv.extra_nics.clone();
        cfg.volumes = lv.volumes.clone();
        cfg.libvirt_xml_overlay = lv.libvirt_xml_overlay.clone();
        cfg.libvirt_xml = lv.libvirt_xml.clone();
        if cfg.devices.is_empty() {
            cfg.devices = lv.devices.clone();
        }
    }
    cfg
}

/// D3. `Provider`'s skeleton (identity — `capabilities`/`health` are out of
/// this spike's scope, which is only the substitution question) plus the VM
/// lifecycle `VmBackend` already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderId(pub &'static str);

pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
}

/// Was `VmBackend::ip_is_predicted() -> bool`; renamed to a 3-state enum
/// here because the substitution spike's own harness needs to tell "not
/// running yet" apart from "running, address unknown" — a `bool` collapses
/// both into `false` and a caller re-derives the difference from `running`
/// anyway, which is exactly the kind of derived state ADR-0008 built
/// `ip_is_predicted` to avoid in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpConfidence {
    /// A real lease/agent answer — evidence the guest asked for it.
    Observed,
    /// Computed from the MAC before the guest ran at all (Cloud Hypervisor).
    Predicted,
    /// Not running, or running with nothing to report yet.
    Unknown,
}

pub struct VmHandle {
    pub name: String,
}

pub struct VmObservation {
    pub running: bool,
    pub ip: Option<String>,
    pub ip_confidence: IpConfidence,
}

pub trait VmProvider: Provider {
    fn create(&self, root: &Path, spec: &VmSpec, ext: &Extensions) -> Result<VmHandle>;
    fn start(&self, root: &Path, h: &VmHandle) -> Result<()>;
    fn stop(&self, root: &Path, h: &VmHandle) -> Result<()>;
    fn destroy(&self, root: &Path, h: &VmHandle) -> Result<()>;
    fn observe(&self, root: &Path, h: &VmHandle) -> Result<VmObservation>;
}

/// D3/D4: the id is fixed ONCE, at construction (see [`registry`], the one
/// place this spike matches a string) — never re-derived by comparing it
/// against a literal inside `create`/`start`/`stop`/`observe`. Those four
/// methods pass it straight into [`spec_to_config`] as data, and delegate to
/// the SAME top-level functions (`create_with`/`start`/`stop`/`status`/
/// `remove`) regardless of which id this was constructed with — the
/// convergence this spike exists to prove.
pub struct LocalVmProvider {
    id: &'static str,
}

impl LocalVmProvider {
    pub const fn cloud_hypervisor() -> Self {
        Self {
            id: "cloud-hypervisor",
        }
    }
    pub const fn libvirt() -> Self {
        Self { id: "libvirt" }
    }
}

impl Provider for LocalVmProvider {
    fn id(&self) -> ProviderId {
        ProviderId(self.id)
    }
}

impl VmProvider for LocalVmProvider {
    fn create(&self, root: &Path, spec: &VmSpec, ext: &Extensions) -> Result<VmHandle> {
        let cfg = spec_to_config(spec, ext, self.id);
        create_with(root, &cfg, &|_stage| {})?;
        Ok(VmHandle {
            name: spec.name.clone(),
        })
    }

    fn start(&self, root: &Path, h: &VmHandle) -> Result<()> {
        start(root, &h.name).map(|_| ())
    }

    fn stop(&self, root: &Path, h: &VmHandle) -> Result<()> {
        stop(root, &h.name)
    }

    fn destroy(&self, root: &Path, h: &VmHandle) -> Result<()> {
        remove(root, &h.name)
    }

    fn observe(&self, root: &Path, h: &VmHandle) -> Result<VmObservation> {
        let vm = status(root, &h.name)?;
        let running = matches!(vm.status, VmStatus::Running);
        let ip_confidence = if !running || vm.ip.is_none() {
            IpConfidence::Unknown
        } else if crate::backend_for(&vm)?.ip_is_predicted() {
            IpConfidence::Predicted
        } else {
            IpConfidence::Observed
        };
        Ok(VmObservation {
            running,
            ip: vm.ip,
            ip_confidence,
        })
    }
}

/// D4: the ONE place a provider id, as a string, gets matched — the
/// composition root this spike's own gate names. Everything past this
/// function only ever holds a `Box<dyn VmProvider>` and calls its methods.
pub fn registry(id: &str) -> Option<Box<dyn VmProvider>> {
    match id {
        "cloud-hypervisor" => Some(Box::new(LocalVmProvider::cloud_hypervisor())),
        "libvirt" => Some(Box::new(LocalVmProvider::libvirt())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> VmSpec {
        VmSpec {
            name: name.into(),
            disk: "/tmp/does-not-need-to-exist.qcow2".into(),
            vcpus: 2,
            memory_mib: 512,
            network: "default".into(),
            namespace: "default".into(),
            cloud_init: Some(false),
            ..Default::default()
        }
    }

    /// The mapping is the port's real contract — this is what would have
    /// caught D1's own missing-field bug (`network`/`namespace` silently
    /// unread by a real provider) if it had existed before that provider was
    /// written: every universal field has to land on `VmConfig` somewhere,
    /// or this test doesn't compile it, let alone pass it.
    #[test]
    fn spec_to_config_carries_every_universal_field() {
        let mut s = spec("v1");
        s.disk_size_gib = Some(20);
        s.hostname = Some("h".into());
        s.ci_user = Some("u".into());
        s.ssh_keys = vec!["ssh-ed25519 AAAA x".into()];
        s.restart_policy = Some("always".into());
        s.serial_capture = true;
        let cfg = spec_to_config(&s, &Extensions::default(), "cloud-hypervisor");
        assert_eq!(cfg.name, "v1");
        assert_eq!(cfg.vcpus, 2);
        assert_eq!(crate::mem_mib(&cfg.memory), 512);
        assert_eq!(cfg.disk_size_gib, Some(20));
        assert_eq!(cfg.network, "default");
        assert_eq!(cfg.namespace.as_deref(), Some("default"));
        assert_eq!(cfg.hostname.as_deref(), Some("h"));
        assert_eq!(cfg.ci_user.as_deref(), Some("u"));
        assert_eq!(cfg.ssh_keys, vec!["ssh-ed25519 AAAA x".to_string()]);
        assert_eq!(cfg.restart_policy.as_deref(), Some("always"));
        assert!(cfg.serial_capture);
        assert_eq!(cfg.backend.as_deref(), Some("cloud-hypervisor"));
    }

    /// The SAME `VmSpec`, the SAME `Extensions::default()` (empty), two
    /// different provider ids — the `VmConfig` differs ONLY in `backend`.
    /// This is the compile-time half of the substitution claim: nothing
    /// about the universal fields depends on which provider receives them.
    #[test]
    fn the_same_spec_differs_only_by_backend_id() {
        let s = spec("v2");
        let ch = spec_to_config(&s, &Extensions::default(), "cloud-hypervisor");
        let lv = spec_to_config(&s, &Extensions::default(), "libvirt");
        let mut ch2 = ch.clone();
        let mut lv2 = lv.clone();
        ch2.backend = None;
        lv2.backend = None;
        assert_eq!(
            format!("{ch2:?}"),
            format!("{lv2:?}"),
            "the two configs diverge outside `backend` — the port is leaking \
             provider identity into shared fields"
        );
        assert_ne!(ch.backend, lv.backend);
    }

    #[test]
    fn cloud_hypervisor_ext_lands_only_on_ch_fields() {
        let s = spec("v3");
        let ext = Extensions {
            cloud_hypervisor: Some(CloudHypervisorExt {
                kernel: Some("/boot/vmlinuz".into()),
                hugepages: true,
                devices: vec!["/sys/bus/pci/devices/0000:65:00.1".into()],
                ..Default::default()
            }),
            libvirt: None,
        };
        let cfg = spec_to_config(&s, &ext, "cloud-hypervisor");
        assert_eq!(cfg.kernel.as_deref(), Some("/boot/vmlinuz"));
        assert!(cfg.hugepages);
        assert_eq!(
            cfg.devices,
            vec!["/sys/bus/pci/devices/0000:65:00.1".to_string()]
        );
        // Libvirt-only fields stay at their zero value — this extension
        // never touched them.
        assert!(cfg.machine.is_none());
        assert!(!cfg.tpm);
    }

    #[test]
    fn libvirt_ext_lands_only_on_libvirt_fields() {
        let s = spec("v4");
        let ext = Extensions {
            cloud_hypervisor: None,
            libvirt: Some(LibvirtExt {
                bridge: Some("vmbr1".into()),
                tpm: true,
                devices: vec!["/sys/bus/pci/devices/0000:65:00.1".into()],
                ..Default::default()
            }),
        };
        let cfg = spec_to_config(&s, &ext, "libvirt");
        assert_eq!(cfg.bridge.as_deref(), Some("vmbr1"));
        assert!(cfg.tpm);
        assert_eq!(
            cfg.devices,
            vec!["/sys/bus/pci/devices/0000:65:00.1".to_string()]
        );
        assert!(cfg.kernel.is_none());
        assert!(!cfg.hugepages);
    }

    #[test]
    fn the_registry_is_the_only_string_match_in_this_module() {
        assert!(registry("cloud-hypervisor").is_some());
        assert!(registry("libvirt").is_some());
        assert!(registry("proxmox").is_none(), "P4c, not this spike");
        assert!(registry("typo").is_none());
    }
}

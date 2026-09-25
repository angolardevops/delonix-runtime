//! What each LOCAL VM backend says about the capability catalog
//! (`delonix_compute::capability`, ADR-0050), and the host probe that narrows
//! a declared "yes" into "not on this host".
//!
//! The declaration is a `match` with no wildcard arm, on purpose: a catalog
//! entry added tomorrow fails to compile here until the backend answers it.
//! Every `Supported` names its evidence — a battery check, a scenario or a
//! test — and `bins/delonix-runtime-bin` has a test that greps for each one,
//! so an evidence string that names nothing real is a red test, not a claim.
//!
//! Two backends, two probes, and the probe is a plain struct so the SAME
//! declaration can be rendered against an assumed-complete host (the published
//! matrix) and against this machine (`delonix provider ls`).

use delonix_compute::capability::{
    Capability as C, CapabilityState as S, HealthStatus, ProviderHealth, ProviderKind,
    ProviderReport,
};

/// What the libvirt backend needs from the host, each probed separately so the
/// message names the missing piece instead of a generic "not available".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibvirtHost {
    pub virsh: bool,
    pub qemu: bool,
    pub kvm: bool,
    /// `qemu:///system` answers — needed for `nat`/`bridge` (an observed IP).
    pub system_uri: bool,
}

impl LibvirtHost {
    /// Everything present: the declaration as published, host-independent.
    pub const ASSUMED: LibvirtHost = LibvirtHost {
        virsh: true,
        qemu: true,
        kvm: true,
        system_uri: true,
    };

    /// Reads this machine. Costs three `which` and one `virsh uri` round trip;
    /// never creates anything.
    pub fn probe() -> LibvirtHost {
        let virsh = super::binary_in_path("virsh");
        LibvirtHost {
            virsh,
            qemu: super::binary_in_path("qemu-system-x86_64"),
            kvm: std::path::Path::new("/dev/kvm").exists(),
            system_uri: virsh && super::system_libvirt_usable(),
        }
    }
}

/// What the Cloud Hypervisor backend needs from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudHypervisorHost {
    pub binary: bool,
    pub kvm: bool,
    /// One of the firmwares the engine knows how to boot with is installed.
    pub firmware: bool,
}

impl CloudHypervisorHost {
    pub const ASSUMED: CloudHypervisorHost = CloudHypervisorHost {
        binary: true,
        kvm: true,
        firmware: true,
    };

    pub fn probe() -> CloudHypervisorHost {
        CloudHypervisorHost {
            binary: super::binary_in_path("cloud-hypervisor"),
            kvm: std::path::Path::new("/dev/kvm").exists(),
            firmware: super::DEFAULT_CH_FIRMWARES
                .iter()
                .any(|f| std::path::Path::new(f).exists()),
        }
    }
}

fn health(available: bool, reason: &'static str, message: String) -> ProviderHealth {
    ProviderHealth {
        status: if available {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unavailable
        },
        reason,
        message,
    }
}

/// The libvirt backend's report against `host`.
pub fn libvirt_report(host: &LibvirtHost) -> ProviderReport {
    let available = host.virsh && host.qemu;
    let (reason, message) = if !host.virsh {
        (
            "VirshMissing",
            "virsh is not installed (libvirt-clients)".to_string(),
        )
    } else if !host.qemu {
        (
            "QemuMissing",
            "qemu-system-x86_64 is not installed".to_string(),
        )
    } else if !host.kvm {
        (
            "NoKvm",
            "/dev/kvm is absent: guests would run without acceleration".to_string(),
        )
    } else if !host.system_uri {
        (
            "SessionOnly",
            "qemu:///system does not answer (join the `libvirt` group): only user-mode networking, no observed IP".to_string(),
        )
    } else {
        ("Ok", String::new())
    };
    // The system connection is what most of the network-facing rows need; a
    // session-only host keeps the lifecycle and loses the observed address.
    let bin = |s: S| s.on_host(available, "virsh + qemu-system-x86_64 not both installed");
    // Narrowed AFTER `bin`: a host without virsh has no system connection
    // either, whatever the probe struct says — `probe()` derives one from the
    // other, but the declaration must not depend on that coupling.
    let sys = |s: S| bin(s).on_host(host.system_uri, "needs qemu:///system (libvirt group)");
    ProviderReport::build(
        "libvirt",
        ProviderKind::Compute,
        available,
        health(available, reason, message),
        |c| {
            match c {
        C::ProviderAvailability => bin(S::Partial { detail: "`available()` checks the two binaries; `provider ls` additionally probes qemu:///system" }),
        C::ResourceReadback => bin(S::Supported { evidence: "check:o vm ls diz Paused (não Stopped)" }),
        C::Events => S::NotImplemented,
        C::AsyncOperations => S::NotImplemented,
        C::VmCreate => bin(S::Supported { evidence: "e2e:vm: o snapshot sobrevive a um stop/start (precisa de hipervisor)" }),
        C::VmStart => bin(S::Supported { evidence: "check:vm start" }),
        C::VmStop => bin(S::Supported { evidence: "check:vm stop" }),
        C::VmDestroy => bin(S::Partial { detail: "`vm destroy` undefines the domain, releases the DHCP reservation and removes the overlay; the battery tears down without a named check" }),
        C::VmRestart => bin(S::Partial { detail: "stop-then-start, always a real reboot; no battery check names it" }),
        C::VmPause => bin(S::Supported { evidence: "check:vm pause" }),
        C::VmResume => bin(S::Supported { evidence: "check:vm unpause" }),
        C::VmResumeSameIdentity => bin(S::Supported { evidence: "check:vm start depois do stop de uma VM pausada" }),
        C::VmClone => S::NotImplemented,
        C::VmTemplate => S::NotImplemented,
        C::VmResizeCold => S::NotImplemented,
        C::VmHotplug => S::NotImplemented,
        C::VmExtraDisks => bin(S::Partial { detail: "`extraDisks` reach the domain XML (unit-tested target letters); never booted in the battery" }),
        C::VmExtraNics => bin(S::Partial { detail: "`extraNics` (network/bridge/user) reach the domain XML; never booted in the battery" }),
        C::VmDiskResize => S::NotImplemented,
        C::VmPciPassthrough => bin(S::Partial { detail: "`<hostdev>` per validated PCI address; no IOMMU host in the battery" }),
        C::VmTpm => bin(S::Partial { detail: "`<tpm model='tpm-crb'>` emulator; swtpm presence is not probed" }),
        C::VmCpuModel => bin(S::Partial { detail: "`cpuModel`/`cpuTopology` in the XML, default host-passthrough; not booted in the battery" }),
        C::VmCpuPinning => bin(S::Partial { detail: "`<cputune>` quota + `<vcpupin>`; unit-tested XML, dropped on qemu:///session" }),
        C::VmHugepages => bin(S::Partial { detail: "`<memoryBacking><hugepages/>`; the host's hugepage pool is not probed" }),
        C::VmCloudInit => bin(S::Partial { detail: "NoCloud seed on a virtio disk (#435); the battery never logs into a guest" }),
        C::VmRestartPolicyNative => bin(S::Partial { detail: "`on_crash=restart` for always/on-failure; unit-tested, no guest crashed on purpose" }),
        C::VmNamespaceIsolation => S::UnsupportedByProvider { reason: "a libvirt VM lives on virbr0 in the host netns, a different L2 the engine does not program; `--namespace` is refused by name" },
        C::VmAntispoof => sys(S::Supported { evidence: "test:crates/adapters/delonix-vm/src/lib.rs::antispoof_live_defines_and_survives_the_domain" }),
        C::VmRawDefinition => bin(S::Partial { detail: "`libvirtXml`/`libvirtXmlOverlay` are UNVALIDATED, trusted manifests only, local CLI only — never reachable from the node contract (ADR-0050)" }),
        C::ContainerLifecycle | C::ContainerExec | C::ContainerLogs | C::ContainerHotReconfigure
        | C::ContainerResourceLimits | C::ContainerGpuCdi | C::ContainerSeccompCustomProfile
        | C::ContainerOomDetection | C::PodSharedNetwork | C::PodSharedIpcUts | C::PodSharedPid
        | C::ContainerImages | C::ContainerBackupRestore => {
            S::UnsupportedByProvider { reason: "a VM provider; containers are the Linux provider's" }
        }
        C::VmNetworkNat => sys(S::Supported { evidence: "e2e:vm: o snapshot sobrevive a um stop/start (precisa de hipervisor)" }),
        C::VmNetworkBridge => sys(S::Partial { detail: "`netMode: bridge` enslaves the NIC to a host bridge; validated live for ADR-0046 phase 2, no battery check" }),
        C::VmNetworkSdn => S::UnsupportedByProvider { reason: "the NIC is on virbr0 (host netns), not on the engine's SDN; `vm bridge` (root, experimental) is the only path across" },
        C::VmStaticIp => sys(S::Partial { detail: "`--ip` reserves a DHCP host entry (`net-update`), released on destroy; argv unit-tested, no battery" }),
        C::StoragePools => S::NotImplemented,
        C::VmSnapshotDisk => bin(S::Supported { evidence: "check:snapshot create com a VM parada" }),
        C::VmSnapshotMemory => bin(S::Supported { evidence: "check:vm snapshot create" }),
        C::VmSnapshotRestore => bin(S::Supported { evidence: "check:vm snapshot restore depois do start" }),
        C::VmSnapshotDelete => bin(S::Supported { evidence: "check:vm snapshot rm" }),
        C::VmSnapshotPersistent => bin(S::Supported { evidence: "check:o libvirt volta a conhecer o snapshot" }),
        C::VmBackupDisk => bin(S::Partial { detail: "`backup create vm` takes a live external snapshot and block-commits it back; the battery backs up a container, not a VM" }),
        C::VmBackupQuiesced => bin(S::Partial { detail: "`--quiesce` asks the guest agent to freeze; whether the guest has one is not probed" }),
        C::VmBackupRestore => bin(S::Partial { detail: "`backup restore` re-imports the disk; battery covers the container kind only" }),
        C::VmMigrationCold => bin(S::Partial { detail: "`vm migrate`: stop, flatten, copy over SSH, import, start; no battery" }),
        C::VmMigrationLive => S::UnsupportedByProvider { reason: "ADR-0031: needs shared VM storage or a privileged libvirt daemon listening on the network; neither is a default of this engine" },
        C::VmReplication => S::RequiresExternalComponent { component: "shared or replicated VM storage outside the engine (ADR-0031)" },
        C::VmHighAvailability => S::RequiresExternalComponent { component: "a cluster manager with quorum and fencing; a single libvirt host has none" },
        C::VmConsoleSerial => bin(S::Partial { detail: "`vm console` runs `virsh console --force`; interactive, no battery" }),
        C::VmConsoleVnc => bin(S::Partial { detail: "`vm vnc` reads `vncdisplay` of a `--vnc` domain; no battery" }),
        C::VmGuestAgent => S::NotImplemented,
        C::VmIpObserved => sys(S::Partial { detail: "DHCP lease with a lease floor, then `domifaddr`; pure test only, the battery does not read the IP" }),
        C::MetricsPrometheus => S::Partial { detail: "`delonix_vms_running/total` only; no per-domain stats" },
        C::MetricsPerWorkloadNetwork => S::NotImplemented,
        C::HostHealth => S::Supported { evidence: "check:system info" },
        C::HostCapacity => S::NotImplemented,
        C::TransportVerified => S::Partial { detail: "local URIs only (qemu:///system|session); a remote libvirt URI is never built" },
        C::CredentialInVault => S::UnsupportedByProvider { reason: "no credential exists: access is membership of the `libvirt` group" },
        C::NetBridge | C::NetMacvlanIpvlan | C::NetVlan | C::NetOverlayVxlan | C::NetOverlayEncrypted | C::NetIpam | C::NetStaticIp | C::NetDns | C::NetPublishPorts | C::NetRoutesBetweenNetworks | C::NetNamespaceIsolation | C::NetTunnelEgress | C::NetRateLimit | C::NetPacketCapture | C::NetL7Proxy | C::NetIpv6 | C::VolumeLocal | C::VolumeBind | C::VolumeNfs | C::VolumeCifs | C::VolumeWebdav | C::VolumeQuota | C::VolumeSnapshot | C::VolumeProvisionNas | C::StorageLvmThin | C::StorageZfsBtrfs | C::StorageCeph | C::FirewallPerWorkload | C::FirewallDefaultDeny | C::FirewallSourceFiltering | C::FirewallEgressPolicy => {
            S::UnsupportedByProvider { reason: "not a compute capability: answered by the network/storage provider" }
        }
    }
        },
    )
}

/// The Cloud Hypervisor backend's report against `host`.
pub fn cloud_hypervisor_report(host: &CloudHypervisorHost) -> ProviderReport {
    let available = host.binary;
    let (reason, message) = if !host.binary {
        (
            "BinaryMissing",
            "cloud-hypervisor is not installed".to_string(),
        )
    } else if !host.kvm {
        ("NoKvm", "/dev/kvm is absent".to_string())
    } else if !host.firmware {
        (
            "FirmwareMissing",
            format!(
                "none of {} is installed",
                super::DEFAULT_CH_FIRMWARES.join(", ")
            ),
        )
    } else {
        ("Ok", String::new())
    };
    let boots = available && host.kvm && host.firmware;
    let bin = |s: S| {
        s.on_host(
            boots,
            "cloud-hypervisor + /dev/kvm + a known firmware are all needed to boot",
        )
    };
    ProviderReport::build(
        "cloud-hypervisor",
        ProviderKind::Compute,
        available,
        health(available, reason, message),
        |c| {
            match c {
        C::ProviderAvailability => S::Partial { detail: "`available()` checks the binary; `provider ls` additionally probes /dev/kvm and the firmware" },
        C::ResourceReadback => bin(S::Supported { evidence: "check:CH: ls nomeia-o" }),
        C::Events => S::NotImplemented,
        C::AsyncOperations => S::NotImplemented,
        C::VmCreate => bin(S::Supported { evidence: "e2e:vm: os mesmos snapshots no backend cloud-hypervisor" }),
        C::VmStart => bin(S::Supported { evidence: "check:CH: vm start" }),
        C::VmStop => bin(S::Supported { evidence: "check:CH: vm stop" }),
        C::VmDestroy => bin(S::Partial { detail: "terminates the VMM (pid + starttime guard) and removes the overlay; no named battery check" }),
        C::VmRestart => bin(S::Partial { detail: "stop-then-start; no battery check names it" }),
        C::VmPause => bin(S::Partial { detail: "`PUT /api/v1/vm.pause` on the VMM socket; no battery check" }),
        C::VmResume => bin(S::Partial { detail: "`vm.resume` on the VMM socket; no battery check" }),
        C::VmResumeSameIdentity => bin(S::Supported { evidence: "check:CH: vm start" }),
        C::VmClone => S::NotImplemented,
        C::VmTemplate => S::NotImplemented,
        C::VmResizeCold => S::NotImplemented,
        C::VmHotplug => S::NotImplemented,
        C::VmExtraDisks => S::UnsupportedByProvider { reason: "the CH command line carries one root disk and the seed; `extraDisks` are refused for this backend" },
        C::VmExtraNics => S::UnsupportedByProvider { reason: "one tap on the SDN; `extraNics` are refused for this backend" },
        C::VmDiskResize => S::NotImplemented,
        C::VmPciPassthrough => bin(S::Partial { detail: "`--device path=<sysfs>` per validated address; no IOMMU host in the battery" }),
        C::VmTpm => S::UnsupportedByProvider { reason: "no vTPM device is emitted for Cloud Hypervisor" },
        C::VmCpuModel => S::UnsupportedByProvider { reason: "CH exposes the host CPU; no model selection is passed" },
        C::VmCpuPinning => bin(S::Partial { detail: "`cpuAffinity` becomes `--cpus affinity=`; not booted in the battery" }),
        C::VmHugepages => bin(S::Partial { detail: "`--memory hugepages=on`; unit-tested argument, pool not probed" }),
        C::VmCloudInit => bin(S::Partial { detail: "NoCloud seed ISO attached as a second disk; the battery never logs into a guest" }),
        C::VmRestartPolicyNative => S::UnsupportedByProvider { reason: "the VMM has no crash policy; the engine's supervisor restarts it (`restart_policy_unsupervised`)" },
        C::VmNamespaceIsolation => bin(S::Supported { evidence: "chaos:scen_namespace_isolation" }),
        C::VmAntispoof => bin(S::Partial { detail: "the tap gets the same `iifname … ip saddr != <ip> drop` rule as a veth (auditoria #3); proven by rule inspection, not in the battery" }),
        C::VmRawDefinition => S::UnsupportedByProvider { reason: "no raw definition format exists for the CH command line" },
        C::ContainerLifecycle | C::ContainerExec | C::ContainerLogs | C::ContainerHotReconfigure
        | C::ContainerResourceLimits | C::ContainerGpuCdi | C::ContainerSeccompCustomProfile
        | C::ContainerOomDetection | C::PodSharedNetwork | C::PodSharedIpcUts | C::PodSharedPid
        | C::ContainerImages | C::ContainerBackupRestore => {
            S::UnsupportedByProvider { reason: "a VM provider; containers are the Linux provider's" }
        }
        C::VmNetworkNat => S::UnsupportedByProvider { reason: "no hypervisor NAT network: the tap joins the engine's SDN, which does the NAT" },
        C::VmNetworkBridge => S::UnsupportedByProvider { reason: "the NIC is a tap on the SDN bridge inside the holder, never a host bridge" },
        C::VmNetworkSdn => bin(S::Supported { evidence: "check:CH: vm start" }),
        C::VmStaticIp => S::UnsupportedByProvider { reason: "the address is derived from the MAC by the engine's DHCP; a fixed IP is not honoured" },
        C::StoragePools => S::NotImplemented,
        C::VmSnapshotDisk => bin(S::Supported { evidence: "check:CH: create com a VM parada" }),
        C::VmSnapshotMemory => S::UnsupportedByProvider { reason: "`vm.snapshot` of the VMM saves memory without the disk, which cannot be restored consistently; refused by design" },
        C::VmSnapshotRestore => bin(S::Supported { evidence: "check:CH: restore" }),
        C::VmSnapshotDelete => bin(S::Supported { evidence: "check:CH: rm com a VM parada" }),
        C::VmSnapshotPersistent => bin(S::Supported { evidence: "check:CH: ls nomeia-o" }),
        C::VmBackupDisk => bin(S::Partial { detail: "`backup create vm` copies the overlay of a STOPPED VM; a running CH VM holds the qcow2 exclusively" }),
        C::VmBackupQuiesced => S::UnsupportedByProvider { reason: "no guest agent channel; only an offline copy is consistent" },
        C::VmBackupRestore => bin(S::Partial { detail: "`backup restore` re-imports the disk; battery covers the container kind only" }),
        C::VmMigrationCold => bin(S::Partial { detail: "`vm migrate`: stop, copy over SSH, import, start; no battery" }),
        C::VmMigrationLive => S::UnsupportedByProvider { reason: "ADR-0031: CH migrates memory only, never the disk" },
        C::VmReplication => S::RequiresExternalComponent { component: "shared or replicated VM storage outside the engine (ADR-0031)" },
        C::VmHighAvailability => S::RequiresExternalComponent { component: "a cluster manager with quorum and fencing" },
        C::VmConsoleSerial => bin(S::Partial { detail: "`vm console` bridges the tty to the VMM's serial socket; no battery" }),
        C::VmConsoleVnc => S::UnsupportedByProvider { reason: "no VNC device on the CH command line" },
        C::VmGuestAgent => S::NotImplemented,
        C::VmIpObserved => S::UnsupportedByProvider { reason: "the address is PREDICTED from the MAC (`ip_is_predicted`); `--wait` verifies it by ARP instead" },
        C::MetricsPrometheus => S::Partial { detail: "`delonix_vms_running/total` only" },
        C::MetricsPerWorkloadNetwork => S::Partial { detail: "the tap's counters are readable in the holder; not exported per VM" },
        C::HostHealth => S::Supported { evidence: "check:system info" },
        C::HostCapacity => S::NotImplemented,
        C::TransportVerified => S::Partial { detail: "a unix socket per VMM, owned by the engine's uid" },
        C::CredentialInVault => S::UnsupportedByProvider { reason: "no credential exists: the VMM is a child of the engine" },
        C::NetBridge | C::NetMacvlanIpvlan | C::NetVlan | C::NetOverlayVxlan | C::NetOverlayEncrypted | C::NetIpam | C::NetStaticIp | C::NetDns | C::NetPublishPorts | C::NetRoutesBetweenNetworks | C::NetNamespaceIsolation | C::NetTunnelEgress | C::NetRateLimit | C::NetPacketCapture | C::NetL7Proxy | C::NetIpv6 | C::VolumeLocal | C::VolumeBind | C::VolumeNfs | C::VolumeCifs | C::VolumeWebdav | C::VolumeQuota | C::VolumeSnapshot | C::VolumeProvisionNas | C::StorageLvmThin | C::StorageZfsBtrfs | C::StorageCeph | C::FirewallPerWorkload | C::FirewallDefaultDeny | C::FirewallSourceFiltering | C::FirewallEgressPolicy => {
            S::UnsupportedByProvider { reason: "not a compute capability: answered by the network/storage provider" }
        }
    }
        },
    )
}

/// A report for a backend that declares nothing — every row `not-implemented`
/// and health `Unknown`/`NotDeclared`. What a test fake registers; never a
/// real backend, which is why the id is stamped in the message.
pub fn undeclared(id: &'static str) -> super::ReportFactory {
    Box::new(move || {
        ProviderReport::build(
            id,
            ProviderKind::Compute,
            false,
            ProviderHealth {
                status: HealthStatus::Unknown,
                reason: "NotDeclared",
                message: format!("backend '{id}' registered without a capability report"),
            },
            |_| S::NotImplemented,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_only_host_keeps_the_lifecycle_and_loses_the_observed_address() {
        let host = LibvirtHost {
            system_uri: false,
            ..LibvirtHost::ASSUMED
        };
        let r = libvirt_report(&host);
        assert!(r.available);
        assert_eq!(r.health.reason, "SessionOnly");
        let by = |c: C| {
            r.capabilities
                .iter()
                .find(|x| x.capability == c)
                .unwrap()
                .state
                .clone()
        };
        assert_eq!(by(C::VmStart).label(), "supported");
        assert_eq!(by(C::VmNetworkNat).label(), "unavailable-on-host");
        // A declared "no" stays a "no" whatever the host looks like.
        assert_eq!(by(C::VmMigrationLive).label(), "unsupported-by-provider");
    }

    #[test]
    fn without_virsh_nothing_that_needs_it_reads_as_supported() {
        let host = LibvirtHost {
            virsh: false,
            ..LibvirtHost::ASSUMED
        };
        let r = libvirt_report(&host);
        assert!(!r.available);
        assert_eq!(r.health.reason, "VirshMissing");
        assert!(r
            .capabilities
            .iter()
            .all(|c| { c.state.label() != "supported" || c.capability == C::HostHealth }));
    }

    #[test]
    fn the_two_local_backends_answer_every_compute_row() {
        let n = C::ALL
            .iter()
            .filter(|c| c.kind() == ProviderKind::Compute)
            .count();
        assert_eq!(libvirt_report(&LibvirtHost::ASSUMED).capabilities.len(), n);
        assert_eq!(
            cloud_hypervisor_report(&CloudHypervisorHost::ASSUMED)
                .capabilities
                .len(),
            n
        );
    }

    #[test]
    fn cloud_hypervisor_without_firmware_is_available_but_cannot_boot() {
        let host = CloudHypervisorHost {
            firmware: false,
            ..CloudHypervisorHost::ASSUMED
        };
        let r = cloud_hypervisor_report(&host);
        assert!(r.available, "the binary is there: selectable");
        assert_eq!(r.health.reason, "FirmwareMissing");
        let start = r
            .capabilities
            .iter()
            .find(|x| x.capability == C::VmStart)
            .unwrap();
        assert_eq!(start.state.label(), "unavailable-on-host");
    }
}

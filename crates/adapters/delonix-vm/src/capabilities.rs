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

// The libvirt half moved to its provider crate (ADR-0044 P4b.4b); re-exported
// so the bin and the node API keep reading both reports from here.
pub use delonix_provider_libvirt::{libvirt_report, LibvirtHost};

use delonix_compute::capability::{
    Capability as C, CapabilityState as S, HealthStatus, ProviderHealth, ProviderKind,
    ProviderReport,
};

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
            C::ProviderAvailability => S::Partial {
                detail: "`available()` checks the binary; `provider ls` additionally probes /dev/kvm and the firmware",
            },
            C::ResourceReadback => bin(S::Supported {
                evidence: "check:CH: ls nomeia-o",
            }),
            C::Events => S::NotImplemented,
            C::AsyncOperations => S::NotImplemented,
            C::VmCreate => bin(S::Supported {
                evidence: "e2e:vm: os mesmos snapshots no backend cloud-hypervisor",
            }),
            C::VmStart => bin(S::Supported {
                evidence: "check:CH: vm start",
            }),
            C::VmStop => bin(S::Supported {
                evidence: "check:CH: vm stop",
            }),
            C::VmDestroy => bin(S::Partial {
                detail: "terminates the VMM (pid + starttime guard) and removes the overlay; no named battery check",
            }),
            C::VmRestart => bin(S::Partial {
                detail: "stop-then-start; no battery check names it",
            }),
            C::VmPause => bin(S::Partial {
                detail: "`PUT /api/v1/vm.pause` on the VMM socket; no battery check",
            }),
            C::VmResume => bin(S::Partial {
                detail: "`vm.resume` on the VMM socket; no battery check",
            }),
            C::VmResumeSameIdentity => bin(S::Supported {
                evidence: "check:CH: vm start",
            }),
            C::VmClone => S::NotImplemented,
            C::VmTemplate => S::NotImplemented,
            C::VmResizeCold => bin(S::Partial {
                detail: "`vm resize` rewrites vcpus/memory in the record and `vm start` rebuilds the vmm command line from it; no battery check yet",
            }),
            C::VmHotplug => S::NotImplemented,
            C::VmExtraDisks => S::UnsupportedByProvider {
                reason: "the CH command line carries one root disk and the seed; `extraDisks` are refused for this backend",
            },
            C::VmExtraNics => S::UnsupportedByProvider {
                reason: "one tap on the SDN; `extraNics` are refused for this backend",
            },
            C::VmDiskResize => S::NotImplemented,
            C::VmPciPassthrough => bin(S::Partial {
                detail: "`--device path=<sysfs>` per validated address; no IOMMU host in the battery",
            }),
            C::VmTpm => S::UnsupportedByProvider {
                reason: "no vTPM device is emitted for Cloud Hypervisor",
            },
            C::VmCpuModel => S::UnsupportedByProvider {
                reason: "CH exposes the host CPU; no model selection is passed",
            },
            C::VmCpuPinning => bin(S::Partial {
                detail: "`cpuAffinity` becomes `--cpus affinity=`; not booted in the battery",
            }),
            C::VmHugepages => bin(S::Partial {
                detail: "`--memory hugepages=on`; unit-tested argument, pool not probed",
            }),
            C::VmCloudInit => bin(S::Partial {
                detail: "NoCloud seed ISO attached as a second disk; the battery never logs into a guest",
            }),
            C::VmRestartPolicyNative => S::UnsupportedByProvider {
                reason: "the VMM has no crash policy; the engine's supervisor restarts it (`restart_policy_unsupervised`)",
            },
            C::VmNamespaceIsolation => bin(S::Supported {
                evidence: "chaos:scen_namespace_isolation",
            }),
            C::VmAntispoof => bin(S::Partial {
                detail: "the tap gets the same `iifname … ip saddr != <ip> drop` rule as a veth (auditoria #3); proven by rule inspection, not in the battery",
            }),
            C::VmRawDefinition => S::UnsupportedByProvider {
                reason: "no raw definition format exists for the CH command line",
            },
            C::SystemContainerLifecycle
            | C::SystemContainerOciImage
            | C::SystemContainerEntrypointEnv
            | C::SystemContainerExec
            | C::SystemContainerLogs
            | C::SystemContainerExitStatus
            | C::SystemContainerNetworkBridge
            | C::SystemContainerUnprivileged
            | C::SystemContainerSnapshot
            | C::SystemContainerResize
            | C::SystemContainerBackup
            | C::SystemContainerClone
            | C::SystemContainerFirewall
            | C::SystemContainerMove => S::UnsupportedByProvider {
                reason: "a VM provider; a system container is the Proxmox provider's (ADR-0058)",
            },
            C::ContainerLifecycle
            | C::ContainerExec
            | C::ContainerLogs
            | C::ContainerHotReconfigure
            | C::ContainerResourceLimits
            | C::ContainerGpuCdi
            | C::ContainerSeccompCustomProfile
            | C::ContainerOomDetection
            | C::PodSharedNetwork
            | C::PodSharedIpcUts
            | C::PodSharedPid
            | C::ContainerImages
            | C::ContainerBackupRestore => S::UnsupportedByProvider {
                reason: "a VM provider; containers are the Linux provider's",
            },
            C::VmNetworkNat => S::UnsupportedByProvider {
                reason: "no hypervisor NAT network: the tap joins the engine's SDN, which does the NAT",
            },
            C::VmNetworkBridge => S::UnsupportedByProvider {
                reason: "the NIC is a tap on the SDN bridge inside the holder, never a host bridge",
            },
            C::VmNetworkSdn => bin(S::Supported {
                evidence: "check:CH: vm start",
            }),
            C::VmStaticIp => S::UnsupportedByProvider {
                reason: "the address is derived from the MAC by the engine's DHCP; a fixed IP is not honoured",
            },
            C::StoragePools => S::NotImplemented,
            C::VmSnapshotDisk => bin(S::Supported {
                evidence: "check:CH: create com a VM parada",
            }),
            C::VmSnapshotMemory => S::UnsupportedByProvider {
                reason: "`vm.snapshot` of the VMM saves memory without the disk, which cannot be restored consistently; refused by design",
            },
            C::VmSnapshotRestore => bin(S::Supported {
                evidence: "check:CH: restore",
            }),
            C::VmSnapshotDelete => bin(S::Supported {
                evidence: "check:CH: rm com a VM parada",
            }),
            C::VmSnapshotPersistent => bin(S::Supported {
                evidence: "check:CH: ls nomeia-o",
            }),
            C::VmBackupDisk => bin(S::Partial {
                detail: "`backup create vm` copies the overlay of a STOPPED VM; a running CH VM holds the qcow2 exclusively",
            }),
            C::VmBackupQuiesced => S::UnsupportedByProvider {
                reason: "no guest agent channel; only an offline copy is consistent",
            },
            C::VmBackupRestore => bin(S::Partial {
                detail: "`backup restore` re-imports the disk; battery covers the container kind only",
            }),
            C::VmMigrationCold => bin(S::Partial {
                detail: "`vm migrate`: stop, copy over SSH, import, start; no battery",
            }),
            C::VmMigrationLive => S::UnsupportedByProvider {
                reason: "ADR-0031: CH migrates memory only, never the disk",
            },
            C::VmReplication => S::RequiresExternalComponent {
                component: "shared or replicated VM storage outside the engine (ADR-0031)",
            },
            C::VmHighAvailability => S::RequiresExternalComponent {
                component: "a cluster manager with quorum and fencing",
            },
            C::VmConsoleSerial => bin(S::Partial {
                detail: "`vm console` bridges the tty to the VMM's serial socket; no battery",
            }),
            C::VmConsoleVnc => S::UnsupportedByProvider {
                reason: "no VNC device on the CH command line",
            },
            C::VmGuestAgent => S::NotImplemented,
            C::VmIpObserved => S::UnsupportedByProvider {
                reason: "the address is PREDICTED from the MAC (`ip_is_predicted`); `--wait` verifies it by ARP instead",
            },
            C::MetricsPrometheus => S::Partial {
                detail: "`delonix_vms_running/total` only",
            },
            C::MetricsPerWorkloadNetwork => S::Partial {
                detail: "the tap's counters are readable in the holder; not exported per VM",
            },
            C::HostHealth => S::Supported {
                evidence: "check:system info",
            },
            C::HostCapacity => S::NotImplemented,
            C::TransportVerified => S::Partial {
                detail: "a unix socket per VMM, owned by the engine's uid",
            },
            C::CredentialInVault => S::UnsupportedByProvider {
                reason: "no credential exists: the VMM is a child of the engine",
            },
            C::NetBridge
            | C::NetMacvlanIpvlan
            | C::NetVlan
            | C::NetOverlayVxlan
            | C::NetOverlayEncrypted
            | C::NetIpam
            | C::NetStaticIp
            | C::NetDns
            | C::NetPublishPorts
            | C::NetRoutesBetweenNetworks
            | C::NetNamespaceIsolation
            | C::NetTunnelEgress
            | C::NetRateLimit
            | C::NetPacketCapture
            | C::NetL7Proxy
            | C::NetIpv6
            | C::VolumeLocal
            | C::VolumeBind
            | C::VolumeNfs
            | C::VolumeCifs
            | C::VolumeWebdav
            | C::VolumeQuota
            | C::VolumeSnapshot
            | C::VolumeProvisionNas
            | C::StorageLvmThin
            | C::StorageZfsBtrfs
            | C::StorageCeph
            | C::FirewallPerWorkload
            | C::FirewallDefaultDeny
            | C::FirewallSourceFiltering
            | C::FirewallEgressPolicy
            | C::NetGatewayFilter
            | C::NetGatewayAlias
            | C::NetGatewayUpdateInPlace
            | C::NetGatewayRuleOrder
            | C::NetGatewayMultiWan
            | C::NetGatewayVpn
            | C::NetNatSnat
            | C::NetNatDnat
            | C::NetNatOneToOne
            | C::NetNatNpt
            | C::NetLbL4
            | C::NetLbHealthCheck
            | C::NetDnsRecords
            | C::NetDnsAuthoritative
            | C::NetIpamProvider
            | C::NetIpamReservation
            | C::NetIpamDhcp
            | C::NetSegmentRemote
            | C::NetApplyStaged
            | C::NetApplyRollback
            | C::NetObserve
            | C::NetVerifyDataplane
            | C::NetOwnershipMarker
            | C::FirewallStateless
            | C::FirewallLogging
            | C::FirewallIcmpType
            | C::FirewallWorkloadPeer => S::UnsupportedByProvider {
                reason: "not a compute capability: answered by the network/storage provider",
            },
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
}

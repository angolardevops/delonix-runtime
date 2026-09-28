//! The OPNsense provider's answer to the capability catalog (ADR-0050, catalog
//! 1.1.0 — ADR-0059 D2): a perimeter appliance, reported under
//! [`ProviderKind::Gateway`], which walks the same `network` rows as the node's
//! own SDN so the two compare row by row.
//!
//! **Declared, never probed.** Building this report contacts nothing, like the
//! Proxmox compute report: a configured target is `Unknown`/`NotProbed`, an
//! absent one `Unavailable`/`NotConfigured`. What the appliance CAN do was read
//! from the 26.1.2 image's API controllers (docs/discovery/64 §4); what the
//! ENGINE does with it is what each row says — a controller the client never
//! calls is `not-implemented`, not `supported`.

use delonix_compute::capability::{
    Capability as C, CapabilityState as S, HealthStatus, ProviderHealth, ProviderKind,
    ProviderReport,
};

/// The appliance's report. `configured` = a target was registered in this
/// process (from the providers file or the environment).
pub fn capability_report(configured: bool) -> ProviderReport {
    let health = if configured {
        ProviderHealth {
            status: HealthStatus::Unknown,
            reason: "NotProbed",
            message: "a remote provider is not contacted by `provider ls`; the first operation authenticates".to_string(),
        }
    } else {
        ProviderHealth {
            status: HealthStatus::Unavailable,
            reason: "NotConfigured",
            message: "add a `type: opnsense` entry to providers.yaml, or set DELONIX_OPNSENSE_URL and a credential".to_string(),
        }
    };
    const LIVE: &str = "live:crates/providers/delonix-opnsense/tests/live.rs::ensures_and_removes_an_alias_and_a_rule_against_a_real_appliance";
    ProviderReport::build(crate::ID, ProviderKind::Gateway, configured, health, |c| {
        match c {
        C::NetGatewayFilter | C::NetGatewayAlias | C::NetApplyStaged => S::Supported { evidence: LIVE },
        C::NetGatewayUpdateInPlace | C::NetGatewayRuleOrder => S::NotImplemented,
        C::NetGatewayMultiWan | C::NetGatewayVpn => S::NotImplemented,
        C::NetNatSnat | C::NetNatDnat | C::NetNatOneToOne | C::NetNatNpt => S::NotImplemented,
        C::NetLbL4 | C::NetLbHealthCheck => S::RequiresExternalComponent { component: "the os-haproxy plugin — OPNsense 26.1.2 ships no load-balancer API" },
        C::NetDnsRecords | C::NetDnsAuthoritative => S::NotImplemented,
        C::NetIpamReservation | C::NetIpamDhcp => S::NotImplemented,
        C::NetIpamProvider => S::UnsupportedByProvider { reason: "the appliance has no IPAM beyond its DHCP servers (net.ipam.dhcp)" },
        C::NetSegmentRemote => S::UnsupportedByProvider { reason: "creating interfaces (VLAN, VXLAN, bridges) is administering the appliance, not a segment the engine owns" },
        C::NetApplyRollback => S::NotImplemented,
        C::NetObserve => S::Partial { detail: "`search_rule` and the alias read back what exists before a write; no comparison with the record yet (ADR-0059 F4)" },
        C::NetVerifyDataplane => S::NotImplemented,
        C::NetOwnershipMarker => S::Partial { detail: "every alias and rule carries a firewall category `delonix-owner:<token>`; one without it is refused, never adopted or deleted, and the commit refuses someone else's staged change — tested on the TLS mock, not yet measured against a live appliance (S6)" },
        C::NetBridge | C::NetMacvlanIpvlan | C::NetVlan | C::NetOverlayVxlan | C::NetOverlayEncrypted
        | C::NetIpam | C::NetStaticIp | C::NetDns | C::NetPublishPorts | C::NetRoutesBetweenNetworks
        | C::NetNamespaceIsolation | C::NetL7Proxy | C::NetTunnelEgress | C::NetIpv6
        | C::NetRateLimit | C::NetPacketCapture => S::UnsupportedByProvider { reason: "a feature of the engine's own SDN on the node; a perimeter appliance does not see it" },
        C::FirewallPerWorkload | C::FirewallDefaultDeny | C::FirewallSourceFiltering
        | C::FirewallEgressPolicy => S::UnsupportedByProvider { reason: "per-workload rules are the node's; a perimeter appliance answers net.gateway.* (ADR-0051)" },
        C::FirewallStateless | C::FirewallLogging | C::FirewallIcmpType => S::NotImplemented,
        C::FirewallWorkloadPeer => S::UnsupportedByProvider { reason: "the appliance does not know the engine's namespaces; a peer is refused, never expanded into a CIDR snapshot (ADR-0059 D6)" },
        C::ProviderAvailability | C::ResourceReadback | C::Events | C::AsyncOperations
        | C::VmCreate | C::VmStart | C::VmStop | C::VmDestroy | C::VmRestart | C::VmPause
        | C::VmResume | C::VmResumeSameIdentity | C::VmClone | C::VmTemplate | C::VmResizeCold
        | C::VmHotplug | C::VmExtraDisks | C::VmExtraNics | C::VmDiskResize | C::VmPciPassthrough
        | C::VmTpm | C::VmCpuModel | C::VmCpuPinning | C::VmHugepages | C::VmCloudInit
        | C::VmRestartPolicyNative | C::VmNamespaceIsolation | C::VmAntispoof | C::VmRawDefinition
        | C::SystemContainerLifecycle | C::SystemContainerOciImage | C::SystemContainerEntrypointEnv | C::SystemContainerExec | C::SystemContainerLogs | C::SystemContainerExitStatus | C::SystemContainerNetworkBridge | C::SystemContainerUnprivileged
        | C::ContainerLifecycle | C::ContainerExec | C::ContainerLogs | C::ContainerHotReconfigure
        | C::ContainerResourceLimits | C::ContainerGpuCdi | C::ContainerSeccompCustomProfile
        | C::ContainerOomDetection | C::PodSharedNetwork | C::PodSharedIpcUts | C::PodSharedPid
        | C::ContainerImages | C::VmNetworkNat | C::VmNetworkBridge | C::VmNetworkSdn
        | C::VmStaticIp | C::VolumeLocal | C::VolumeBind | C::VolumeNfs | C::VolumeCifs
        | C::VolumeWebdav | C::VolumeQuota | C::VolumeSnapshot | C::VolumeProvisionNas
        | C::StoragePools | C::StorageLvmThin | C::StorageZfsBtrfs | C::StorageCeph
        | C::VmSnapshotDisk | C::VmSnapshotMemory | C::VmSnapshotRestore | C::VmSnapshotDelete
        | C::VmSnapshotPersistent | C::VmBackupDisk | C::VmBackupQuiesced | C::VmBackupRestore
        | C::ContainerBackupRestore | C::VmMigrationCold | C::VmMigrationLive | C::VmReplication
        | C::VmHighAvailability | C::VmConsoleSerial | C::VmConsoleVnc | C::VmGuestAgent
        | C::VmIpObserved | C::MetricsPrometheus | C::MetricsPerWorkloadNetwork | C::HostHealth
        | C::HostCapacity | C::TransportVerified | C::CredentialInVault => {
            S::UnsupportedByProvider { reason: "not a network capability" }
        }
    }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_a_gateway_that_answers_every_network_row() {
        let r = capability_report(true);
        assert_eq!(r.id, "opnsense");
        assert_eq!(r.kind, ProviderKind::Gateway);
        let n = C::ALL
            .iter()
            .filter(|c| c.kind() == ProviderKind::Network)
            .count();
        assert_eq!(r.capabilities.len(), n);
        assert_eq!(r.health.reason, "NotProbed");
    }

    #[test]
    fn unconfigured_it_is_listed_and_not_selectable() {
        let r = capability_report(false);
        assert!(!r.available);
        assert_eq!(r.health.reason, "NotConfigured");
    }

    #[test]
    fn only_what_the_live_case_exercises_is_supported() {
        let r = capability_report(true);
        let mut yes: Vec<&str> = r
            .capabilities
            .iter()
            .filter(|c| c.state.label() == "supported")
            .map(|c| c.capability.name())
            .collect();
        yes.sort_unstable();
        assert_eq!(
            yes,
            [
                "net.apply.staged",
                "net.gateway.alias",
                "net.gateway.filter"
            ]
        );
    }
}

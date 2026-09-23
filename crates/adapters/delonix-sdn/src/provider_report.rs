//! The Linux provider's answer to the capability catalog for NETWORKING
//! (`delonix_compute::capability`, ADR-0050): the rootless SDN — holder
//! netns, bridge, slirp, nftables, WireGuard — as one network provider.

use delonix_compute::capability::{
    Capability as C, CapabilityState as S, HealthStatus, ProviderHealth, ProviderKind,
    ProviderReport,
};
use delonix_compute::capability_host::binary_in_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdnHost {
    pub nft: bool,
    pub slirp4netns: bool,
    pub wg: bool,
    /// `bridge-nf-call-iptables=1` where the dataplane runs. `None` = could
    /// not be asked (holder down and no host value readable).
    pub br_netfilter: Option<bool>,
    pub rootless: bool,
}

impl SdnHost {
    pub const ASSUMED: SdnHost = SdnHost {
        nft: true,
        slirp4netns: true,
        wg: true,
        br_netfilter: Some(true),
        rootless: true,
    };

    /// Reads this machine. Asks the holder for `br_netfilter` when it is up
    /// (the sysctl is per-netns, and the holder's is the one that filters);
    /// otherwise reads the host's value, which is what a fresh holder inherits.
    pub fn probe() -> SdnHost {
        let br_netfilter = match crate::infra::br_netfilter_active() {
            Ok(v) => Some(v),
            Err(_) => std::fs::read_to_string("/proc/sys/net/bridge/bridge-nf-call-iptables")
                .ok()
                .map(|s| s.trim() == "1"),
        };
        SdnHost {
            nft: binary_in_path("nft"),
            slirp4netns: binary_in_path("slirp4netns"),
            wg: binary_in_path("wg"),
            br_netfilter,
            rootless: delonix_node::is_rootless(),
        }
    }
}

pub fn report(host: &SdnHost) -> ProviderReport {
    let available = host.nft && (host.slirp4netns || !host.rootless);
    let (status, reason, message) = if !host.nft {
        (
            HealthStatus::Unavailable,
            "NftMissing",
            "nftables is not installed".to_string(),
        )
    } else if host.rootless && !host.slirp4netns {
        (
            HealthStatus::Unavailable,
            "SlirpMissing",
            "slirp4netns is not installed (rootless uplink)".to_string(),
        )
    } else if host.br_netfilter == Some(false) {
        (
            HealthStatus::Healthy,
            "BrNetfilterOff",
            "bridge-nf-call-iptables=0: namespace isolation and per-workload rules install but do not filter".to_string(),
        )
    } else if host.br_netfilter.is_none() {
        (
            HealthStatus::Unknown,
            "BrNetfilterUnknown",
            "could not read bridge-nf-call-iptables".to_string(),
        )
    } else {
        (HealthStatus::Healthy, "Ok", String::new())
    };
    let base = |s: S| {
        s.on_host(
            available,
            "nft (and slirp4netns when rootless) are required",
        )
    };
    let filt = |s: S| {
        base(s).on_host(
            host.br_netfilter != Some(false),
            "bridge-nf-call-iptables is 0: rules install and do not filter",
        )
    };
    let wg = |s: S| base(s).on_host(host.wg, "wireguard-tools not installed");
    ProviderReport::build(
        "linux",
        ProviderKind::Network,
        available,
        ProviderHealth {
            status,
            reason,
            message,
        },
        |c| {
            match c {
        C::NetBridge => base(S::Supported { evidence: "e2e:network: ciclo de vida" }),
        C::NetMacvlanIpvlan => S::UnsupportedByProvider { reason: "needs CAP_NET_ADMIN in the host's init netns; registered with Realized=False, never realised rootless" },
        C::NetVlan => S::Partial { detail: "`network vlan` puts an 802.1Q VLAN on a physical NIC — root only, and it says so on every run" },
        C::NetOverlayVxlan => base(S::Partial { detail: "VXLAN uplink + FDB peers inside the holder; inter-node forwarding needs a second node the battery does not have" }),
        C::NetOverlayEncrypted => wg(S::Partial { detail: "WireGuard peer per overlay node; peer removal proven on one node, traffic across two nodes not measured" }),
        C::NetIpam => base(S::Supported { evidence: "chaos:scen_concurrent_attach" }),
        C::NetStaticIp => base(S::Partial { detail: "`--ip` reserves in the same per-prefix IPAM ledger; unit-tested, not attached live in the battery" }),
        C::NetDns => base(S::Partial { detail: "`<name>.<ns>.delonix.internal` served by the holder; proven E2E in-session, no battery `nslookup`" }),
        C::NetPublishPorts => base(S::Supported { evidence: "check:update: publish-add a quente" }),
        C::NetRoutesBetweenNetworks => filt(S::Supported { evidence: "chaos:scen_stack_netroute" }),
        C::NetNamespaceIsolation => filt(S::Supported { evidence: "chaos:scen_namespace_isolation" }),
        C::NetL7Proxy => base(S::Partial { detail: "hyper reverse proxy in the holder, TLS termination, SIGHUP reload; proven E2E in-session, battery only lists routes" }),
        C::NetTunnelEgress => S::RequiresExternalComponent { component: "a tunnel provider (pinggy, ngrok, cloudflared) and its binary/account" },
        C::NetIpv6 => S::UnsupportedByProvider { reason: "disabled in every container netns and dropped by `table ip6` in the holder: the firewall is `table ip` (v0.37.1); `DELONIX_ENABLE_IPV6=1` is a noisy escape, not a feature" },
        C::NetRateLimit => base(S::Supported { evidence: "check:update: net-rate a quente" }),
        C::NetPacketCapture => base(S::Partial { detail: "`net capture` on a container's SDN interface; no battery" }),
        C::FirewallPerWorkload => filt(S::Supported { evidence: "e2e:container em rede custom: hot reconfig pelo ingress" }),
        C::FirewallDefaultDeny => filt(S::Partial { detail: "`policy deny` per direction with the conntrack prologue (2026-07-28); validated live, no battery check" }),
        C::FirewallSourceFiltering => filt(S::Partial { detail: "`allow <port> --from <cidr>` works on published ports for routable sources (never for a loopback client); validated live, no battery" }),
        C::FirewallEgressPolicy => filt(S::Partial { detail: "per-container and per-network egress policy; battery lists (`net egress ls`), does not block" }),
        C::ProviderAvailability | C::ResourceReadback | C::Events | C::AsyncOperations
        | C::VmCreate | C::VmStart | C::VmStop | C::VmDestroy | C::VmRestart | C::VmPause
        | C::VmResume | C::VmResumeSameIdentity | C::VmClone | C::VmTemplate | C::VmResizeCold
        | C::VmHotplug | C::VmExtraDisks | C::VmExtraNics | C::VmDiskResize | C::VmPciPassthrough
        | C::VmTpm | C::VmCpuModel | C::VmCpuPinning | C::VmHugepages | C::VmCloudInit
        | C::VmRestartPolicyNative | C::VmNamespaceIsolation | C::VmAntispoof | C::VmRawDefinition
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
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn br_netfilter_off_turns_isolation_into_a_host_no_and_keeps_the_bridge() {
        let host = SdnHost {
            br_netfilter: Some(false),
            ..SdnHost::ASSUMED
        };
        let r = report(&host);
        assert!(r.available);
        assert_eq!(r.health.reason, "BrNetfilterOff");
        let by = |c: C| {
            r.capabilities
                .iter()
                .find(|x| x.capability == c)
                .unwrap()
                .state
                .label()
        };
        assert_eq!(by(C::NetNamespaceIsolation), "unavailable-on-host");
        assert_eq!(by(C::NetBridge), "supported");
    }

    #[test]
    fn a_rootless_host_without_slirp_has_no_network_provider() {
        let host = SdnHost {
            slirp4netns: false,
            ..SdnHost::ASSUMED
        };
        let r = report(&host);
        assert!(!r.available);
        assert_eq!(r.health.reason, "SlirpMissing");
    }
}

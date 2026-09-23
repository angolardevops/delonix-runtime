//! The Linux provider's answer to the capability catalog for STORAGE
//! (`delonix_compute::capability`, ADR-0050): named volumes, bind mounts and
//! network shares mounted on the host.

use delonix_compute::capability::{
    Capability as C, CapabilityState as S, HealthStatus, ProviderHealth, ProviderKind,
    ProviderReport,
};
use delonix_compute::capability_host::binary_in_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageHost {
    /// `mount -t nfs|cifs|davfs` needs CAP_SYS_ADMIN: only a root session has it.
    pub can_mount: bool,
    pub mount_nfs: bool,
    pub mount_cifs: bool,
    pub mount_davfs: bool,
}

impl StorageHost {
    pub const ASSUMED: StorageHost = StorageHost {
        can_mount: true,
        mount_nfs: true,
        mount_cifs: true,
        mount_davfs: true,
    };

    pub fn probe() -> StorageHost {
        StorageHost {
            can_mount: !delonix_node::is_rootless(),
            mount_nfs: binary_in_path("mount.nfs") || binary_in_path("mount.nfs4"),
            mount_cifs: binary_in_path("mount.cifs"),
            mount_davfs: binary_in_path("mount.davfs"),
        }
    }
}

pub fn report(host: &StorageHost) -> ProviderReport {
    let (reason, message) = if !host.can_mount {
        (
            "NetworkSharesNeedRoot",
            "network shares (nfs/cifs/webdav) need CAP_SYS_ADMIN to mount; local volumes are unaffected".to_string(),
        )
    } else {
        ("Ok", String::new())
    };
    let share = |s: S, tool: bool, name: &str| {
        s.on_host(
            host.can_mount,
            "mount needs CAP_SYS_ADMIN (rootless session)",
        )
        .on_host(tool, &format!("{name} is not installed"))
    };
    ProviderReport::build(
        "linux",
        ProviderKind::Storage,
        true,
        ProviderHealth {
            status: HealthStatus::Healthy,
            reason,
            message,
        },
        |c| {
            match c {
        C::VolumeLocal => S::Supported { evidence: "e2e:volumes: ciclo de vida" },
        C::VolumeBind => S::Supported { evidence: "check:update: mount visível DENTRO do container" },
        C::VolumeNfs => share(S::Partial { detail: "read and written from a container against a real NAS in-session; the battery has no NFS server" }, host.mount_nfs, "mount.nfs"),
        C::VolumeCifs => share(S::Partial { detail: "same mount path as nfs with `mount -t cifs`; no battery server" }, host.mount_cifs, "mount.cifs"),
        C::VolumeWebdav => share(S::Partial { detail: "`mount -t davfs`; no battery server" }, host.mount_davfs, "mount.davfs"),
        C::VolumeQuota => S::Supported { evidence: "check:volume create --parent" },
        C::VolumeSnapshot => S::Supported { evidence: "check:snapshot create" },
        C::VolumeProvisionNas => S::RequiresExternalComponent { component: "a TrueNAS SCALE 25.x API (`spec.provision.truenas`, ADR-0009); `scen_truenas_destroy` skips without one" },
        C::StorageLvmThin => S::NotImplemented,
        C::StorageZfsBtrfs => S::NotImplemented,
        C::StorageCeph => S::RequiresExternalComponent { component: "a Ceph cluster and an RBD/CephFS driver the engine does not ship" },
        C::ProviderAvailability | C::ResourceReadback | C::Events | C::AsyncOperations
        | C::VmCreate | C::VmStart | C::VmStop | C::VmDestroy | C::VmRestart | C::VmPause
        | C::VmResume | C::VmResumeSameIdentity | C::VmClone | C::VmTemplate | C::VmResizeCold
        | C::VmHotplug | C::VmExtraDisks | C::VmExtraNics | C::VmDiskResize | C::VmPciPassthrough
        | C::VmTpm | C::VmCpuModel | C::VmCpuPinning | C::VmHugepages | C::VmCloudInit
        | C::VmRestartPolicyNative | C::VmNamespaceIsolation | C::VmAntispoof | C::VmRawDefinition
        | C::ContainerLifecycle | C::ContainerExec | C::ContainerLogs | C::ContainerHotReconfigure
        | C::ContainerResourceLimits | C::ContainerGpuCdi | C::ContainerSeccompCustomProfile
        | C::ContainerOomDetection | C::PodSharedNetwork | C::PodSharedIpcUts | C::PodSharedPid
        | C::ContainerImages | C::NetBridge | C::NetMacvlanIpvlan | C::NetVlan | C::NetOverlayVxlan
        | C::NetOverlayEncrypted | C::NetIpam | C::NetStaticIp | C::NetDns | C::NetPublishPorts
        | C::NetRoutesBetweenNetworks | C::NetNamespaceIsolation | C::NetL7Proxy | C::NetTunnelEgress
        | C::NetIpv6 | C::NetRateLimit | C::NetPacketCapture | C::VmNetworkNat | C::VmNetworkBridge
        | C::VmNetworkSdn | C::VmStaticIp | C::StoragePools | C::VmSnapshotDisk | C::VmSnapshotMemory
        | C::VmSnapshotRestore | C::VmSnapshotDelete | C::VmSnapshotPersistent | C::VmBackupDisk
        | C::VmBackupQuiesced | C::VmBackupRestore | C::ContainerBackupRestore | C::VmMigrationCold
        | C::VmMigrationLive | C::VmReplication | C::VmHighAvailability | C::FirewallPerWorkload
        | C::FirewallDefaultDeny | C::FirewallSourceFiltering | C::FirewallEgressPolicy
        | C::VmConsoleSerial | C::VmConsoleVnc | C::VmGuestAgent | C::VmIpObserved
        | C::MetricsPrometheus | C::MetricsPerWorkloadNetwork | C::HostHealth | C::HostCapacity
        | C::TransportVerified | C::CredentialInVault => {
            S::UnsupportedByProvider { reason: "not a storage capability" }
        }
    }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rootless_session_keeps_local_volumes_and_loses_network_shares() {
        let host = StorageHost {
            can_mount: false,
            ..StorageHost::ASSUMED
        };
        let r = report(&host);
        let by = |c: C| {
            r.capabilities
                .iter()
                .find(|x| x.capability == c)
                .unwrap()
                .state
                .label()
        };
        assert_eq!(by(C::VolumeLocal), "supported");
        assert_eq!(by(C::VolumeNfs), "unavailable-on-host");
        assert_eq!(r.health.reason, "NetworkSharesNeedRoot");
    }
}

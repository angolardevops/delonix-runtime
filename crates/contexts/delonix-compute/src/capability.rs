//! The versioned capability catalog — the DENOMINATOR behind any "this provider
//! covers X" claim about libvirt, Cloud Hypervisor, Proxmox or the Linux host.
//!
//! The Proxmox coverage matrix (ADR-0049) has a denominator the vendor
//! publishes: the API schema every node serves. libvirt and the Linux kernel
//! have no such list that maps onto what an operator asks an engine for — a
//! count of `virsh` subcommands or of syscalls would be a number without a
//! meaning. So the denominator here is the engine's OWN list of what a
//! provider may be asked to do, written once, versioned, and read by every
//! provider (ADR-0050). A provider classifies EVERY entry, and the compiler
//! makes "forgot one" impossible: a report is built by walking [`Capability::ALL`],
//! and the state of each entry is a `match` with no wildcard arm.
//!
//! Three things this module deliberately is not:
//!
//! * **Not a feature flag.** Nothing here changes what a provider does. It
//!   describes it, so that the CLI, the node contract (`ProviderInfo` in
//!   `proto/delonix/node/v1/common.proto`) and a document say the same thing
//!   from the same source.
//! * **Not self-certifying.** [`CapabilityState::Supported`] carries an
//!   `evidence` string that names a test or a battery section, and a test in
//!   the CLI crate checks that the evidence exists in the repository. A
//!   capability nobody has exercised is [`CapabilityState::Partial`] at best.
//! * **Not a lowest common denominator.** A capability one provider has and
//!   another lacks is declared by both — one as supported, the other as
//!   [`CapabilityState::UnsupportedByProvider`] with the reason written. The
//!   contract's rule (`Capability` in `common.proto`) is that a request needing
//!   a capability the selected provider lacks is refused by name, never
//!   accepted and dropped.
//!
//! The catalog is versioned by [`CATALOG_VERSION`]: adding an entry bumps the
//! minor, renaming or removing one bumps the major. A name, once published,
//! is what scripts branch on — it does not change meaning.

use serde::Serialize;

/// Version of the catalog itself (not of the engine). See the module docs for
/// what bumps which part.
pub const CATALOG_VERSION: &str = "1.3.0";

/// Which port a capability belongs to — the words the node contract's
/// `ProviderInfo.kind` uses. `Gateway` (catalog 1.1.0, ADR-0059 D2) is a
/// provider reached over the network that enforces policy at a boundary the
/// workloads' traffic crosses, not on the node — a perimeter appliance. It owns
/// no catalog entry of its own: its report walks the `Network` entries, so a
/// node's network provider and a perimeter appliance compare row by row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Compute,
    Network,
    Storage,
    Image,
    Gateway,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Compute => "compute",
            ProviderKind::Network => "network",
            ProviderKind::Storage => "storage",
            ProviderKind::Image => "image",
            ProviderKind::Gateway => "gateway",
        }
    }

    /// The kind of catalog entry a report of this kind walks: its own, except
    /// `Gateway`, which answers the `Network` rows (see the type's docs).
    pub fn catalog_kind(self) -> ProviderKind {
        match self {
            ProviderKind::Gateway => ProviderKind::Network,
            k => k,
        }
    }
}

/// The area of the operator's question — the rows of the matrix a reader
/// compares providers by. These are the engine's domains, not a vendor's UI
/// tabs: "Ceph", "ACME" or "Notifications" are not domains here, and an
/// operation that needs one of those is a capability whose state says which
/// external component it requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Domain {
    /// Discovering the provider and the host: availability, health, events.
    Inventory,
    /// The VM lifecycle and its shape (CPU, memory, disks, devices).
    VmCompute,
    /// Containers and pods.
    Containers,
    /// System containers: a whole OS userland run as one unit with VM-like
    /// semantics (catalog 1.2.0, ADR-0058). Not a `Container`: no `exec`,
    /// logs or exit status are promised by the domain itself.
    SystemContainers,
    /// Networks, addressing, isolation and exposure.
    Network,
    /// Volumes, disks and pools.
    Storage,
    /// Snapshots, checkpoints, backup and restore.
    Protection,
    /// Moving or replicating a workload between hosts; high availability.
    Mobility,
    /// Firewalling and traffic policy.
    Firewall,
    /// Console access and guest-side agents.
    Console,
    /// Metrics and observability of the provider's resources.
    Metrics,
    /// Identity and access to the provider's own transport.
    Access,
}

impl Domain {
    pub fn as_str(self) -> &'static str {
        match self {
            Domain::Inventory => "inventory",
            Domain::VmCompute => "vm-compute",
            Domain::Containers => "containers",
            Domain::SystemContainers => "system-containers",
            Domain::Network => "network",
            Domain::Storage => "storage",
            Domain::Protection => "protection",
            Domain::Mobility => "mobility",
            Domain::Firewall => "firewall",
            Domain::Console => "console",
            Domain::Metrics => "metrics",
            Domain::Access => "access",
        }
    }
}

/// One entry of the catalog. The variant is the identity; [`Capability::name`]
/// is the stable string that leaves the engine (CLI JSON, `ProviderInfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    // --- inventory ------------------------------------------------------
    /// The provider can say whether it is usable on this host without side effects.
    ProviderAvailability,
    /// The provider reports the state of what it manages (list + readback).
    ResourceReadback,
    /// Lifecycle events are emitted as they happen (not derived by polling).
    Events,
    /// Long operations are tracked as jobs with a terminal verdict.
    AsyncOperations,

    // --- vm compute -----------------------------------------------------
    VmCreate,
    VmStart,
    /// Stop keeps the disk, the record and the snapshots.
    VmStop,
    /// Destroy releases everything the VM owns.
    VmDestroy,
    VmRestart,
    /// Suspend the vCPUs with guest memory intact.
    VmPause,
    VmResume,
    /// Bring a STOPPED VM back with the same identity and disks (no second VM).
    VmResumeSameIdentity,
    VmClone,
    VmTemplate,
    /// Change CPU/memory of a stopped VM from the record.
    VmResizeCold,
    /// Adding a vCPU to a RUNNING VM (ADR-0068 D11). Split from `.remove`:
    /// the hypervisor accepting a removal has been measured to not mean the
    /// guest actually released it (ADR-0068 finding 3).
    VmHotplugCpuAdd,
    VmHotplugCpuRemove,
    /// Adding memory to a RUNNING VM (ADR-0068 D11).
    VmHotplugMemoryAdd,
    VmHotplugMemoryRemove,
    /// Attaching a disk to a RUNNING VM (ADR-0068 D11).
    VmHotplugDiskAdd,
    VmHotplugDiskRemove,
    /// Attaching a NIC to a RUNNING VM (ADR-0068 D11).
    VmHotplugNicAdd,
    VmHotplugNicRemove,
    VmExtraDisks,
    VmExtraNics,
    VmDiskResize,
    /// PCI/VFIO device passthrough.
    VmPciPassthrough,
    VmTpm,
    VmCpuModel,
    VmCpuPinning,
    VmHugepages,
    /// cloud-init intent fields (hostname, ssh keys, user-data) realised by the provider.
    VmCloudInit,
    /// A restart policy the provider enforces itself (no engine supervisor).
    VmRestartPolicyNative,
    /// The engine's isolation namespace applies to the VM's address.
    VmNamespaceIsolation,
    /// Anti-spoofing of the VM's source address at the hypervisor.
    VmAntispoof,
    /// A raw hypervisor definition escape hatch (trusted callers only).
    VmRawDefinition,

    // --- containers -----------------------------------------------------
    ContainerLifecycle,
    ContainerExec,
    ContainerLogs,
    /// Ports, volumes, networks, limits changed with the PID unchanged.
    ContainerHotReconfigure,
    ContainerResourceLimits,
    ContainerGpuCdi,
    ContainerSeccompCustomProfile,
    ContainerOomDetection,
    PodSharedNetwork,
    PodSharedIpcUts,
    PodSharedPid,
    /// Container image pull/build/scan (the image port, reported by the compute provider that consumes it).
    ContainerImages,

    // --- system containers (catalog 1.2.0, ADR-0058) ----------------------
    /// Create, start, stop and destroy a system container.
    SystemContainerLifecycle,
    /// Created from an OCI image the engine pulled and verified.
    SystemContainerOciImage,
    /// The entrypoint and environment asked for are the ones the provider
    /// kept, read back after the create.
    SystemContainerEntrypointEnv,
    SystemContainerExec,
    SystemContainerLogs,
    /// The exit status of the container's entrypoint.
    SystemContainerExitStatus,
    /// Attached to a bridge (and VLAN) of the host that runs it.
    SystemContainerNetworkBridge,
    /// Runs in a user namespace, never as the host's root.
    SystemContainerUnprivileged,
    /// Snapshots of the root volume, listed, deleted and rolled back to
    /// (catalog 1.3.0, plan 63 slice 5).
    SystemContainerSnapshot,
    /// Memory, swap and cores changed on a running container, and the root
    /// volume grown, without recreating it (catalog 1.3.0, plan 63 slice 5).
    SystemContainerResize,
    /// An archive of the container taken on the provider's storage, listed,
    /// deleted and restored over the container (catalog 1.3.0, plan 63 slice 5).
    SystemContainerBackup,
    /// A full copy of the container under a new name, running or not
    /// (catalog 1.3.0, plan 63 slice 5).
    SystemContainerClone,
    /// The container's own firewall on the provider, from a `NetworkPolicy`
    /// with `scope: systemcontainer` (catalog 1.3.0, plan 63 slice 5).
    SystemContainerFirewall,
    /// The container moved to another node of the provider's cluster; a
    /// running one only by stopping it (catalog 1.3.0, plan 63 slice 5).
    SystemContainerMove,

    // --- network --------------------------------------------------------
    NetBridge,
    NetMacvlanIpvlan,
    NetVlan,
    NetOverlayVxlan,
    NetOverlayEncrypted,
    NetIpam,
    NetStaticIp,
    NetDns,
    NetPublishPorts,
    NetRoutesBetweenNetworks,
    NetNamespaceIsolation,
    NetL7Proxy,
    NetTunnelEgress,
    NetIpv6,
    NetRateLimit,
    NetPacketCapture,
    /// Perimeter filter rules on a boundary the workloads' traffic crosses (ADR-0059).
    NetGatewayFilter,
    /// Named address sets a perimeter rule refers to.
    NetGatewayAlias,
    /// A perimeter rule or alias is changed where it is, not removed and re-created.
    NetGatewayUpdateInPlace,
    /// The engine sets the exact evaluation order of perimeter rules.
    NetGatewayRuleOrder,
    /// More than one uplink, with gateway groups or failover.
    NetGatewayMultiWan,
    /// VPN termination at the boundary.
    NetGatewayVpn,
    /// Source NAT (masquerade) of a segment's traffic.
    NetNatSnat,
    /// Destination NAT (port forward) to a workload.
    NetNatDnat,
    /// One external address mapped to one internal address.
    NetNatOneToOne,
    /// IPv6 network prefix translation.
    NetNatNpt,
    /// A layer-4 load balancer: a virtual address in front of a backend set.
    NetLbL4,
    /// Backends are taken out of rotation by a health check.
    NetLbHealthCheck,
    /// Records written in a provider's DNS (not the engine's own resolver, `net.dns`).
    NetDnsRecords,
    /// An authoritative zone served by the provider.
    NetDnsAuthoritative,
    /// Addresses allocated in a provider's IPAM (not the engine's own, `net.ipam`).
    NetIpamProvider,
    /// A fixed address reserved per MAC in a provider's IPAM.
    NetIpamReservation,
    /// A DHCP range served by the provider.
    NetIpamDhcp,
    /// A segment realized by a remote provider (zone and VNets).
    NetSegmentRemote,
    /// Changes are staged, then activated in one step.
    NetApplyStaged,
    /// Staged changes are discarded before activation when a step fails.
    NetApplyRollback,
    /// The actual state is read back so a plan can compare it with the record.
    NetObserve,
    /// An apply is verified by a traffic probe, not only by a readback.
    NetVerifyDataplane,
    /// Objects the engine writes carry an immutable marker; one without it is never adopted by name.
    NetOwnershipMarker,
    /// A NAT network provided by the hypervisor (libvirt `nat`), addresses observed by lease.
    VmNetworkNat,
    /// A host bridge the VM's NIC is enslaved to.
    VmNetworkBridge,
    /// The VM's NIC lives on the engine's own SDN (same L2 as containers).
    VmNetworkSdn,
    VmStaticIp,

    // --- storage --------------------------------------------------------
    VolumeLocal,
    VolumeBind,
    VolumeNfs,
    VolumeCifs,
    VolumeWebdav,
    VolumeQuota,
    VolumeSnapshot,
    VolumeProvisionNas,
    /// Storage pools/volumes managed by the provider (libvirt pools, Proxmox storage).
    StoragePools,
    StorageLvmThin,
    StorageZfsBtrfs,
    StorageCeph,

    // --- protection -----------------------------------------------------
    VmSnapshotDisk,
    VmSnapshotMemory,
    VmSnapshotRestore,
    VmSnapshotDelete,
    /// Snapshots survive the provider forgetting the domain (stop/start).
    VmSnapshotPersistent,
    VmBackupDisk,
    /// The backup is taken with the guest filesystem frozen.
    VmBackupQuiesced,
    VmBackupRestore,
    ContainerBackupRestore,

    // --- mobility -------------------------------------------------------
    VmMigrationCold,
    VmMigrationLive,
    VmReplication,
    VmHighAvailability,

    // --- firewall -------------------------------------------------------
    FirewallPerWorkload,
    FirewallDefaultDeny,
    FirewallSourceFiltering,
    FirewallEgressPolicy,
    /// A rule that bypasses connection tracking.
    FirewallStateless,
    /// A rule logs the packets it matches.
    FirewallLogging,
    /// A rule matches an ICMP type and code.
    FirewallIcmpType,
    /// A rule's peer is named by namespace or selector, not by a CIDR.
    FirewallWorkloadPeer,

    // --- console --------------------------------------------------------
    VmConsoleSerial,
    VmConsoleVnc,
    VmGuestAgent,
    /// The address reported was observed (lease/agent), not computed.
    VmIpObserved,

    // --- metrics --------------------------------------------------------
    MetricsPrometheus,
    MetricsPerWorkloadNetwork,
    HostHealth,
    HostCapacity,

    // --- access ---------------------------------------------------------
    /// The provider's transport is verified (TLS) or local-only.
    TransportVerified,
    /// The credential the engine holds for the provider is stored in the engine's vault.
    CredentialInVault,
}

impl Capability {
    /// Every entry, in matrix order. The single list every report walks.
    pub const ALL: &'static [Capability] = &[
        Self::ProviderAvailability,
        Self::ResourceReadback,
        Self::Events,
        Self::AsyncOperations,
        Self::VmCreate,
        Self::VmStart,
        Self::VmStop,
        Self::VmDestroy,
        Self::VmRestart,
        Self::VmPause,
        Self::VmResume,
        Self::VmResumeSameIdentity,
        Self::VmClone,
        Self::VmTemplate,
        Self::VmResizeCold,
        Self::VmHotplugCpuAdd,
        Self::VmHotplugCpuRemove,
        Self::VmHotplugMemoryAdd,
        Self::VmHotplugMemoryRemove,
        Self::VmHotplugDiskAdd,
        Self::VmHotplugDiskRemove,
        Self::VmHotplugNicAdd,
        Self::VmHotplugNicRemove,
        Self::VmExtraDisks,
        Self::VmExtraNics,
        Self::VmDiskResize,
        Self::VmPciPassthrough,
        Self::VmTpm,
        Self::VmCpuModel,
        Self::VmCpuPinning,
        Self::VmHugepages,
        Self::VmCloudInit,
        Self::VmRestartPolicyNative,
        Self::VmNamespaceIsolation,
        Self::VmAntispoof,
        Self::VmRawDefinition,
        Self::ContainerLifecycle,
        Self::ContainerExec,
        Self::ContainerLogs,
        Self::ContainerHotReconfigure,
        Self::ContainerResourceLimits,
        Self::ContainerGpuCdi,
        Self::ContainerSeccompCustomProfile,
        Self::ContainerOomDetection,
        Self::PodSharedNetwork,
        Self::PodSharedIpcUts,
        Self::PodSharedPid,
        Self::ContainerImages,
        Self::SystemContainerLifecycle,
        Self::SystemContainerOciImage,
        Self::SystemContainerEntrypointEnv,
        Self::SystemContainerExec,
        Self::SystemContainerLogs,
        Self::SystemContainerExitStatus,
        Self::SystemContainerNetworkBridge,
        Self::SystemContainerUnprivileged,
        Self::SystemContainerSnapshot,
        Self::SystemContainerResize,
        Self::SystemContainerBackup,
        Self::SystemContainerClone,
        Self::SystemContainerFirewall,
        Self::SystemContainerMove,
        Self::NetBridge,
        Self::NetMacvlanIpvlan,
        Self::NetVlan,
        Self::NetOverlayVxlan,
        Self::NetOverlayEncrypted,
        Self::NetIpam,
        Self::NetStaticIp,
        Self::NetDns,
        Self::NetPublishPorts,
        Self::NetRoutesBetweenNetworks,
        Self::NetNamespaceIsolation,
        Self::NetL7Proxy,
        Self::NetTunnelEgress,
        Self::NetIpv6,
        Self::NetRateLimit,
        Self::NetPacketCapture,
        Self::NetGatewayFilter,
        Self::NetGatewayAlias,
        Self::NetGatewayUpdateInPlace,
        Self::NetGatewayRuleOrder,
        Self::NetGatewayMultiWan,
        Self::NetGatewayVpn,
        Self::NetNatSnat,
        Self::NetNatDnat,
        Self::NetNatOneToOne,
        Self::NetNatNpt,
        Self::NetLbL4,
        Self::NetLbHealthCheck,
        Self::NetDnsRecords,
        Self::NetDnsAuthoritative,
        Self::NetIpamProvider,
        Self::NetIpamReservation,
        Self::NetIpamDhcp,
        Self::NetSegmentRemote,
        Self::NetApplyStaged,
        Self::NetApplyRollback,
        Self::NetObserve,
        Self::NetVerifyDataplane,
        Self::NetOwnershipMarker,
        Self::VmNetworkNat,
        Self::VmNetworkBridge,
        Self::VmNetworkSdn,
        Self::VmStaticIp,
        Self::VolumeLocal,
        Self::VolumeBind,
        Self::VolumeNfs,
        Self::VolumeCifs,
        Self::VolumeWebdav,
        Self::VolumeQuota,
        Self::VolumeSnapshot,
        Self::VolumeProvisionNas,
        Self::StoragePools,
        Self::StorageLvmThin,
        Self::StorageZfsBtrfs,
        Self::StorageCeph,
        Self::VmSnapshotDisk,
        Self::VmSnapshotMemory,
        Self::VmSnapshotRestore,
        Self::VmSnapshotDelete,
        Self::VmSnapshotPersistent,
        Self::VmBackupDisk,
        Self::VmBackupQuiesced,
        Self::VmBackupRestore,
        Self::ContainerBackupRestore,
        Self::VmMigrationCold,
        Self::VmMigrationLive,
        Self::VmReplication,
        Self::VmHighAvailability,
        Self::FirewallPerWorkload,
        Self::FirewallDefaultDeny,
        Self::FirewallSourceFiltering,
        Self::FirewallEgressPolicy,
        Self::FirewallStateless,
        Self::FirewallLogging,
        Self::FirewallIcmpType,
        Self::FirewallWorkloadPeer,
        Self::VmConsoleSerial,
        Self::VmConsoleVnc,
        Self::VmGuestAgent,
        Self::VmIpObserved,
        Self::MetricsPrometheus,
        Self::MetricsPerWorkloadNetwork,
        Self::HostHealth,
        Self::HostCapacity,
        Self::TransportVerified,
        Self::CredentialInVault,
    ];

    /// The stable dotted name. Published; scripts branch on it.
    pub fn name(self) -> &'static str {
        match self {
            Self::ProviderAvailability => "provider.availability",
            Self::ResourceReadback => "provider.readback",
            Self::Events => "provider.events",
            Self::AsyncOperations => "provider.async-operations",
            Self::VmCreate => "vm.create",
            Self::VmStart => "vm.start",
            Self::VmStop => "vm.stop",
            Self::VmDestroy => "vm.destroy",
            Self::VmRestart => "vm.restart",
            Self::VmPause => "vm.pause",
            Self::VmResume => "vm.resume",
            Self::VmResumeSameIdentity => "vm.resume-same-identity",
            Self::VmClone => "vm.clone",
            Self::VmTemplate => "vm.template",
            Self::VmResizeCold => "vm.resize.cold",
            Self::VmHotplugCpuAdd => "vm.hotplug.cpu.add",
            Self::VmHotplugCpuRemove => "vm.hotplug.cpu.remove",
            Self::VmHotplugMemoryAdd => "vm.hotplug.memory.add",
            Self::VmHotplugMemoryRemove => "vm.hotplug.memory.remove",
            Self::VmHotplugDiskAdd => "vm.hotplug.disk.add",
            Self::VmHotplugDiskRemove => "vm.hotplug.disk.remove",
            Self::VmHotplugNicAdd => "vm.hotplug.nic.add",
            Self::VmHotplugNicRemove => "vm.hotplug.nic.remove",
            Self::VmExtraDisks => "vm.disks.extra",
            Self::VmExtraNics => "vm.nics.extra",
            Self::VmDiskResize => "vm.disk.resize",
            Self::VmPciPassthrough => "vm.device.pci-passthrough",
            Self::VmTpm => "vm.device.tpm",
            Self::VmCpuModel => "vm.cpu.model",
            Self::VmCpuPinning => "vm.cpu.pinning",
            Self::VmHugepages => "vm.memory.hugepages",
            Self::VmCloudInit => "vm.cloud-init",
            Self::VmRestartPolicyNative => "vm.restart-policy.native",
            Self::VmNamespaceIsolation => "vm.namespace-isolation",
            Self::VmAntispoof => "vm.antispoof",
            Self::VmRawDefinition => "vm.raw-definition",
            Self::ContainerLifecycle => "container.lifecycle",
            Self::ContainerExec => "container.exec",
            Self::ContainerLogs => "container.logs",
            Self::ContainerHotReconfigure => "container.hot-reconfigure",
            Self::ContainerResourceLimits => "container.resource-limits",
            Self::ContainerGpuCdi => "container.gpu.cdi",
            Self::ContainerSeccompCustomProfile => "container.seccomp.custom-profile",
            Self::ContainerOomDetection => "container.oom-detection",
            Self::PodSharedNetwork => "pod.shared-network",
            Self::PodSharedIpcUts => "pod.shared-ipc-uts",
            Self::PodSharedPid => "pod.shared-pid",
            Self::ContainerImages => "container.images",
            Self::SystemContainerLifecycle => "system-container.lifecycle",
            Self::SystemContainerOciImage => "system-container.oci-image",
            Self::SystemContainerEntrypointEnv => "system-container.entrypoint-env",
            Self::SystemContainerExec => "system-container.exec",
            Self::SystemContainerLogs => "system-container.logs",
            Self::SystemContainerExitStatus => "system-container.exit-status",
            Self::SystemContainerNetworkBridge => "system-container.network.bridge",
            Self::SystemContainerUnprivileged => "system-container.unprivileged",
            Self::SystemContainerSnapshot => "system-container.snapshot",
            Self::SystemContainerResize => "system-container.resize",
            Self::SystemContainerBackup => "system-container.backup",
            Self::SystemContainerClone => "system-container.clone",
            Self::SystemContainerFirewall => "system-container.firewall",
            Self::SystemContainerMove => "system-container.move",
            Self::NetBridge => "net.bridge",
            Self::NetMacvlanIpvlan => "net.macvlan-ipvlan",
            Self::NetVlan => "net.vlan",
            Self::NetOverlayVxlan => "net.overlay.vxlan",
            Self::NetOverlayEncrypted => "net.overlay.encrypted",
            Self::NetIpam => "net.ipam",
            Self::NetStaticIp => "net.static-ip",
            Self::NetDns => "net.dns",
            Self::NetPublishPorts => "net.publish-ports",
            Self::NetRoutesBetweenNetworks => "net.routes",
            Self::NetNamespaceIsolation => "net.namespace-isolation",
            Self::NetL7Proxy => "net.l7-proxy",
            Self::NetTunnelEgress => "net.tunnel",
            Self::NetIpv6 => "net.ipv6",
            Self::NetRateLimit => "net.rate-limit",
            Self::NetPacketCapture => "net.capture",
            Self::NetGatewayFilter => "net.gateway.filter",
            Self::NetGatewayAlias => "net.gateway.alias",
            Self::NetGatewayUpdateInPlace => "net.gateway.update-in-place",
            Self::NetGatewayRuleOrder => "net.gateway.rule-order",
            Self::NetGatewayMultiWan => "net.gateway.multi-wan",
            Self::NetGatewayVpn => "net.gateway.vpn",
            Self::NetNatSnat => "net.nat.snat",
            Self::NetNatDnat => "net.nat.dnat",
            Self::NetNatOneToOne => "net.nat.one-to-one",
            Self::NetNatNpt => "net.nat.npt",
            Self::NetLbL4 => "net.lb.l4",
            Self::NetLbHealthCheck => "net.lb.health-check",
            Self::NetDnsRecords => "net.dns.records",
            Self::NetDnsAuthoritative => "net.dns.authoritative",
            Self::NetIpamProvider => "net.ipam.provider",
            Self::NetIpamReservation => "net.ipam.reservation",
            Self::NetIpamDhcp => "net.ipam.dhcp",
            Self::NetSegmentRemote => "net.segment.remote",
            Self::NetApplyStaged => "net.apply.staged",
            Self::NetApplyRollback => "net.apply.rollback",
            Self::NetObserve => "net.observe",
            Self::NetVerifyDataplane => "net.verify.dataplane",
            Self::NetOwnershipMarker => "net.ownership-marker",
            Self::VmNetworkNat => "vm.network.nat",
            Self::VmNetworkBridge => "vm.network.bridge",
            Self::VmNetworkSdn => "vm.network.sdn",
            Self::VmStaticIp => "vm.network.static-ip",
            Self::VolumeLocal => "volume.local",
            Self::VolumeBind => "volume.bind",
            Self::VolumeNfs => "volume.nfs",
            Self::VolumeCifs => "volume.cifs",
            Self::VolumeWebdav => "volume.webdav",
            Self::VolumeQuota => "volume.quota",
            Self::VolumeSnapshot => "volume.snapshot",
            Self::VolumeProvisionNas => "volume.provision.nas",
            Self::StoragePools => "storage.pools",
            Self::StorageLvmThin => "storage.lvm-thin",
            Self::StorageZfsBtrfs => "storage.zfs-btrfs",
            Self::StorageCeph => "storage.ceph",
            Self::VmSnapshotDisk => "vm.snapshot.disk",
            Self::VmSnapshotMemory => "vm.snapshot.memory",
            Self::VmSnapshotRestore => "vm.snapshot.restore",
            Self::VmSnapshotDelete => "vm.snapshot.delete",
            Self::VmSnapshotPersistent => "vm.snapshot.persistent",
            Self::VmBackupDisk => "vm.backup.disk",
            Self::VmBackupQuiesced => "vm.backup.quiesced",
            Self::VmBackupRestore => "vm.backup.restore",
            Self::ContainerBackupRestore => "container.backup-restore",
            Self::VmMigrationCold => "vm.migration.cold",
            Self::VmMigrationLive => "vm.migration.live",
            Self::VmReplication => "vm.replication",
            Self::VmHighAvailability => "vm.high-availability",
            Self::FirewallPerWorkload => "firewall.per-workload",
            Self::FirewallDefaultDeny => "firewall.default-deny",
            Self::FirewallSourceFiltering => "firewall.source-filtering",
            Self::FirewallEgressPolicy => "firewall.egress-policy",
            Self::FirewallStateless => "firewall.stateless",
            Self::FirewallLogging => "firewall.logging",
            Self::FirewallIcmpType => "firewall.icmp-type",
            Self::FirewallWorkloadPeer => "firewall.workload-peer",
            Self::VmConsoleSerial => "vm.console.serial",
            Self::VmConsoleVnc => "vm.console.vnc",
            Self::VmGuestAgent => "vm.guest-agent",
            Self::VmIpObserved => "vm.ip.observed",
            Self::MetricsPrometheus => "metrics.prometheus",
            Self::MetricsPerWorkloadNetwork => "metrics.network.per-workload",
            Self::HostHealth => "host.health",
            Self::HostCapacity => "host.capacity",
            Self::TransportVerified => "access.transport-verified",
            Self::CredentialInVault => "access.credential-in-vault",
        }
    }

    /// Which kind of provider is asked this question.
    pub fn kind(self) -> ProviderKind {
        use Capability::*;
        match self {
            ProviderAvailability | ResourceReadback | Events | AsyncOperations => {
                ProviderKind::Compute
            }
            VmCreate
            | VmStart
            | VmStop
            | VmDestroy
            | VmRestart
            | VmPause
            | VmResume
            | VmResumeSameIdentity
            | VmClone
            | VmTemplate
            | VmResizeCold
            | VmHotplugCpuAdd
            | VmHotplugCpuRemove
            | VmHotplugMemoryAdd
            | VmHotplugMemoryRemove
            | VmHotplugDiskAdd
            | VmHotplugDiskRemove
            | VmHotplugNicAdd
            | VmHotplugNicRemove
            | VmExtraDisks
            | VmExtraNics
            | VmDiskResize
            | VmPciPassthrough
            | VmTpm
            | VmCpuModel
            | VmCpuPinning
            | VmHugepages
            | VmCloudInit
            | VmRestartPolicyNative
            | VmNamespaceIsolation
            | VmAntispoof
            | VmRawDefinition => ProviderKind::Compute,
            ContainerLifecycle
            | ContainerExec
            | ContainerLogs
            | ContainerHotReconfigure
            | ContainerResourceLimits
            | ContainerGpuCdi
            | ContainerSeccompCustomProfile
            | ContainerOomDetection
            | PodSharedNetwork
            | PodSharedIpcUts
            | PodSharedPid
            | ContainerImages => ProviderKind::Compute,
            SystemContainerLifecycle
            | SystemContainerOciImage
            | SystemContainerEntrypointEnv
            | SystemContainerExec
            | SystemContainerLogs
            | SystemContainerExitStatus
            | SystemContainerNetworkBridge
            | SystemContainerUnprivileged
            | SystemContainerSnapshot
            | SystemContainerResize
            | SystemContainerBackup
            | SystemContainerClone
            | SystemContainerFirewall
            | SystemContainerMove => ProviderKind::Compute,
            NetBridge
            | NetMacvlanIpvlan
            | NetVlan
            | NetOverlayVxlan
            | NetOverlayEncrypted
            | NetIpam
            | NetStaticIp
            | NetDns
            | NetPublishPorts
            | NetRoutesBetweenNetworks
            | NetNamespaceIsolation
            | NetL7Proxy
            | NetTunnelEgress
            | NetIpv6
            | NetRateLimit
            | NetPacketCapture
            | NetGatewayFilter
            | NetGatewayAlias
            | NetGatewayUpdateInPlace
            | NetGatewayRuleOrder
            | NetGatewayMultiWan
            | NetGatewayVpn
            | NetNatSnat
            | NetNatDnat
            | NetNatOneToOne
            | NetNatNpt
            | NetLbL4
            | NetLbHealthCheck
            | NetDnsRecords
            | NetDnsAuthoritative
            | NetIpamProvider
            | NetIpamReservation
            | NetIpamDhcp
            | NetSegmentRemote
            | NetApplyStaged
            | NetApplyRollback
            | NetObserve
            | NetVerifyDataplane
            | NetOwnershipMarker => ProviderKind::Network,
            VmNetworkNat | VmNetworkBridge | VmNetworkSdn | VmStaticIp => ProviderKind::Compute,
            VolumeLocal | VolumeBind | VolumeNfs | VolumeCifs | VolumeWebdav | VolumeQuota
            | VolumeSnapshot | VolumeProvisionNas | StorageLvmThin | StorageZfsBtrfs
            | StorageCeph => ProviderKind::Storage,
            StoragePools => ProviderKind::Compute,
            VmSnapshotDisk
            | VmSnapshotMemory
            | VmSnapshotRestore
            | VmSnapshotDelete
            | VmSnapshotPersistent
            | VmBackupDisk
            | VmBackupQuiesced
            | VmBackupRestore
            | ContainerBackupRestore => ProviderKind::Compute,
            VmMigrationCold | VmMigrationLive | VmReplication | VmHighAvailability => {
                ProviderKind::Compute
            }
            FirewallPerWorkload
            | FirewallDefaultDeny
            | FirewallSourceFiltering
            | FirewallEgressPolicy
            | FirewallStateless
            | FirewallLogging
            | FirewallIcmpType
            | FirewallWorkloadPeer => ProviderKind::Network,
            VmConsoleSerial | VmConsoleVnc | VmGuestAgent | VmIpObserved => ProviderKind::Compute,
            MetricsPrometheus | MetricsPerWorkloadNetwork | HostHealth | HostCapacity => {
                ProviderKind::Compute
            }
            TransportVerified | CredentialInVault => ProviderKind::Compute,
        }
    }

    /// The matrix row.
    pub fn domain(self) -> Domain {
        use Capability::*;
        match self {
            ProviderAvailability | ResourceReadback | Events | AsyncOperations => Domain::Inventory,
            VmCreate
            | VmStart
            | VmStop
            | VmDestroy
            | VmRestart
            | VmPause
            | VmResume
            | VmResumeSameIdentity
            | VmClone
            | VmTemplate
            | VmResizeCold
            | VmHotplugCpuAdd
            | VmHotplugCpuRemove
            | VmHotplugMemoryAdd
            | VmHotplugMemoryRemove
            | VmHotplugDiskAdd
            | VmHotplugDiskRemove
            | VmHotplugNicAdd
            | VmHotplugNicRemove
            | VmExtraDisks
            | VmExtraNics
            | VmDiskResize
            | VmPciPassthrough
            | VmTpm
            | VmCpuModel
            | VmCpuPinning
            | VmHugepages
            | VmCloudInit
            | VmRestartPolicyNative
            | VmNamespaceIsolation
            | VmAntispoof
            | VmRawDefinition => Domain::VmCompute,
            ContainerLifecycle
            | ContainerExec
            | ContainerLogs
            | ContainerHotReconfigure
            | ContainerResourceLimits
            | ContainerGpuCdi
            | ContainerSeccompCustomProfile
            | ContainerOomDetection
            | PodSharedNetwork
            | PodSharedIpcUts
            | PodSharedPid
            | ContainerImages => Domain::Containers,
            SystemContainerLifecycle
            | SystemContainerOciImage
            | SystemContainerEntrypointEnv
            | SystemContainerExec
            | SystemContainerLogs
            | SystemContainerExitStatus
            | SystemContainerNetworkBridge
            | SystemContainerUnprivileged
            | SystemContainerSnapshot
            | SystemContainerResize
            | SystemContainerBackup
            | SystemContainerClone
            | SystemContainerFirewall
            | SystemContainerMove => Domain::SystemContainers,
            NetBridge
            | NetMacvlanIpvlan
            | NetVlan
            | NetOverlayVxlan
            | NetOverlayEncrypted
            | NetIpam
            | NetStaticIp
            | NetDns
            | NetPublishPorts
            | NetRoutesBetweenNetworks
            | NetNamespaceIsolation
            | NetL7Proxy
            | NetTunnelEgress
            | NetIpv6
            | NetRateLimit
            | NetPacketCapture
            | NetGatewayFilter
            | NetGatewayAlias
            | NetGatewayUpdateInPlace
            | NetGatewayRuleOrder
            | NetGatewayMultiWan
            | NetGatewayVpn
            | NetNatSnat
            | NetNatDnat
            | NetNatOneToOne
            | NetNatNpt
            | NetLbL4
            | NetLbHealthCheck
            | NetDnsRecords
            | NetDnsAuthoritative
            | NetIpamProvider
            | NetIpamReservation
            | NetIpamDhcp
            | NetSegmentRemote
            | NetApplyStaged
            | NetApplyRollback
            | NetObserve
            | NetVerifyDataplane
            | NetOwnershipMarker
            | VmNetworkNat
            | VmNetworkBridge
            | VmNetworkSdn
            | VmStaticIp => Domain::Network,
            VolumeLocal | VolumeBind | VolumeNfs | VolumeCifs | VolumeWebdav | VolumeQuota
            | VolumeSnapshot | VolumeProvisionNas | StoragePools | StorageLvmThin
            | StorageZfsBtrfs | StorageCeph => Domain::Storage,
            VmSnapshotDisk
            | VmSnapshotMemory
            | VmSnapshotRestore
            | VmSnapshotDelete
            | VmSnapshotPersistent
            | VmBackupDisk
            | VmBackupQuiesced
            | VmBackupRestore
            | ContainerBackupRestore => Domain::Protection,
            VmMigrationCold | VmMigrationLive | VmReplication | VmHighAvailability => {
                Domain::Mobility
            }
            FirewallPerWorkload
            | FirewallDefaultDeny
            | FirewallSourceFiltering
            | FirewallEgressPolicy
            | FirewallStateless
            | FirewallLogging
            | FirewallIcmpType
            | FirewallWorkloadPeer => Domain::Firewall,
            VmConsoleSerial | VmConsoleVnc | VmGuestAgent | VmIpObserved => Domain::Console,
            MetricsPrometheus | MetricsPerWorkloadNetwork | HostHealth | HostCapacity => {
                Domain::Metrics
            }
            TransportVerified | CredentialInVault => Domain::Access,
        }
    }

    /// Looks a published name up. `None` for a name this catalog version does
    /// not know — the caller refuses it, never guesses a neighbour.
    pub fn from_name(name: &str) -> Option<Capability> {
        Self::ALL.iter().copied().find(|c| c.name() == name)
    }
}

/// What a provider says about ONE capability. Five declared states plus one
/// the host decides at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum CapabilityState {
    /// Implemented AND exercised: `evidence` names the test or battery section
    /// that proves it (`e2e:<section title>`, `test:<path>::<fn>`,
    /// `chaos:<scenario>`, `live:<path>`). A CLI test checks the reference exists.
    Supported { evidence: &'static str },
    /// Implemented with a written limitation, or implemented without a live
    /// proof yet. `detail` says which.
    Partial { detail: &'static str },
    /// This provider cannot do it, by its nature or by a written decision.
    UnsupportedByProvider { reason: &'static str },
    /// Possible only with a component the engine does not ship or manage.
    RequiresExternalComponent { component: &'static str },
    /// Nobody built it yet. Distinct from "cannot": it could exist.
    NotImplemented,
    /// Declared as supported, but the probe of THIS host said no
    /// (a tool missing, a connection refused, a kernel feature off).
    UnavailableOnHost { detail: String },
}

impl CapabilityState {
    pub fn label(&self) -> &'static str {
        match self {
            CapabilityState::Supported { .. } => "supported",
            CapabilityState::Partial { .. } => "partial",
            CapabilityState::UnsupportedByProvider { .. } => "unsupported-by-provider",
            CapabilityState::RequiresExternalComponent { .. } => "requires-external-component",
            CapabilityState::NotImplemented => "not-implemented",
            CapabilityState::UnavailableOnHost { .. } => "unavailable-on-host",
        }
    }

    /// The free text next to the label (empty for the two states that need none).
    pub fn detail(&self) -> String {
        match self {
            CapabilityState::Supported { evidence } => (*evidence).to_string(),
            CapabilityState::Partial { detail } => (*detail).to_string(),
            CapabilityState::UnsupportedByProvider { reason } => (*reason).to_string(),
            CapabilityState::RequiresExternalComponent { component } => (*component).to_string(),
            CapabilityState::NotImplemented => String::new(),
            CapabilityState::UnavailableOnHost { detail } => detail.clone(),
        }
    }

    /// `true` when a request needing this capability may go ahead on this
    /// provider — the contract's `Capability.supported`.
    pub fn is_usable(&self) -> bool {
        matches!(
            self,
            CapabilityState::Supported { .. } | CapabilityState::Partial { .. }
        )
    }

    /// `true` when the PROVIDER declares this capability, whatever this host
    /// has installed: `is_usable`, plus `UnavailableOnHost` — which only ever
    /// comes from [`CapabilityState::on_host`] narrowing a declared
    /// `Supported`/`Partial`, so it is a declared "yes" the probe could not
    /// confirm here. This is the question a predicate about the provider's
    /// NATURE asks ("does libvirt supervise a crash restart itself?"); a
    /// request that must run on this host keeps asking `is_usable`.
    pub fn declared_usable(&self) -> bool {
        self.is_usable() || matches!(self, CapabilityState::UnavailableOnHost { .. })
    }

    /// Narrows a declared state by what the host probe found: a declared
    /// `Supported`/`Partial` becomes `UnavailableOnHost` when `present` is
    /// false. The other states are already "no" and do not change — a host
    /// cannot make a provider support what it does not.
    pub fn on_host(self, present: bool, detail: &str) -> CapabilityState {
        if present || !self.is_usable() {
            self
        } else {
            CapabilityState::UnavailableOnHost {
                detail: detail.to_string(),
            }
        }
    }
}

/// One row of a provider's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityReport {
    #[serde(serialize_with = "ser_name")]
    pub capability: Capability,
    #[serde(serialize_with = "ser_domain")]
    pub domain: Domain,
    #[serde(flatten)]
    pub state: CapabilityState,
}

fn ser_name<S: serde::Serializer>(c: &Capability, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(c.name())
}

fn ser_domain<S: serde::Serializer>(d: &Domain, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(d.as_str())
}

impl CapabilityReport {
    pub fn new(capability: Capability, state: CapabilityState) -> Self {
        Self {
            capability,
            domain: capability.domain(),
            state,
        }
    }
}

/// The provider's own health, as the contract's `Condition` reads it: a
/// three-state answer with a stable reason. `Unknown` is never collapsed into
/// `Unavailable` — "could not ask" and "asked, and no" have opposite remedies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HealthStatus {
    Healthy,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderHealth {
    pub status: HealthStatus,
    /// CamelCase, stable — scripts branch on it.
    pub reason: &'static str,
    pub message: String,
}

/// Everything one provider says about itself — the shape `ProviderInfo` in
/// the node contract carries, produced by the provider, not about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderReport {
    pub id: String,
    pub kind: ProviderKind,
    /// `true` when this provider can be selected on this host right now.
    pub available: bool,
    pub health: ProviderHealth,
    pub capabilities: Vec<CapabilityReport>,
}

impl ProviderReport {
    /// Builds a report by asking `classify` about EVERY catalog entry of the
    /// provider's `kind` (and the cross-kind entries a compute provider owns,
    /// see [`Capability::kind`]; a `Gateway` report walks the `Network` entries,
    /// see [`ProviderKind::catalog_kind`]). The walk is the whole point: a provider
    /// cannot skip an entry, and a new catalog entry makes every provider's
    /// `classify` fail to compile until it answers.
    pub fn build(
        id: &str,
        kind: ProviderKind,
        available: bool,
        health: ProviderHealth,
        classify: impl Fn(Capability) -> CapabilityState,
    ) -> Self {
        let capabilities = Capability::ALL
            .iter()
            .copied()
            .filter(|c| c.kind() == kind.catalog_kind())
            .map(|c| CapabilityReport::new(c, classify(c)))
            .collect();
        Self {
            id: id.to_string(),
            kind,
            available,
            health,
            capabilities,
        }
    }

    pub fn count(&self, label: &str) -> usize {
        self.capabilities
            .iter()
            .filter(|r| r.state.label() == label)
            .count()
    }

    /// This provider's share of the maturity cell metric.
    pub fn cell_metric(&self) -> CellMetric {
        let mut m = CellMetric::default();
        for r in &self.capabilities {
            m.add(&r.state);
        }
        m
    }
}

/// The maturity plan's cell metric (plan 65, level N2): of the cells a provider
/// COULD hold, how many are **proved** — `supported`, with evidence the
/// evidence gate confirms exists.
///
/// The plan's filename is Portuguese, so it is named once in the docstring of
/// `scripts/capability_ratchet.py`, which the language ratchet does not read:
/// an ENGLISH comment that cites a Portuguese path counts as Portuguese debt,
/// and the reference is worth keeping where it does not cost one.
///
/// A *cell* is one (capability × provider) pair. The denominator deliberately
/// leaves out the two states that say the cell can never be filled by that
/// provider — `unsupported-by-provider` (it cannot, by its nature or by a
/// written decision) and `requires-external-component` (it needs something the
/// engine does not ship). Counting those would make a provider look worse for
/// honestly declaring what it is not, and better for staying silent.
///
/// What remains is work that exists (`supported`, `partial`), work that does
/// not (`not-implemented`), or a declared yes this host could not confirm
/// (`unavailable-on-host`) — and that is the set a maturity number has to
/// measure.
///
/// **The published matrix is the DECLARED view**, so it never carries
/// `unavailable-on-host`; `provider ls` on a real host can report a smaller
/// metric, and the difference is exactly what that host is missing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CellMetric {
    /// Cells at N2: `supported`.
    pub proved: usize,
    /// Cells this provider could hold at all (the denominator).
    pub applicable: usize,
}

impl CellMetric {
    pub fn add(&mut self, state: &CapabilityState) {
        match state {
            CapabilityState::Supported { .. } => {
                self.proved += 1;
                self.applicable += 1;
            }
            CapabilityState::Partial { .. }
            | CapabilityState::NotImplemented
            | CapabilityState::UnavailableOnHost { .. } => self.applicable += 1,
            CapabilityState::UnsupportedByProvider { .. }
            | CapabilityState::RequiresExternalComponent { .. } => {}
        }
    }

    pub fn merge(&mut self, other: CellMetric) {
        self.proved += other.proved;
        self.applicable += other.applicable;
    }

    /// Percent in tenths, rounded half-up with INTEGER arithmetic.
    ///
    /// Not `{:.1}` over an `f64`: this number is written into a file that a
    /// test compares byte for byte and a gate parses, and a float's rounding of
    /// an exact half is the kind of difference that shows up as a mysterious
    /// one-digit diff rather than as an error.
    pub fn tenths(&self) -> usize {
        if self.applicable == 0 {
            return 0;
        }
        (self.proved * 1000 + self.applicable / 2) / self.applicable
    }

    pub fn percent(&self) -> String {
        let t = self.tenths();
        format!("{}.{}", t / 10, t % 10)
    }
}

/// The metric over a whole set of reports.
pub fn cell_metric(reports: &[ProviderReport]) -> CellMetric {
    let mut m = CellMetric::default();
    for r in reports {
        m.merge(r.cell_metric());
    }
    m
}

/// The metric per domain, in the catalog's own order.
///
/// The order comes from [`Capability::ALL`] and not from a second list of
/// domains: a list that has to agree with another list is a list that stops
/// agreeing, and this repository has paid for that more than once.
pub fn cell_metric_by_domain(reports: &[ProviderReport]) -> Vec<(Domain, CellMetric)> {
    let mut order: Vec<Domain> = Vec::new();
    for c in Capability::ALL {
        if !order.contains(&c.domain()) {
            order.push(c.domain());
        }
    }
    let mut out: Vec<(Domain, CellMetric)> = Vec::new();
    for d in order {
        let mut m = CellMetric::default();
        for r in reports {
            for cr in &r.capabilities {
                if cr.capability.domain() == d {
                    m.add(&cr.state);
                }
            }
        }
        if m.applicable > 0 {
            out.push((d, m));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn declared_usable_survives_the_host_probe_but_never_promotes_a_no() {
        use super::CapabilityState as S;
        let yes = S::Partial { detail: "d" };
        let narrowed = yes.clone().on_host(false, "tool missing");
        assert!(matches!(narrowed, S::UnavailableOnHost { .. }));
        assert!(!narrowed.is_usable(), "the host did say no");
        assert!(narrowed.declared_usable(), "but the provider did say yes");
        for no in [
            S::UnsupportedByProvider { reason: "r" },
            S::RequiresExternalComponent { component: "c" },
            S::NotImplemented,
        ] {
            assert!(!no.declared_usable(), "{}", no.label());
            assert!(
                !no.on_host(false, "x").declared_usable(),
                "a probe cannot make a no a yes"
            );
        }
    }

    use super::*;
    use std::collections::HashSet;

    #[test]
    fn names_are_unique_dotted_and_stable_looking() {
        let mut seen = HashSet::new();
        for c in Capability::ALL {
            let n = c.name();
            assert!(seen.insert(n), "duplicate capability name {n}");
            assert!(n.contains('.'), "{n} is not dotted");
            assert!(
                n.chars().all(|ch| ch.is_ascii_lowercase()
                    || ch.is_ascii_digit()
                    || ch == '.'
                    || ch == '-'),
                "{n} has characters a script would have to quote"
            );
        }
    }

    #[test]
    fn every_entry_is_in_all_exactly_once() {
        // The `match` arms in `name()` are exhaustive, so a variant missing
        // from ALL would be the only way to hide an entry. Round-trip every
        // name back through `from_name`.
        for c in Capability::ALL {
            assert_eq!(Capability::from_name(c.name()), Some(*c));
        }
        assert_eq!(Capability::from_name("vm.does-not-exist"), None);
    }

    #[test]
    fn a_declared_no_never_becomes_a_host_no() {
        let no = CapabilityState::UnsupportedByProvider {
            reason: "by design",
        };
        assert_eq!(no.clone().on_host(false, "tool missing"), no);
        let yes = CapabilityState::Supported { evidence: "e2e:x" };
        assert_eq!(yes.clone().on_host(true, "-"), yes);
        assert_eq!(
            yes.on_host(false, "virsh missing").label(),
            "unavailable-on-host"
        );
    }

    #[test]
    fn a_report_walks_only_its_kind_and_every_entry_of_it() {
        let r = ProviderReport::build(
            "x",
            ProviderKind::Network,
            true,
            ProviderHealth {
                status: HealthStatus::Healthy,
                reason: "Ok",
                message: String::new(),
            },
            |_| CapabilityState::NotImplemented,
        );
        let expected = Capability::ALL
            .iter()
            .filter(|c| c.kind() == ProviderKind::Network)
            .count();
        assert_eq!(r.capabilities.len(), expected);
        assert!(r
            .capabilities
            .iter()
            .all(|c| c.capability.kind() == ProviderKind::Network));
        assert_eq!(r.count("not-implemented"), expected);
    }

    #[test]
    fn a_gateway_report_walks_the_network_entries_and_keeps_its_own_kind() {
        let r = ProviderReport::build(
            "appliance",
            ProviderKind::Gateway,
            false,
            ProviderHealth {
                status: HealthStatus::Unknown,
                reason: "NotProbed",
                message: String::new(),
            },
            |_| CapabilityState::NotImplemented,
        );
        assert_eq!(r.kind, ProviderKind::Gateway);
        let network = Capability::ALL
            .iter()
            .filter(|c| c.kind() == ProviderKind::Network)
            .count();
        assert_eq!(r.capabilities.len(), network);
        assert!(r
            .capabilities
            .iter()
            .any(|c| c.capability == Capability::NetGatewayFilter));
        assert!(
            !Capability::ALL
                .iter()
                .any(|c| c.kind() == ProviderKind::Gateway),
            "no entry is tagged Gateway: a gateway answers the network rows"
        );
    }

    #[test]
    fn json_carries_the_name_the_domain_and_the_flattened_state() {
        let r = CapabilityReport::new(
            Capability::VmPause,
            CapabilityState::Supported {
                evidence: "e2e:vm pause",
            },
        );
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["capability"], "vm.pause");
        assert_eq!(v["domain"], "vm-compute");
        assert_eq!(v["state"], "supported");
        assert_eq!(v["evidence"], "e2e:vm pause");
    }

    /// The denominator is the whole point of the metric, so each state is
    /// asserted by name instead of by a total that would hide a swap.
    #[test]
    fn only_the_cells_a_provider_could_hold_are_in_the_denominator() {
        use super::CapabilityState as S;
        let mut m = CellMetric::default();
        m.add(&S::Supported { evidence: "e" });
        m.add(&S::Partial { detail: "d" });
        m.add(&S::NotImplemented);
        m.add(&S::UnavailableOnHost {
            detail: "tool missing".into(),
        });
        assert_eq!((m.proved, m.applicable), (1, 4));

        // The two that say "never, by this provider" are out: counting them
        // would punish a provider for declaring what it is not.
        let mut out = CellMetric::default();
        out.add(&S::UnsupportedByProvider { reason: "r" });
        out.add(&S::RequiresExternalComponent { component: "c" });
        assert_eq!((out.proved, out.applicable), (0, 0));
    }

    /// A declared yes the host could not confirm stays in the denominator and
    /// out of the numerator: it is a cell that EXISTS and is not proved here.
    #[test]
    fn a_host_that_cannot_honour_a_yes_lowers_the_metric_instead_of_hiding_it() {
        use super::CapabilityState as S;
        let declared = S::Supported { evidence: "e" };
        let narrowed = declared.clone().on_host(false, "no /dev/kvm");
        let mut a = CellMetric::default();
        a.add(&declared);
        let mut b = CellMetric::default();
        b.add(&narrowed);
        assert_eq!((a.proved, a.applicable), (1, 1));
        assert_eq!(
            (b.proved, b.applicable),
            (0, 1),
            "the cell did not disappear — it stopped being proved on this host"
        );
    }

    /// Integer arithmetic, rounded half-up: this number is written into a file
    /// a test compares byte for byte, and a float's half is a diff nobody can
    /// explain.
    #[test]
    fn the_percent_rounds_half_up_without_a_float() {
        let cases = [
            ((0usize, 0usize), "0.0"),
            ((1, 3), "33.3"),
            ((2, 3), "66.7"),
            ((100, 252), "39.7"),
            ((1, 16), "6.3"),
            ((1, 2), "50.0"),
        ];
        for ((proved, applicable), want) in cases {
            let m = CellMetric { proved, applicable };
            assert_eq!(m.percent(), want, "{proved}/{applicable}");
        }
    }

    /// The per-domain breakdown has to add up to the total, or the table at the
    /// top of the matrix says one thing and its own rows say another.
    #[test]
    fn the_domains_add_up_to_the_total() {
        let report = ProviderReport::build(
            "t",
            ProviderKind::Compute,
            true,
            ProviderHealth {
                status: HealthStatus::Unknown,
                reason: "NotProbed",
                message: String::new(),
            },
            |c| match c.domain() {
                Domain::VmCompute => CapabilityState::Supported { evidence: "e" },
                Domain::Storage => CapabilityState::UnsupportedByProvider { reason: "r" },
                _ => CapabilityState::NotImplemented,
            },
        );
        let reports = vec![report];
        let total = cell_metric(&reports);
        let by_domain = cell_metric_by_domain(&reports);
        let summed = by_domain
            .iter()
            .fold(CellMetric::default(), |mut a, (_, m)| {
                a.merge(*m);
                a
            });
        assert_eq!(
            (summed.proved, summed.applicable),
            (total.proved, total.applicable)
        );
        assert!(
            !by_domain.iter().any(|(d, _)| *d == Domain::Storage),
            "a domain with no applicable cell is not a row of zeros"
        );
    }
}

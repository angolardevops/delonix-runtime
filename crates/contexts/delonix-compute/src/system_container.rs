//! The system-container provider port (ADR-0058, plan 63 slice 3): what a
//! provider of system containers receives (`SystemContainerSpec`), what it
//! hands back (`SystemContainerHandle`, `SystemContainerObservation`), and the
//! lifecycle it answers.
//!
//! A system container is NOT a `Container`. It is a whole userland run as one
//! unit, closer to a VM than to an application container: the provider that
//! runs it (a Proxmox node) offers no `exec`, no logs and no exit status, and
//! the engine's dataplane (SDN, isolation, DNS, publish) does not reach it.
//! Those are catalog rows the provider answers (`system-container.*`), never
//! defaults here that quietly do nothing (ADR-0044 D3).
//!
//! Three things the shape decides:
//!
//! - **The image arrives as a local OCI archive** with its manifest digest,
//!   already pulled and verified by the engine. The provider uploads it and
//!   never pulls anything itself: a node's own pull keeps no digest and takes
//!   no credentials (ADR-0058 point 6).
//! - **`unprivileged` is a field, and `false` is refused.** Asking for a
//!   privileged container on a remote node is a privilege decision of its own,
//!   with its own spike; the field exists so the refusal is by name.
//! - **The network verdict is part of the observation.** A start that did not
//!   bring the address it asked for is `NetworkState::NotReady` with the
//!   provider's reason, never "running, all good" (ADR-0058 T3).

use crate::vm_provider::Provider;
use delonix_model::Result;
use std::path::{Path, PathBuf};

/// Everything a system-container provider needs. Built by the caller that
/// pulled the image; never persisted on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemContainerSpec {
    /// The container's name, used as its hostname.
    pub name: String,
    /// A local image archive with OCI media types (`write_oci_media_archive`).
    pub archive: PathBuf,
    /// The digest of the manifest INSIDE `archive` (`sha256:<hex>`): the
    /// archive's identity, and the name it is uploaded under.
    pub manifest_digest: String,
    /// The command to run as the container's init. Empty keeps the image's.
    pub entrypoint: Vec<String>,
    /// The runtime environment. Empty keeps the image's; non-empty replaces
    /// it whole, as the image's own is replaced (never merged in silence).
    pub env: Vec<(String, String)>,
    pub memory_mib: u32,
    pub swap_mib: u32,
    pub cores: u32,
    pub rootfs_gib: u32,
    /// `None`: no network interface at all.
    pub network: Option<SystemContainerNet>,
    /// Must be `true`; a provider refuses `false` by name.
    pub unprivileged: bool,
}

/// A network interface on a bridge of the host that runs the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemContainerNet {
    pub bridge: String,
    pub vlan: Option<u16>,
    /// Ask for an IPv4 address by DHCP; without it the interface is up with
    /// no address.
    pub dhcp: bool,
}

/// What a provider hands back from `create`: enough to address the container
/// again. `locator` is the provider's own form (`proxmox:<node>:<vmid>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemContainerHandle {
    pub name: String,
    pub locator: String,
}

/// Whether the network asked for is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkState {
    /// No network was asked for, or no address was.
    NotRequested,
    /// The interface has this IPv4 address.
    Ready { ipv4: String },
    /// Running, and the address asked for did not come. `reason` is the
    /// provider's own words (a task warning, or what the interfaces show).
    NotReady { reason: String },
    /// Not running: there is nothing to judge.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemContainerObservation {
    pub running: bool,
    pub network: NetworkState,
}

/// What the provider holds for a container NOW, read from the provider and
/// never from a local record — a change made on the host by hand has to show
/// up here, or a plan would compare the record with itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemContainerConfig {
    pub memory_mib: u32,
    pub swap_mib: u32,
    pub cores: u32,
    pub entrypoint: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The fields a running container takes without being recreated (measured on
/// the provider before being declared: the Proxmox node writes memory and
/// swap to the container's cgroup at once, and cores to its cpuset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemContainerResources {
    pub memory_mib: u32,
    pub swap_mib: u32,
    pub cores: u32,
}

/// The system-container lifecycle (ADR-0058).
///
/// `create` leaves the container created, configured and stopped; `start`
/// judges the network it brought up. `stop` keeps the container; `destroy`
/// releases it and its disk. `dir` is this container's own state directory,
/// where a provider keeps its task ledger.
pub trait SystemContainerProvider: Provider {
    fn create(&self, dir: &Path, spec: &SystemContainerSpec) -> Result<SystemContainerHandle>;
    fn start(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        spec: &SystemContainerSpec,
    ) -> Result<SystemContainerObservation>;
    fn stop(&self, dir: &Path, h: &SystemContainerHandle) -> Result<()>;
    fn destroy(&self, dir: &Path, h: &SystemContainerHandle) -> Result<()>;
    fn observe(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        spec: &SystemContainerSpec,
    ) -> Result<SystemContainerObservation>;
    /// `None` when the provider no longer has the container.
    fn configuration(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
    ) -> Result<Option<SystemContainerConfig>>;
    /// Applies `r` to a created container, running or not, and reads it
    /// back; a provider that kept something else answers an error.
    fn resize(
        &self,
        dir: &Path,
        h: &SystemContainerHandle,
        r: SystemContainerResources,
    ) -> Result<()>;
}

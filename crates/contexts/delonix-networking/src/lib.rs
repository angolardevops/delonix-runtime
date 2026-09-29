//! The networking context (`networking.delonix.io`, ADR-0040 D2.2): the role
//! ports a network provider implements, their registries by name, and the
//! owner marks a remote object carries (ADR-0059 D7).
//!
//! `GatewayProvider` and `NetworkZoneProvider` moved here from `delonix-sdn`
//! in ADR-0059 F2a, in the shape `VmBackend` left `delonix-vm` for the
//! compute context (ADR-0044 P4b.2): the providers that implement them
//! (`delonix-opnsense`, `delonix-proxmox`) depend on a context, not on the
//! native dataplane, and `delonix-sdn` re-exports the three modules under
//! their old paths.

mod error;
pub mod gateway;
pub mod network_zone;
pub mod ownership;

pub use error::{Error, Result};

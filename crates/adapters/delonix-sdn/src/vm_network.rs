//! The rootless SDN as the VM engine's [`VmNetwork`] port.
//!
//! The VM engine used to call [`crate::infra`] directly, which made one adapter
//! depend on another (ADR-0040's `delonix-vm → delonix-sdn` exception). It now
//! asks the port, and the composition root registers this implementation.

use delonix_compute::ports::VmNetwork;
use delonix_model::Result;

/// The host's rootless infra.
pub struct HostVmNetwork;

impl VmNetwork for HostVmNetwork {
    fn ensure_network(&self, name: &str) -> Result<()> {
        crate::infra::network_create(name)
            .map(|_| ())
            .map_err(Into::into)
    }

    fn attach_tap(&self, vm: &str, network: &str, mac: &str, namespace: &str) -> Result<String> {
        crate::infra::vm_attach(vm, network, mac, namespace).map_err(Into::into)
    }

    fn lease_ip(&self, network: &str, mac: &str) -> Option<String> {
        crate::infra::dhcp_ip_for_mac(network, mac)
    }

    fn detach_tap(&self, vm: &str, lease: Option<&str>) {
        crate::infra::vm_detach(vm, lease)
    }

    fn join_argv(&self) -> Option<Vec<String>> {
        crate::infra::infra_join_argv()
    }
}

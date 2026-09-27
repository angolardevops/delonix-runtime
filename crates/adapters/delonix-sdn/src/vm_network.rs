//! The rootless SDN as the VM engine's [`VmNetwork`] port.
//!
//! The VM engine used to call [`crate::infra`] directly, which made one adapter
//! depend on another (ADR-0040's `delonix-vm → delonix-sdn` exception). It now
//! asks the port, and the composition root registers this implementation.

use std::path::PathBuf;

use delonix_compute::ports::VmNetwork;
use delonix_model::Result;

/// The host's rootless infra.
pub struct HostVmNetwork {
    /// The engine's state root, where networks are declared (`NetworkStore`).
    pub state_root: PathBuf,
}

impl VmNetwork for HostVmNetwork {
    /// A VM's named network, created the way `network create` creates one: the
    /// `NetworkStore` record first (it decides the prefix), then the holder's
    /// `NetDef` aligned to it.
    ///
    /// **It used to be `infra::network_create` alone** — the holder's own
    /// allocator and no record. Measured: `vm create --network s3vm` brought the
    /// network up, `network ls` did not list it, and `network rm s3vm` answered
    /// `DX-4000 no such network` — a network nobody could see or remove, on a /16
    /// the declared networks' allocator did not know was taken.
    fn ensure_network(&self, name: &str) -> Result<()> {
        let store = crate::NetworkStore::open(&self.state_root)?;
        let (net, created) = match store.get(name) {
            Ok(n) => (n, false),
            Err(e) => {
                if !e.is_not_found() {
                    return Err(e.into());
                }
                let legacy = crate::infra::network_get(name).map(|d| d.prefix);
                (record_for(&store, name, legacy.as_deref())?, true)
            }
        };
        if let Err(e) = crate::infra::network_create_with(name, &net.prefix) {
            // Same rollback as `network create`: a record whose network could not
            // be realized would list as present and refuse every attach.
            if created {
                let _ = store.remove(name);
            }
            return Err(e.into());
        }
        Ok(())
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

/// The `NetworkStore` record of a network the VM engine asks for by name.
///
/// A network an earlier version brought up for a VM already has a `NetDef` and
/// no record: it is ADOPTED on its own /16 rather than given a new one, which
/// the holder would refuse as a prefix conflict — and that is also what makes
/// those networks visible to `network ls`/`rm` again.
fn record_for(
    store: &crate::NetworkStore,
    name: &str,
    legacy_prefix: Option<&str>,
) -> Result<crate::Network> {
    match legacy_prefix.and_then(base_octet) {
        Some(base) => store.create_with_base(name, base),
        None => store.create(name),
    }
    .map_err(Into::into)
}

/// The second octet of a two-octet prefix (`"10.201"` → `201`); `None` for any
/// other shape, which then gets a fresh record instead of an adopted one.
fn base_octet(prefix: &str) -> Option<u8> {
    match prefix.split('.').collect::<Vec<_>>().as_slice() {
        ["10", b] => b.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_vm_network_is_adopted_on_its_own_octet() {
        assert_eq!(base_octet("10.201"), Some(201));
        assert_eq!(base_octet("10.254"), Some(254));
        assert_eq!(base_octet("192.168"), None);
        assert_eq!(base_octet("10.201.5"), None);
        assert_eq!(base_octet("10.x"), None);
    }

    /// The record is the part that used to be skipped: a network the VM engine
    /// creates has to be one the CLI lists, and one it inherited from an earlier
    /// version keeps the /16 its workloads are already addressed on.
    #[test]
    fn a_vm_network_gets_a_network_store_record() {
        let root = std::env::temp_dir().join(format!("dlx-vmnet-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = crate::NetworkStore::open(&root).unwrap();
        let fresh = record_for(&store, "vmnet", None).unwrap();
        assert_eq!(store.get("vmnet").unwrap().prefix, fresh.prefix);
        assert!(store.list().unwrap().iter().any(|n| n.name == "vmnet"));

        let adopted = record_for(&store, "oldvm", Some("10.233")).unwrap();
        assert_eq!(adopted.prefix, "10.233");
        assert_eq!(store.get("oldvm").unwrap().prefix, "10.233");
        let _ = std::fs::remove_dir_all(&root);
    }
}

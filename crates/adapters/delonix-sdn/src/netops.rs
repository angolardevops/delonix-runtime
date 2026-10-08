//! Network use cases shared by every interface that creates or removes a
//! network — the CLI and the node API.
//!
//! One write path: the declarative record (`NetworkStore`) and the dataplane
//! record (`infra`) are two stores, and the order they are written in, the
//! rollback when the second fails and the check that refuses to remove a
//! network something is attached to are decided here, once.

use crate::{infra, Cidr, Network, NetworkStore, Result};

/// The label a pod member carries with the address the pod's netns got. A
/// member has no `network` of its own — the pod's netns is what sits on the
/// bridge — so this is how a member is found attached to a network.
pub const POD_IP_LABEL: &str = "delonix.io/pod-ip";

/// Creates a `bridge` network: the declarative record, then the dataplane
/// record on the same prefix.
///
/// `subnet` is honoured (or refused, naming what is supported); `None` lets
/// the store allocate. `gateway` runs between the two writes, with the record
/// just created: it returns the declared gateway (`None` = the engine's own
/// address) or refuses it.
///
/// A failure of `gateway` or of the dataplane write removes the declarative
/// record again. Otherwise it would be orphaned: listed, impossible to attach
/// to, and a retry would answer "already exists".
pub fn create_bridge(
    store: &NetworkStore,
    name: &str,
    subnet: Option<&str>,
    gateway: impl FnOnce(&Network) -> Result<Option<String>>,
) -> Result<Network> {
    let net = match subnet {
        Some(s) => store.create_with_cidr(name, NetworkStore::validate_subnet(s)?)?,
        None => store.create(name)?,
    };
    let declared = match gateway(&net) {
        Ok(g) => g,
        Err(e) => {
            let _ = store.remove(name);
            return Err(e);
        }
    };
    if let Err(e) = infra::network_create_with_gateway(name, &net.prefix, declared.as_deref()) {
        let _ = store.remove(name);
        return Err(e);
    }
    Ok(net)
}

/// Who, among these records, is on network `name` — as `container <name>` /
/// `vm <name>` labels. `vms` is `(vm name, its network)`.
///
/// A pod member is attached when the address under [`POD_IP_LABEL`] is inside
/// `subnet`.
pub fn dependents_of(
    name: &str,
    subnet: &str,
    containers: &[delonix_compute::Container],
    vms: &[(String, String)],
) -> Vec<String> {
    let cidr = Cidr::parse(subnet);
    let in_subnet = |ip: &str| {
        cidr.as_ref()
            .zip(Cidr::parse_addr(ip))
            .is_some_and(|(c, a)| c.contains(a))
    };
    let mut out: Vec<String> = containers
        .iter()
        .filter(|c| {
            c.network.as_deref() == Some(name)
                || c.extra_networks.iter().any(|e| e.network == name)
                || (c.pod.is_some() && c.labels.get(POD_IP_LABEL).is_some_and(|ip| in_subnet(ip)))
        })
        .map(|c| format!("container {}", c.name))
        .collect();
    out.extend(
        vms.iter()
            .filter(|(_, net)| net == name)
            .map(|(vm, _)| format!("vm {vm}")),
    );
    out
}

/// Removes network `name`: the dataplane record with its bridge, the VXLAN
/// uplink of an overlay, and only then the declarative record. Does NOT
/// check what is attached — the caller decides that with [`dependents_of`].
///
/// The uplink's name is read BEFORE anything else: it is derived from the
/// `vni`, which only the store record carries, and the next step erases it.
///
/// Deliberately the DATAPLANE runs first and the declarative record last —
/// the reverse of [`create_bridge`]'s order, and for a matching reason:
/// `infra::network_remove`/`infra::vxlan_remove` are best-effort (they log
/// and return rather than propagate a `netdef_lock()` contention or a dead
/// holder), so there is no signal here to roll back ON. What used to happen
/// erasing the store record FIRST was that a dataplane failure left the
/// physical `NetDef` on disk — read by `network_get`, and by the
/// prefix-conflict check a later `network create` runs — with NOTHING in
/// `NetworkStore` pointing at it any more: the network vanished from
/// `network ls` while a resource with its prefix kept blocking a new one,
/// unreachable by any command. Removing the store record LAST means a
/// dataplane failure simply leaves the network visible (and its name/prefix
/// still taken, honestly) for an operator to retry or investigate, instead
/// of a silently orphaned, invisible one.
pub fn remove(store: &NetworkStore, name: &str) -> Result<()> {
    let uplink = store.get(name).ok().and_then(|n| n.vxlan_dev());
    if let Some(dev) = uplink {
        infra::vxlan_remove(&dev);
    }
    infra::network_remove(name);
    store.remove(name)?;
    Ok(())
}

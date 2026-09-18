//! The `container run` network phase on this host: the implementation of
//! `delonix_compute::ports::NetworkProvider` over the holder's SDN, and the two
//! publish helpers the lifecycle verbs share.
//!
//! Terminal manners stay with the caller: the warning that namespace isolation
//! is inert on this host, and the L7 proxy route of `--expose` (the proxy is not
//! in this crate), arrive as hooks.

use std::path::PathBuf;

use delonix_compute::Container;
use delonix_model::records::ContainerFw;
use delonix_model::Result;

/// Publish a port; if it fails because the port is held by an **orphan process**
/// (the container died without `stop` and the slirp kept holding it), clears ONLY
/// that one and tries again.
///
/// Why this way and not sweeping everything beforehand: the preventive reaper ran
/// on EVERY `run` with ports and deleted by default — all it took was the
/// container list coming back empty (a read error, or a store view without the
/// records) for `live_ports` to be empty and it to conclude that NOTHING is in
/// use, deleting the hostfwds of LIVE containers. That's what made a `Ready`
/// cluster's apiserver unreachable and made two containers with `-p` never
/// coexist. Here the cleanup is REACTIVE and surgical: it only happens when the
/// port we want fails, and only touches that one. With no conflict, nothing is
/// deleted — and a state-read error can no longer destroy what's working.
pub fn publish_with_retry(ip: &str, spec: &str) -> Result<()> {
    publish_with_retry_local(ip, spec).map_err(Into::into)
}

fn publish_with_retry_local(ip: &str, spec: &str) -> crate::Result<()> {
    match crate::infra::publish_port(ip, spec) {
        Ok(()) => Ok(()),
        Err(e) => {
            let (hp, _, _) = crate::parse_publish(spec)?;
            // Orphans first (dead container's slirp still holding the port),
            // then the hostfwd for that specific port.
            let _ = crate::reap_orphan_slirp();
            crate::infra::unpublish_port(&hp);
            crate::infra::publish_port(ip, spec).map_err(|_| e)
        }
    }
}

/// Release the ports published by a container (best-effort, idempotent).
///
/// Two paths, both need cleanup:
///
/// - **custom network**: persistent rules in the ingress (hostfwd on the single
///   slirp + DNAT on the holder) — removed per port.
/// - **per-container slirp**: ITS slirp is killed. This branch used to claim that
///   "the slirp process dies with the container's netns, there's nothing to clean
///   up" and returned right away. That's false: the slirp only exits once it
///   NOTICES the netns is gone, and in that window it keeps holding the host port.
///   Measured thus: `stop` followed by an immediate `start` failed 3 times out of
///   3 with `add_hostfwd: slirp_add_hostfwd failed`, and started working on its
///   own a few seconds later.
///
/// `slirp_pid` has to be the init's pid **from before** stopping it: `runtime::stop`
/// and `runtime::remove` set `container.pid = None`, so reading `c.pid` in here
/// would give `None` for every caller that already stopped the container — the
/// slirp would never be reaped and the bug above would stand. Hence an explicit
/// parameter instead of coming from the record.
pub fn unpublish_ports(c: &Container, slirp_pid: Option<i32>) {
    match port_home(c.network.is_some(), c.pod.is_some(), !c.ports.is_empty()) {
        PortHome::Ingress { detach } => {
            // 1) ports: release the hostfwd/DNAT in the ingress (idempotent — removing
            //    a port that's no longer there is harmless).
            for spec in &c.ports {
                if let Ok((host_port, _, _)) = crate::parse_publish(spec) {
                    crate::infra::unpublish_port(&host_port);
                }
            }
            // 2) network: release the veth/IP and drop the ingress ref marker.
            //    ALWAYS detach when there's an ip — `crate::infra::release` is now
            //    IDEMPOTENT (a per-id marker set, not a blind counter), so `stop`
            //    then `rm` of the same container no longer double-counts, and a
            //    container that died ABRUPTLY (no `stop`, `reconcile_status` already
            //    nulled the pid) still gets its marker released here. The old guard
            //    `slirp_pid.is_some()` skipped the detach precisely in that abrupt
            //    path → the ref leaked (seen: 16 with 3 containers alive). The
            //    `system prune` reaper (`reap_orphan_refs`) is the backstop for
            //    containers that die and are never `rm`'d at all.
            if let (true, Some(ip)) = (detach, &c.ip) {
                crate::infra::detach_container(&c.id, ip);
            }
        }
        PortHome::OwnSlirp => {
            if let Some(pid) = slirp_pid {
                crate::reap_slirp_for(pid);
            }
        }
        // With no published ports there's no slirp with an api-socket holding anything.
        PortHome::Nowhere => {}
    }
}

/// Where a container's published ports live, and so what releasing them means.
#[derive(Debug, PartialEq, Eq)]
enum PortHome {
    /// The shared ingress slirp. `detach` says whether the container also owns
    /// its netns — a pod member does not: the netns is the pod's, shared with
    /// its peers.
    Ingress { detach: bool },
    /// The container's own slirp (no custom network).
    OwnSlirp,
    /// Nothing published, nothing to release.
    Nowhere,
}

/// A pod member has no `network` in its record — membership is the `pod` field
/// — and its ports are published on the shared ingress, like a custom network's.
/// Reading only `network` sent it down the own-slirp path, which a member does
/// not have: the hostfwd stayed on the ingress after `rm -f`, and a new pod on
/// the same port was refused as «already in use» (measured).
fn port_home(has_network: bool, in_pod: bool, has_ports: bool) -> PortHome {
    if has_network {
        PortHome::Ingress { detach: true }
    } else if in_pod {
        PortHome::Ingress { detach: false }
    } else if has_ports {
        PortHome::OwnSlirp
    } else {
        PortHome::Nowhere
    }
}

/// What a container's start needs from the network: the resolver for its
/// `/etc/resolv.conf` and the address for its `/etc/hosts`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LaunchAddresses {
    pub dns: Option<String>,
    pub hosts_ip: Option<String>,
}

/// The network's answers for a start.
///
/// DNS: on a custom network it is the holder's own address on the bridge, where
/// the internal resolver answers; a pod member is on `delonix0`, so it is the
/// infra gateway; with `-p` (slirp) it is the slirp's resolver; on `--net host`
/// there is none, and the runtime copies the host's `resolv.conf`.
///
/// `bridge_addr` and NOT `default_route`: a network with a DECLARED gateway sends
/// its workloads out through an appliance, and that appliance does not run this
/// engine's resolver. Taking one string for both questions is what made
/// `<name>.<ns>.delonix.internal` stop resolving on such a network — with no
/// error, because a resolver that is simply not there just times out.
///
/// A POD member needs the infra gateway for the same reason: without it nothing
/// resolved by name in a pod, since the re-exec runs in the holder's mount
/// namespace, where the host's `/etc/resolv.conf` does not exist.
pub fn launch_addresses(l: &delonix_compute::launch::Launch) -> LaunchAddresses {
    let dns = match &l.custom_net {
        Some(n) => crate::infra::resolve_net(n).ok().map(|p| p.bridge_addr),
        None if l.pod => Some(crate::infra::INFRA_GATEWAY.to_string()),
        None if !l.slirp_ports.is_empty() => Some(crate::SLIRP_DNS.to_string()),
        None => None,
    };
    // /etc/hosts: the custom network's address, or the slirp's with `-p`.
    let hosts_ip = l
        .attached_ip
        .clone()
        .or_else(|| (!l.slirp_ports.is_empty()).then(|| crate::SLIRP_IP.to_string()));
    LaunchAddresses { dns, hosts_ip }
}

/// The host's network as the `container run` use case's `NetworkProvider`.
pub struct HostNetwork<'a> {
    /// The engine's state root, where networks are declared.
    pub state_root: PathBuf,
    /// Called after a successful attach, with the container's namespace.
    pub on_attached: &'a dyn Fn(&str),
    /// Registers a `--expose` route in the L7 proxy.
    pub register_expose: &'a dyn Fn(&str, &str, &str, u16) -> Result<()>,
}

impl delonix_compute::ports::NetworkProvider for HostNetwork<'_> {
    fn check_network(&self, name: &str) -> Result<()> {
        let store =
            crate::NetworkStore::open(&self.state_root).map_err(delonix_model::Error::from)?;
        store.get(name).map(|_| ()).map_err(Into::into)
    }

    fn attach(
        &self,
        id: &str,
        network: &str,
        namespace: &str,
        fixed_ip: Option<&str>,
    ) -> Result<(String, String)> {
        let attached = match fixed_ip {
            Some(fixed) => crate::infra::attach_container_on_ip(id, network, fixed, namespace),
            None => crate::infra::attach_container(id, network, namespace),
        }
        .map_err(delonix_model::Error::from)?;
        (self.on_attached)(namespace);
        Ok(attached)
    }

    fn detach(&self, id: &str, ip: &str) {
        crate::infra::detach_container(id, ip);
    }

    fn publish(&self, ip: &str, spec: &str) -> Result<()> {
        publish_with_retry(ip, spec)
    }

    fn unpublish(&self, container: &Container) {
        unpublish_ports(container, None);
    }

    fn apply_firewall(&self, id: &str, ip: &str, fw: &ContainerFw) -> Result<()> {
        crate::infra::apply_firewall(id, ip, fw).map_err(Into::into)
    }

    fn shape(&self, id: &str, bps: &str, burst: Option<&str>) -> Result<()> {
        let rate = crate::parse_net_rate(bps, burst).map_err(delonix_model::Error::from)?;
        crate::infra::set_net_rate(id, rate.rate_bit, rate.burst_bytes).map_err(Into::into)
    }

    fn register_expose(&self, name: &str, namespace: &str, ip: &str, port: u16) -> Result<()> {
        (self.register_expose)(name, namespace, ip, port)
    }
}

#[cfg(test)]
mod port_home_tests {
    use super::{port_home, PortHome};

    #[test]
    fn a_pod_member_releases_its_ports_on_the_ingress_but_keeps_the_netns() {
        assert_eq!(
            port_home(true, false, true),
            PortHome::Ingress { detach: true }
        );
        assert_eq!(
            port_home(false, true, true),
            PortHome::Ingress { detach: false }
        );
        assert_eq!(port_home(false, false, true), PortHome::OwnSlirp);
        assert_eq!(port_home(false, false, false), PortHome::Nowhere);
    }
}

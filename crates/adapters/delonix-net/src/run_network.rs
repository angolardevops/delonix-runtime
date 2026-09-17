//! The `container run` network phase on this host: the implementation of
//! `delonix_compute::ports::NetworkProvider` over the holder's SDN, and the two
//! publish helpers the lifecycle verbs share.
//!
//! Terminal manners stay with the caller: the warning that namespace isolation
//! is inert on this host, and the L7 proxy route of `--expose` (the proxy is not
//! in this crate), arrive as hooks.

use std::path::PathBuf;

use delonix_runtime_core::{Container, ContainerFw, Result};

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
    match &c.network {
        Some(_) => {
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
            if let Some(ip) = &c.ip {
                crate::infra::detach_container(&c.id, ip);
            }
        }
        None => {
            // With no published ports there's no slirp with an api-socket holding anything.
            if c.ports.is_empty() {
                return;
            }
            if let Some(pid) = slirp_pid {
                crate::reap_slirp_for(pid);
            }
        }
    }
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
        crate::NetworkStore::open(&self.state_root)?
            .get(name)
            .map(|_| ())
    }

    fn attach(
        &self,
        id: &str,
        network: &str,
        namespace: &str,
        fixed_ip: Option<&str>,
    ) -> Result<(String, String)> {
        let attached = match fixed_ip {
            Some(fixed) => crate::infra::attach_container_on_ip(id, network, fixed, namespace)?,
            None => crate::infra::attach_container(id, network, namespace)?,
        };
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
        crate::infra::apply_firewall(id, ip, fw)
    }

    fn shape(&self, id: &str, bps: &str, burst: Option<&str>) -> Result<()> {
        let rate = crate::parse_net_rate(bps, burst)?;
        crate::infra::set_net_rate(id, rate.rate_bit, rate.burst_bytes)
    }

    fn register_expose(&self, name: &str, namespace: &str, ip: &str, port: u16) -> Result<()> {
        (self.register_expose)(name, namespace, ip, port)
    }
}

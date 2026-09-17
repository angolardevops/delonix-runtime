//! The host as the `container run` use case's `WorkloadRuntime`
//! (`docs/discovery/54_P2_COMPUTE_RUN.md`, the last read-and-effect port of P3).
//!
//! It starts the process with this engine — in the caller, or under the detached
//! supervisor — from the spawn specification the one builder produces
//! ([`crate::launch_spec::run_spec`]).
//!
//! What is not the kernel's comes in as hooks, the way the other P3 adapters
//! take it: what the NETWORK decides (the resolver and `/etc/hosts` address, and
//! the container's own slirp), and what belongs to the TERMINAL or to the CLI's
//! own helpers (the health monitor, printing the id, removing what a refused
//! start left behind). An adapter does not depend on another adapter, so the
//! network's answers cannot be fetched from here.

use std::path::Path;

use delonix_compute::launch::{Launch, WorkloadRuntime};
use delonix_runtime_core::{Container, Result, Status, Store};

/// The resolver and the `/etc/hosts` address a start gets from the network.
pub type LaunchAddressesHook<'a> = dyn Fn(&Launch) -> (Option<String>, Option<String>) + 'a;

/// The host's process runtime as a [`WorkloadRuntime`].
pub struct HostWorkload<'a> {
    pub store: &'a Store,
    /// Where the event log lives.
    pub state_root: &'a Path,
    /// What the network decides for this start.
    pub addresses: &'a LaunchAddressesHook<'a>,
    /// Wires the container's own slirp to its init, with the ports it publishes.
    pub attach_slirp: &'a dyn Fn(i32, &[String]) -> Result<()>,
    /// Called once in the supervisor, after the first start was reported.
    pub on_first_start: &'a dyn Fn(&Container),
    /// The error when the supervisor died before saying why the start failed.
    pub silent_death: &'a str,
    /// Called once a detached start is supervised.
    pub on_supervised: &'a dyn Fn(&Container),
    /// Removes what a start that never happened left behind.
    pub discard: &'a dyn Fn(&str),
}

impl WorkloadRuntime for HostWorkload<'_> {
    fn create(&self, c: &mut Container, l: &Launch) -> Result<Status> {
        let hook = |pid: i32| (self.attach_slirp)(pid, &l.slirp_ports);
        let (dns, hosts_ip) = (self.addresses)(l);
        let spec = crate::launch_spec::run_spec(c, l, dns, hosts_ip, &hook);
        crate::create_with(self.store, c, &l.rootfs, &spec)
    }

    fn supervise(&self, c: &mut Container, l: &Launch, policy: &str) -> Result<()> {
        let hook = |pid: i32| (self.attach_slirp)(pid, &l.slirp_ports);
        let (dns, hosts_ip) = (self.addresses)(l);
        let spec = crate::launch_spec::run_spec(c, l, dns, hosts_ip, &hook);
        crate::supervise::run_supervised(
            self.store,
            c,
            &l.rootfs,
            &spec,
            policy,
            &crate::supervise::Supervision {
                state_root: self.state_root,
                on_first_start: self.on_first_start,
                silent_death: self.silent_death,
            },
        )?;
        (self.on_supervised)(c);
        Ok(())
    }

    fn discard_unstarted(&self, id: &str) {
        (self.discard)(id);
    }
}

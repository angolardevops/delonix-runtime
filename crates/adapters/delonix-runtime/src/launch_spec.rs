//! The one builder of the engine's spawn specification.
//!
//! `run` and `start` both start a container from a [`Launch`], and both go
//! through [`run_spec`]. Two literals of `RunSpec` diverged SIX times (`-v`, `-p`
//! on a custom network, extra networks, pod membership, DNS, confinement): a
//! field set in one builder and forgotten in the other is lost on the first
//! restart, in silence.

use crate::{DnsConfig, RunSpec, StartedHook};
use delonix_compute::launch::Launch;
use delonix_runtime_core::Container;

/// The spawn specification for container `c` started as `l`.
///
/// `dns` and `hosts_ip` are the network's answers (the resolver to write in
/// `/etc/resolv.conf` and the address for `/etc/hosts`); the caller asks the
/// network for them, because this crate does not know the node's networks.
pub fn run_spec<'h>(
    c: &Container,
    l: &Launch,
    dns: Option<String>,
    hosts_ip: Option<String>,
    slirp_hook: &'h StartedHook<'h>,
) -> RunSpec<'h> {
    RunSpec {
        dns_config: dns_config_of(c),
        detach: l.detach,
        new_netns: l.new_netns(),
        pod_infra_pid: l.pod_infra_pid(),
        userns: l.userns(c),
        inherit_userns: l.inherit_userns(),
        log_path: l.log_path.clone(),
        mounts: l.mounts.clone(),
        on_started: if l.slirp_ports.is_empty() {
            None
        } else {
            Some(slirp_hook)
        },
        hosts_ip,
        dns,
        host_pid: c.host_pid,
        host_ipc: c.host_ipc,
        apparmor: l.apparmor.clone(),
        selinux: c.selinux.clone(),
        log_cri: c.log_cri,
        run_uid: c.run_uid,
        run_gid: c.run_gid,
    }
}

/// The explicit resolver a container was created with, if any.
///
/// Part of the one builder on purpose: the DNS a container gets on a `restart`
/// has to be the one it was created with. Every field of this family that only
/// the creation path read has ended up lost on the first restart — `-v`, `-p` on
/// a custom network, extra networks, pod membership — and `--dns` was the fifth:
/// it held until the first `stop`+`start` and then resolved through the host's
/// resolver, silently (measured).
pub fn dns_config_of(c: &Container) -> Option<DnsConfig> {
    let cfg = DnsConfig {
        servers: c.dns_servers.clone(),
        searches: c.dns_searches.clone(),
        options: c.dns_options.clone(),
    };
    (!cfg.is_empty()).then_some(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container() -> Container {
        Container::new("abc".into(), "n".into(), "img".into(), vec![], "64M".into())
    }

    fn launch() -> Launch {
        Launch {
            slirp_ports: vec![],
            rootfs: "/r".into(),
            mounts: vec![],
            detach: true,
            second_pass: false,
            net_none: false,
            custom_net: None,
            pod: false,
            attached_ip: None,
            pod_infra_pid: None,
            apparmor: None,
            log_path: None,
        }
    }

    #[test]
    fn an_explicit_dns_reaches_the_spec() {
        // `run` and `start` both build here, so a `--dns` on the record reaches
        // the spec on a restart as well.
        let mut c = container();
        c.dns_servers = vec!["1.1.1.1".into()];
        let hook = |_: i32| Ok(());
        let spec = run_spec(&c, &launch(), None, None, &hook);
        assert_eq!(
            spec.dns_config.map(|d| d.servers),
            Some(vec!["1.1.1.1".to_string()])
        );
    }

    #[test]
    fn the_network_answers_are_passed_through() {
        let c = container();
        let hook = |_: i32| Ok(());
        let spec = run_spec(
            &c,
            &launch(),
            Some("10.0.2.3".into()),
            Some("10.0.2.100".into()),
            &hook,
        );
        assert_eq!(spec.dns.as_deref(), Some("10.0.2.3"));
        assert_eq!(spec.hosts_ip.as_deref(), Some("10.0.2.100"));
        assert!(spec.dns_config.is_none());
        assert!(spec.on_started.is_none(), "no ports, no slirp hook");
    }
}

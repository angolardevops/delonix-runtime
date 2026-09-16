//! The `container run` network phase over [`NetworkProvider`]
//! (`docs/discovery/54_P2_COMPUTE_RUN.md`, step 3b).
//!
//! A custom network is wired in two passes. The first attaches the container and
//! registers its `--expose` route; the caller then re-executes into the
//! network's namespace (a process mechanism, which stays in the interface). The
//! pass that starts the container publishes its ports and records the network,
//! the address, the namespace isolation, the route and the shaping — BEFORE the
//! workload starts, so a supervised `-d` gets all of them (#344 measured the
//! opposite) and a refusal undoes the attach instead of leaving a running
//! container behind an error.

use delonix_runtime_core::{Container, Result};

use crate::ports::NetworkProvider;
use crate::{Notice, RunOpts};

/// The first pass on a custom network: attach and register the `--expose`
/// route. Returns the network namespace and the address to re-execute into.
pub fn attach_custom_network<N: NetworkProvider>(
    o: &RunOpts,
    container: &Container,
    network: &str,
    net: &N,
    notices: &mut Vec<Notice>,
) -> Result<(String, String)> {
    net.check_network(network)?;
    let (netns, ip) = net.attach(
        &container.id,
        network,
        &container.namespace,
        o.ip.as_deref(),
    )?;
    if let Some(port) = o.expose {
        if let Err(e) = net.register_expose(&container.name, &container.namespace, &ip, port) {
            notices.push(Notice::new(
                "warning: --expose of '{name}' not registered in the proxy: {e}",
                &[("name", &container.name), ("e", &e.to_string())],
            ));
        }
    }
    Ok((netns, ip))
}

/// The pass that starts the container: publish its ports on the attached
/// address, then record and apply what a custom network carries. On a refusal
/// the attach is undone and the error returned; a firewall that cannot be
/// applied is a warning, as it always was.
pub fn wire_network<N: NetworkProvider>(
    o: &RunOpts,
    c: &mut Container,
    custom_net: Option<&str>,
    attached_ip: Option<&str>,
    net: &N,
    notices: &mut Vec<Notice>,
) -> Result<()> {
    if o.expose.is_some() && custom_net.is_none() {
        notices.push(Notice::new(
            "warning: --expose requires `--net <network>` (the proxy reaches the container via its SDN IP) — ignored",
            &[],
        ));
    }
    if let Some(ip) = attached_ip {
        for spec in &c.ports {
            if let Err(e) = net.publish(ip, spec) {
                net.unpublish(c);
                net.detach(&c.id, ip);
                return Err(e);
            }
        }
    }
    let Some(network) = custom_net else {
        return Ok(());
    };
    c.network = Some(network.to_string());
    c.ip = attached_ip.map(str::to_string);
    // Namespace isolation: outside `default` the container gets the namespace
    // firewall (same-namespace accept, cross-namespace new connections dropped).
    if c.namespace != "default" {
        if let Some(ip) = c.ip.clone() {
            let mut fw = c.firewall.clone().unwrap_or_default();
            fw.enabled = true;
            fw.namespace = c.namespace.clone();
            match net.apply_firewall(&c.id, &ip, &fw) {
                Ok(()) => c.firewall = Some(fw),
                Err(e) => notices.push(Notice::new(
                    "warning: namespace isolation '{namespace}' not applied: {e}",
                    &[("namespace", &c.namespace), ("e", &e.to_string())],
                )),
            }
        }
    }
    // `--expose`: persisted, to re-register on `start` and de-register on `rm`;
    // the route itself was registered by the first pass.
    if let Some(port) = o.expose {
        c.expose = Some(port);
    }
    if let Some(bps) = &o.net_bps {
        if let Err(e) = net.shape(&c.id, bps, o.net_burst.as_deref()) {
            net.unpublish(c);
            if let Some(ip) = c.ip.clone() {
                net.detach(&c.id, &ip);
            }
            return Err(e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use delonix_runtime_core::{ContainerFw, Error};
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeNet {
        calls: RefCell<Vec<String>>,
        fail_publish: bool,
        fail_firewall: bool,
        fail_shape: bool,
    }

    impl FakeNet {
        fn log(&self, s: String) {
            self.calls.borrow_mut().push(s);
        }
    }

    impl NetworkProvider for FakeNet {
        fn check_network(&self, name: &str) -> Result<()> {
            self.log(format!("check {name}"));
            Ok(())
        }
        fn attach(
            &self,
            id: &str,
            network: &str,
            ns: &str,
            fixed: Option<&str>,
        ) -> Result<(String, String)> {
            self.log(format!("attach {id} {network} {ns} {fixed:?}"));
            Ok((format!("ns-{id}"), fixed.unwrap_or("10.0.0.5").to_string()))
        }
        fn detach(&self, id: &str, ip: &str) {
            self.log(format!("detach {id} {ip}"));
        }
        fn publish(&self, ip: &str, spec: &str) -> Result<()> {
            self.log(format!("publish {ip} {spec}"));
            if self.fail_publish {
                return Err(Error::Invalid("port taken".into()));
            }
            Ok(())
        }
        fn unpublish(&self, c: &Container) {
            self.log(format!("unpublish {}", c.id));
        }
        fn apply_firewall(&self, id: &str, _: &str, fw: &ContainerFw) -> Result<()> {
            self.log(format!("firewall {id} {}", fw.namespace));
            if self.fail_firewall {
                return Err(Error::Invalid("nft".into()));
            }
            Ok(())
        }
        fn shape(&self, id: &str, bps: &str, _: Option<&str>) -> Result<()> {
            self.log(format!("shape {id} {bps}"));
            if self.fail_shape {
                return Err(Error::Invalid("bad rate".into()));
            }
            Ok(())
        }
        fn register_expose(&self, name: &str, _: &str, ip: &str, port: u16) -> Result<()> {
            self.log(format!("expose {name} {ip}:{port}"));
            Ok(())
        }
    }

    fn container(ns: &str) -> Container {
        let mut c = Container::new(
            "c1".into(),
            "web".into(),
            "img".into(),
            vec!["sh".into()],
            "64M".into(),
        );
        c.namespace = ns.into();
        c.ports = vec!["8080:80".into()];
        c
    }

    #[test]
    fn the_first_pass_attaches_and_registers_the_route() {
        let net = FakeNet::default();
        let o = RunOpts {
            expose: Some(80),
            ip: Some("10.0.0.9".into()),
            ..Default::default()
        };
        let (netns, ip) =
            attach_custom_network(&o, &container("teamA"), "lab", &net, &mut Vec::new()).unwrap();
        assert_eq!((netns.as_str(), ip.as_str()), ("ns-c1", "10.0.0.9"));
        assert_eq!(
            *net.calls.borrow(),
            [
                "check lab",
                "attach c1 lab teamA Some(\"10.0.0.9\")",
                "expose web 10.0.0.9:80"
            ]
        );
    }

    #[test]
    fn the_start_pass_records_isolation_route_and_shaping_before_the_workload() {
        let net = FakeNet::default();
        let o = RunOpts {
            expose: Some(80),
            net_bps: Some("1mbit".into()),
            ..Default::default()
        };
        let mut c = container("teamA");
        let mut notices = Vec::new();
        wire_network(
            &o,
            &mut c,
            Some("lab"),
            Some("10.0.0.5"),
            &net,
            &mut notices,
        )
        .unwrap();
        assert_eq!(c.network.as_deref(), Some("lab"));
        assert_eq!(c.ip.as_deref(), Some("10.0.0.5"));
        assert_eq!(
            c.firewall.as_ref().map(|f| f.namespace.as_str()),
            Some("teamA")
        );
        assert_eq!(c.expose, Some(80));
        assert_eq!(
            *net.calls.borrow(),
            [
                "publish 10.0.0.5 8080:80",
                "firewall c1 teamA",
                "shape c1 1mbit"
            ]
        );
        assert!(notices.is_empty());
    }

    #[test]
    fn a_refused_publish_or_shaping_undoes_the_attach() {
        let net = FakeNet {
            fail_publish: true,
            ..Default::default()
        };
        let mut c = container("default");
        assert!(wire_network(
            &RunOpts::default(),
            &mut c,
            Some("lab"),
            Some("10.0.0.5"),
            &net,
            &mut Vec::new()
        )
        .is_err());
        assert!(net
            .calls
            .borrow()
            .contains(&"detach c1 10.0.0.5".to_string()));

        let net = FakeNet {
            fail_shape: true,
            ..Default::default()
        };
        let o = RunOpts {
            net_bps: Some("x".into()),
            ..Default::default()
        };
        let mut c = container("default");
        assert!(wire_network(
            &o,
            &mut c,
            Some("lab"),
            Some("10.0.0.5"),
            &net,
            &mut Vec::new()
        )
        .is_err());
        assert!(net
            .calls
            .borrow()
            .contains(&"detach c1 10.0.0.5".to_string()));
    }

    #[test]
    fn a_firewall_failure_is_a_warning_and_default_gets_none() {
        let net = FakeNet {
            fail_firewall: true,
            ..Default::default()
        };
        let mut c = container("teamA");
        let mut notices = Vec::new();
        wire_network(
            &RunOpts::default(),
            &mut c,
            Some("lab"),
            Some("10.0.0.5"),
            &net,
            &mut notices,
        )
        .unwrap();
        assert!(c.firewall.is_none());
        assert_eq!(notices.len(), 1);

        let net = FakeNet::default();
        let mut c = container("default");
        wire_network(
            &RunOpts::default(),
            &mut c,
            Some("lab"),
            Some("10.0.0.5"),
            &net,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!net.calls.borrow().iter().any(|l| l.starts_with("firewall")));
    }

    #[test]
    fn expose_without_a_custom_network_warns() {
        let o = RunOpts {
            expose: Some(80),
            ..Default::default()
        };
        let mut notices = Vec::new();
        wire_network(
            &o,
            &mut container("default"),
            None,
            None,
            &FakeNet::default(),
            &mut notices,
        )
        .unwrap();
        assert_eq!(notices.len(), 1);
    }
}

//! The service view: what a browser can open for a workload (ADR-0048, phase 1).
//!
//! One pure computation shared by `container ls`, `vm ls`, `stack ls` and the `describe`
//! verbs, so a listing cannot say one thing while another says something else — the
//! discipline `fw_rule_tail` already enforces for firewall rules.
//!
//! A row is `{fqdn, scheme, port}`. The name is DERIVED (`<name>.<ns>.svc.delonix.internal`
//! for a workload the proxy auto-registers, the route's own host for a declared route); the
//! port is the LISTENER the L7 proxy is reachable on, because a workload on the SDN is not
//! reachable from the host any other way.

use super::ingress_proxy::{self, ProxyConfig, Where};

/// One thing a browser can open.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct SvcRow {
    pub fqdn: String,
    pub scheme: &'static str,
    pub port: u16,
    pub url: String,
}

impl SvcRow {
    fn new(fqdn: String, tls: bool, port: u16) -> Self {
        let scheme = if tls { "https" } else { "http" };
        // The default port of the scheme is left out, as a browser would.
        let default = (scheme == "http" && port == 80) || (scheme == "https" && port == 443);
        let url = if default {
            format!("{scheme}://{fqdn}")
        } else {
            format!("{scheme}://{fqdn}:{port}")
        };
        SvcRow {
            fqdn,
            scheme,
            port,
            url,
        }
    }
}

/// What the proxy knows, read once per listing.
pub struct SvcIndex {
    auto: Vec<ingress_proxy::AutoRoute>,
    manual: Vec<ProxyConfig>,
}

impl SvcIndex {
    pub fn load() -> Self {
        SvcIndex {
            auto: ingress_proxy::auto_routes(),
            manual: [Where::Holder, Where::Host]
                .into_iter()
                .filter_map(ingress_proxy::read_manual_config)
                .collect(),
        }
    }

    /// The rows for the workload `name` of `namespace` that has address `ip` on the SDN
    /// (or, for a libvirt VM, on the host's `virbr`).
    pub fn rows(&self, name: &str, namespace: &str, ip: Option<&str>) -> Vec<SvcRow> {
        let ns = if namespace.is_empty() {
            "default"
        } else {
            namespace
        };
        let mut out: Vec<SvcRow> = Vec::new();
        // `container run --expose`: registered under the standard name, on the auto port.
        for a in self
            .auto
            .iter()
            .filter(|a| a.name == name && a.namespace == ns)
        {
            out.push(SvcRow::new(a.fqdn(), false, ingress_proxy::AUTO_HTTP_PORT));
        }
        // Declared routes: every host whose backend is this workload's address, on
        // every listener the route asked for.
        let Some(ip) = ip.filter(|i| !i.is_empty()) else {
            out.sort();
            out.dedup();
            return out;
        };
        let prefix = format!("{ip}:");
        for cfg in &self.manual {
            for r in cfg
                .routes
                .iter()
                .filter(|r| !r.host.is_empty() && r.backend.starts_with(&prefix))
            {
                let asked: Vec<&ingress_proxy::Listener> = cfg
                    .listeners
                    .iter()
                    .filter(|l| l.sources.contains(&r.source))
                    .collect();
                // A config from before listeners recorded who asked for them: every
                // listener of the instance may serve the host.
                let listeners: Vec<&ingress_proxy::Listener> =
                    if cfg.listeners.iter().all(|l| l.sources.is_empty()) {
                        cfg.listeners.iter().collect()
                    } else {
                        asked
                    };
                for l in listeners {
                    out.push(SvcRow::new(r.host.clone(), l.tls, l.port));
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// The cell a listing shows: the URLs joined, `-` when there are none (so
/// `Table::drop_uninformative` removes the column on a host that exposes nothing).
pub fn cell(rows: &[SvcRow]) -> String {
    if rows.is_empty() {
        return "-".to_string();
    }
    rows.iter()
        .map(|r| r.url.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ingress_proxy::{Listener, Route};

    fn cfg(host: &str, backend: &str, source: &str, port: u16, tls: bool) -> ProxyConfig {
        ProxyConfig {
            listeners: vec![Listener {
                port,
                tls,
                addr: None,
                sources: vec![source.into()],
            }],
            routes: vec![Route {
                host: host.into(),
                path: "/".into(),
                backend: backend.into(),
                source: source.into(),
            }],
            tls: None,
            published_hosts: Vec::new(),
            claims: Vec::new(),
            stamps: Vec::new(),
            bind: None,
        }
    }

    #[test]
    fn an_exposed_container_gets_the_standard_name_on_the_auto_port() {
        let idx = SvcIndex {
            auto: vec![ingress_proxy::AutoRoute {
                name: "web".into(),
                namespace: "dev".into(),
                ip: "10.0.0.5".into(),
                port: 80,
            }],
            manual: Vec::new(),
        };
        let r = idx.rows("web", "dev", Some("10.0.0.5"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "http://web.dev.svc.delonix.internal:8080");
        assert!(
            idx.rows("web", "prod", None).is_empty(),
            "another namespace is another service"
        );
    }

    #[test]
    fn a_declared_route_shows_its_host_on_its_own_listener_and_scheme() {
        let idx = SvcIndex {
            auto: Vec::new(),
            manual: vec![cfg("app.example.pt", "10.0.0.7:80", "r1", 443, true)],
        };
        let r = idx.rows("vm1", "default", Some("10.0.0.7"));
        assert_eq!(r[0].url, "https://app.example.pt");
        assert!(idx.rows("vm1", "default", Some("10.0.0.8")).is_empty());
        assert!(idx.rows("vm1", "default", None).is_empty());
    }

    #[test]
    fn the_scheme_comes_from_tls_not_from_the_port_number() {
        let idx = SvcIndex {
            auto: Vec::new(),
            manual: vec![cfg("a.pt", "10.0.0.7:80", "r1", 8443, false)],
        };
        assert_eq!(
            idx.rows("x", "default", Some("10.0.0.7"))[0].url,
            "http://a.pt:8443"
        );
    }

    #[test]
    fn an_empty_result_is_a_dash_so_the_column_can_disappear() {
        assert_eq!(cell(&[]), "-");
    }
}

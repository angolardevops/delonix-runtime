//! A VM's own firewall, as the engine asks for it (ADR-0052).
//!
//! The per-workload firewall of a CONTAINER is the Linux provider's nftables
//! chain in the holder (`delonix-sdn`). A VM on a REMOTE node is out of that
//! chain's reach: its traffic never crosses this host. What reaches it is the
//! firewall of the node it runs on, and [`VmBackend::apply_firewall`] is the
//! port through which a backend offers it. A backend without one refuses by
//! name (the default method), never accepts and ignores.
//!
//! One [`VmFirewallPolicy`] is the WHOLE desired state of one direction of one
//! VM — the same contract `kind: NetworkPolicy` has for a container: applying
//! it replaces what the engine wrote for that direction before, and leaves the
//! other direction alone.
//!
//! [`VmBackend::apply_firewall`]: crate::vm_backend::VmBackend::apply_firewall

use std::fmt;

/// Inbound or outbound, from the VM's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

impl Direction {
    /// `in`/`out` — the words both `ContainerFw` and the Proxmox API use.
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The transport a rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
    /// Any protocol. With a port it means TCP **and** UDP — the meaning
    /// `net ingress allow <c> 80` has for a container.
    Any,
}

impl Proto {
    pub fn as_str(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
            Proto::Any => "any",
        }
    }

    /// Parses `tcp`/`udp`/`any`. Anything else is refused — a protocol a
    /// backend would silently widen to "any" is a rule that allows more than
    /// it says.
    pub fn parse(s: &str) -> Option<Proto> {
        match s {
            "tcp" => Some(Proto::Tcp),
            "udp" => Some(Proto::Udp),
            "any" => Some(Proto::Any),
            _ => None,
        }
    }
}

/// One rule. `port: None` is every port; `peer: None` is every address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// `true` = allow, `false` = deny.
    pub allow: bool,
    pub proto: Proto,
    /// A port or an `n-m` range, already validated by the caller.
    pub port: Option<String>,
    /// The OTHER end: the source of an inbound rule, the destination of an
    /// outbound one. An IPv4 address or CIDR.
    pub peer: Option<String>,
}

impl Rule {
    /// The comparable rendering the reconciler keys on:
    /// `action|proto|port|peer|`. The same shape `cmd::firewall` renders a
    /// container's stored rule in, so a VM's plan reads like a container's.
    pub fn key(&self) -> String {
        format!(
            "{}|{}|{}|{}|",
            if self.allow { "allow" } else { "deny" },
            self.proto.as_str(),
            self.port.as_deref().unwrap_or("*"),
            self.peer.as_deref().unwrap_or("0.0.0.0/0"),
        )
    }
}

/// The desired (or observed) state of one direction of one VM's firewall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub direction: Direction,
    /// What happens to traffic no rule matches. `false` = drop.
    pub default_allow: bool,
    /// In evaluation order: the first match wins.
    pub rules: Vec<Rule>,
}

impl Policy {
    /// One direction of the policy IR (ADR-0059 D6) as a VM's own firewall
    /// policy: the lowering every VM backend receives.
    ///
    /// Refused, by name, because a VM firewall on a node cannot hold it with the
    /// IR's meaning:
    /// - a namespace, other-namespaces or selector peer — it names workloads on
    ///   this engine's SDN, which a VM filtered by its node is not on;
    /// - a guardrail — the engine's namespace isolation lives in the holder
    ///   chain, and a VM policy that silently dropped it would read as isolated;
    /// - ICMP, a logged rule, a stateless rule — the port has no field for them.
    ///
    /// A single-host prefix renders as the bare address, the form the node
    /// lists and the reconciler compares.
    pub fn from_ir(p: &delonix_net_rules::policy::Policy) -> Result<Policy, String> {
        use delonix_net_rules::policy as ir;
        let mut rules = Vec::with_capacity(p.rules.len());
        for (i, r) in p.rules.iter().enumerate() {
            let n = i + 1;
            let refuse =
                |what: &str| Err(format!("rule #{n}: {what} has no form in a VM firewall"));
            if r.guardrail {
                return refuse("an engine guardrail (namespace isolation)");
            }
            if r.log {
                return refuse("a logged rule");
            }
            if !r.stateful {
                return refuse("a stateless rule");
            }
            let proto = match r.proto {
                ir::Proto::Tcp => Proto::Tcp,
                ir::Proto::Udp => Proto::Udp,
                ir::Proto::Any if r.icmp_type.is_none() => Proto::Any,
                ir::Proto::Any | ir::Proto::Icmp => return refuse("an ICMP rule"),
            };
            let peer = match &r.peer {
                ir::Peer::Any => None,
                ir::Peer::Cidr(c) => {
                    let text = c.to_string_cidr();
                    Some(text.strip_suffix("/32").map(str::to_string).unwrap_or(text))
                }
                ir::Peer::Namespace(_) | ir::Peer::OtherNamespaces(_) | ir::Peer::Selector(_) => {
                    return refuse("a peer that names this engine's workloads");
                }
            };
            let port = r.ports.map(|p| {
                if p.first == p.last {
                    p.first.to_string()
                } else {
                    format!("{}-{}", p.first, p.last)
                }
            });
            rules.push(Rule {
                allow: r.action == ir::Action::Allow,
                proto,
                port,
                peer,
            });
        }
        Ok(Policy {
            direction: match p.direction {
                ir::Direction::Ingress => Direction::In,
                ir::Direction::Egress => Direction::Out,
            },
            default_allow: p.default == ir::Action::Allow,
            rules,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delonix_net_rules::policy as ir;
    use delonix_net_rules::Cidr;

    fn one(rule: ir::Rule) -> ir::Policy {
        ir::Policy {
            direction: ir::Direction::Ingress,
            default: ir::Action::Deny,
            rules: vec![rule],
        }
    }

    #[test]
    fn an_ir_rule_lowers_to_the_vm_rule_the_node_is_sent() {
        let rule = ir::Rule {
            ports: Some(ir::PortRange {
                first: 8000,
                last: 8080,
            }),
            ..ir::Rule::new(
                ir::Action::Deny,
                ir::Proto::Tcp,
                ir::Peer::Cidr(Cidr::parse("10.0.0.0/8").unwrap()),
            )
        };
        let p = Policy::from_ir(&one(rule)).unwrap();
        assert_eq!(p.direction, Direction::In);
        assert!(!p.default_allow);
        assert_eq!(p.rules[0].key(), "deny|tcp|8000-8080|10.0.0.0/8|");
        let host = ir::Rule::new(
            ir::Action::Allow,
            ir::Proto::Any,
            ir::Peer::Cidr(Cidr {
                base: Cidr::parse_addr("10.0.0.5").unwrap(),
                len: 32,
            }),
        );
        assert_eq!(
            Policy::from_ir(&one(host)).unwrap().rules[0].key(),
            "allow|any|*|10.0.0.5|"
        );
    }

    #[test]
    fn what_a_vm_firewall_cannot_hold_is_refused_by_name() {
        let base = || ir::Rule::new(ir::Action::Allow, ir::Proto::Tcp, ir::Peer::Any);
        let cases = [
            (
                ir::Rule {
                    guardrail: true,
                    ..base()
                },
                "guardrail",
            ),
            (
                ir::Rule {
                    log: true,
                    ..base()
                },
                "logged",
            ),
            (
                ir::Rule {
                    stateful: false,
                    ..base()
                },
                "stateless",
            ),
            (
                ir::Rule {
                    proto: ir::Proto::Icmp,
                    ..base()
                },
                "ICMP",
            ),
            (
                ir::Rule {
                    proto: ir::Proto::Any,
                    icmp_type: Some(8),
                    ..base()
                },
                "ICMP",
            ),
            (
                ir::Rule {
                    peer: ir::Peer::Namespace("a".into()),
                    ..base()
                },
                "workloads",
            ),
            (
                ir::Rule {
                    peer: ir::Peer::OtherNamespaces("a".into()),
                    ..base()
                },
                "workloads",
            ),
            (
                ir::Rule {
                    peer: ir::Peer::Selector(vec![]),
                    ..base()
                },
                "workloads",
            ),
        ];
        for (rule, word) in cases {
            let e = Policy::from_ir(&one(rule.clone())).unwrap_err();
            assert!(e.contains(word) && e.contains("rule #1"), "{rule:?}: {e}");
        }
    }
}

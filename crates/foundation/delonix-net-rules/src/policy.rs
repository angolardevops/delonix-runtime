//! The typed policy IR of ADR-0059 D6: one direction of one target, in order.
//!
//! A policy is a default action and a list of rules where **the first rule that
//! matches decides — the order is the priority**, with no separate number to
//! disagree with it. Every enforcement point (the nft chain in the holder, the
//! Proxmox per-VM firewall, the OPNsense filter) is a lowering of this type, and
//! [`evaluate`] is the reference semantics each lowering is checked against: a
//! lowering that gives a different verdict for any packet of the golden table
//! is wrong.
//!
//! Hand-written and dependency-free, like the rest of this crate, so the same
//! type can be computed where no engine runs.

use crate::Cidr;

/// What a rule, or a policy's default, does with a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Let it through.
    Allow,
    /// Drop it.
    Deny,
}

/// Which side of the target the policy guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Traffic TO the target; the peer is the source.
    Ingress,
    /// Traffic FROM the target; the peer is the destination.
    Egress,
}

/// The L4 protocol a rule matches. IPv4 only: an IPv6 peer is refused where a
/// policy is parsed (audit 62 P3), never carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    /// Every protocol — or, with ports, TCP and UDP only (a port needs a
    /// transport header).
    Any,
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
    /// ICMP.
    Icmp,
}

/// A destination port, or an inclusive range of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    /// The first port (1..=65535).
    pub first: u16,
    /// The last port, `>= first`.
    pub last: u16,
}

impl PortRange {
    /// One port.
    pub fn one(port: u16) -> Self {
        PortRange {
            first: port,
            last: port,
        }
    }

    /// Whether `port` is in the range.
    pub fn contains(&self, port: u16) -> bool {
        (self.first..=self.last).contains(&port)
    }
}

/// The other end of a packet a rule is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    /// Any address.
    Any,
    /// An IPv4 prefix.
    Cidr(Cidr),
    /// A workload of this engine in the named namespace.
    Namespace(String),
    /// A workload of this engine in any namespace BUT the named one. Traffic
    /// from outside the engine's workloads (the gateway, DNS, the internet)
    /// never matches it: it is the namespace isolation guardrail's peer.
    OtherNamespaces(String),
    /// A workload of this engine carrying every one of these labels (ADR-0024).
    Selector(Vec<(String, String)>),
}

/// One rule of a policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Allow or deny.
    pub action: Action,
    /// The protocol.
    pub proto: Proto,
    /// The destination port(s); `None` is every port.
    pub ports: Option<PortRange>,
    /// The ICMP type, for an ICMP rule; `None` is every type.
    pub icmp_type: Option<u8>,
    /// The other end.
    pub peer: Peer,
    /// Whether the return of a flow this rule admitted passes without being
    /// matched again. `false` needs the enforcement point's
    /// `firewall.stateless` capability.
    pub stateful: bool,
    /// Whether a match is logged.
    pub log: bool,
    /// The document that contributed the rule (ADR-0028's contribution ledger).
    pub origin: Option<String>,
    /// An immutable rule the engine owns. A user rule never removes it.
    pub guardrail: bool,
}

impl Rule {
    /// A stateful, unlogged user rule with every port and every ICMP type.
    pub fn new(action: Action, proto: Proto, peer: Peer) -> Self {
        Rule {
            action,
            proto,
            ports: None,
            icmp_type: None,
            peer,
            stateful: true,
            log: false,
            origin: None,
            guardrail: false,
        }
    }
}

/// One direction of one target: the rules in order, then the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// The side it guards.
    pub direction: Direction,
    /// The action for a packet no rule matched.
    pub default: Action,
    /// The rules; the first that matches decides.
    pub rules: Vec<Rule>,
}

impl Policy {
    /// A policy that lets everything through and has no rule.
    pub fn open(direction: Direction) -> Self {
        Policy {
            direction,
            default: Action::Allow,
            rules: Vec::new(),
        }
    }

    /// True when every rule is stateful, so the return of an admitted flow
    /// is let through before any rule is evaluated.
    pub fn is_stateful(&self) -> bool {
        self.rules.iter().all(|r| r.stateful)
    }
}

/// A packet, as a policy sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// The protocol.
    pub proto: Proto,
    /// The destination port (TCP/UDP).
    pub dport: Option<u16>,
    /// The ICMP type (ICMP).
    pub icmp_type: Option<u8>,
    /// The address of the other end (source on ingress, destination on egress).
    pub peer: u32,
    /// The namespace of the other end when it is a workload of this engine;
    /// `None` for anything else (the gateway, DNS, the internet).
    pub peer_namespace: Option<String>,
    /// The labels of the other end, when it is a workload of this engine.
    pub peer_labels: Vec<(String, String)>,
    /// True for the first packet of a flow; false for the return of one
    /// already admitted.
    pub new: bool,
}

impl Packet {
    /// A new TCP packet to `dport` from `peer` (`a.b.c.d`), outside any
    /// namespace. Panics on an address that does not parse — for tests and
    /// fixed tables.
    pub fn tcp(peer: &str, dport: u16) -> Self {
        Packet {
            proto: Proto::Tcp,
            dport: Some(dport),
            icmp_type: None,
            peer: Cidr::parse_addr(peer).expect("an IPv4 address"),
            peer_namespace: None,
            peer_labels: Vec::new(),
            new: true,
        }
    }

    /// The same packet with its protocol changed.
    pub fn with_proto(mut self, proto: Proto) -> Self {
        self.proto = proto;
        if proto == Proto::Icmp {
            self.dport = None;
        }
        self
    }

    /// The same packet, from a workload in `namespace`.
    pub fn from_workload(mut self, namespace: &str) -> Self {
        self.peer_namespace = Some(namespace.to_string());
        self
    }

    /// The same packet, as the return of an admitted flow.
    pub fn established(mut self) -> Self {
        self.new = false;
        self
    }
}

/// Whether `rule` matches `packet`.
pub fn matches(rule: &Rule, packet: &Packet) -> bool {
    let proto_ok = match rule.proto {
        Proto::Any if rule.ports.is_some() => matches!(packet.proto, Proto::Tcp | Proto::Udp),
        Proto::Any => true,
        p => p == packet.proto,
    };
    if !proto_ok {
        return false;
    }
    if let Some(range) = rule.ports {
        match packet.dport {
            Some(port) if range.contains(port) => {}
            _ => return false,
        }
    }
    if let Some(t) = rule.icmp_type {
        if packet.icmp_type != Some(t) {
            return false;
        }
    }
    match &rule.peer {
        Peer::Any => true,
        Peer::Cidr(c) => c.contains(packet.peer),
        Peer::Namespace(n) => packet.peer_namespace.as_deref() == Some(n.as_str()),
        Peer::OtherNamespaces(n) => packet.peer_namespace.as_deref().is_some_and(|ns| ns != n),
        Peer::Selector(want) => {
            packet.peer_namespace.is_some() && want.iter().all(|w| packet.peer_labels.contains(w))
        }
    }
}

/// The reference verdict of `policy` for `packet`: the return of an admitted
/// flow passes when every rule is stateful; otherwise the first rule that
/// matches decides, and the default decides the rest.
pub fn evaluate(policy: &Policy, packet: &Packet) -> Action {
    if !packet.new && policy.is_stateful() {
        return Action::Allow;
    }
    policy
        .rules
        .iter()
        .find(|r| matches(r, packet))
        .map_or(policy.default, |r| r.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(s: &str) -> Peer {
        Peer::Cidr(Cidr::parse(s).expect("cidr"))
    }

    #[test]
    fn the_first_rule_that_matches_decides() {
        let p = Policy {
            direction: Direction::Ingress,
            default: Action::Allow,
            rules: vec![
                Rule {
                    ports: Some(PortRange::one(22)),
                    ..Rule::new(Action::Allow, Proto::Tcp, cidr("10.0.0.0/8"))
                },
                Rule {
                    ports: Some(PortRange::one(22)),
                    ..Rule::new(Action::Deny, Proto::Tcp, Peer::Any)
                },
            ],
        };
        assert_eq!(evaluate(&p, &Packet::tcp("10.1.1.1", 22)), Action::Allow);
        assert_eq!(evaluate(&p, &Packet::tcp("11.1.1.1", 22)), Action::Deny);
        assert_eq!(evaluate(&p, &Packet::tcp("11.1.1.1", 80)), Action::Allow);
    }

    /// `any` with a port is TCP and UDP, never ICMP; `any` without one is
    /// every protocol.
    #[test]
    fn any_with_a_port_is_tcp_and_udp_only() {
        let with_port = Rule {
            ports: Some(PortRange::one(9999)),
            ..Rule::new(Action::Allow, Proto::Any, Peer::Any)
        };
        let base = Packet::tcp("1.2.3.4", 9999);
        assert!(matches(&with_port, &base));
        assert!(matches(&with_port, &base.clone().with_proto(Proto::Udp)));
        assert!(!matches(&with_port, &base.clone().with_proto(Proto::Icmp)));
        assert!(!matches(&with_port, &Packet::tcp("1.2.3.4", 80)));
        let no_port = Rule::new(Action::Allow, Proto::Any, Peer::Any);
        assert!(matches(&no_port, &base.with_proto(Proto::Icmp)));
    }

    #[test]
    fn a_namespace_peer_is_only_ever_a_workload() {
        let other = Rule::new(Action::Deny, Proto::Any, Peer::OtherNamespaces("a".into()));
        let pkt = Packet::tcp("10.200.0.5", 80);
        assert!(!matches(&other, &pkt), "the gateway is no workload");
        assert!(!matches(&other, &pkt.clone().from_workload("a")));
        assert!(matches(&other, &pkt.clone().from_workload("b")));
        let same = Rule::new(Action::Allow, Proto::Any, Peer::Namespace("a".into()));
        assert!(matches(&same, &pkt.clone().from_workload("a")));
        assert!(!matches(&same, &pkt));
        let sel = Rule::new(
            Action::Allow,
            Proto::Any,
            Peer::Selector(vec![("app".into(), "web".into())]),
        );
        let mut labelled = pkt.clone().from_workload("a");
        labelled.peer_labels = vec![("app".into(), "web".into()), ("tier".into(), "1".into())];
        assert!(matches(&sel, &labelled));
        assert!(!matches(&sel, &pkt.from_workload("a")));
    }

    #[test]
    fn the_return_of_a_flow_passes_only_when_every_rule_is_stateful() {
        let mut p = Policy {
            direction: Direction::Ingress,
            default: Action::Deny,
            rules: vec![Rule::new(Action::Deny, Proto::Any, Peer::Any)],
        };
        let back = Packet::tcp("1.2.3.4", 80).established();
        assert_eq!(evaluate(&p, &back), Action::Allow);
        p.rules[0].stateful = false;
        assert_eq!(evaluate(&p, &back), Action::Deny);
    }

    #[test]
    fn a_port_range_is_inclusive() {
        let r = PortRange {
            first: 8000,
            last: 8002,
        };
        assert!(r.contains(8000) && r.contains(8002) && !r.contains(8003));
    }
}

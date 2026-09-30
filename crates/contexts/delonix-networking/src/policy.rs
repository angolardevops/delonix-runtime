//! A workload's firewall record as the typed policy IR (ADR-0059 D6).
//!
//! `ContainerFw` stays the persisted form (no record migration); this is the
//! **total** parse from it to [`TargetPolicy`]. Total means one rule the IR
//! cannot hold refuses the whole set — a rule is never skipped, because a
//! skipped `deny` is a wider firewall than the one the operator wrote (S1,
//! audit 62 P0-2).
//!
//! The IR reproduces what the nft chain in the holder does today, and the
//! golden table below pins it: the stateful prologue, the user rules in
//! order, then the namespace isolation guardrail, then each direction's
//! default. The guardrail comes AFTER the user rules on purpose — an explicit
//! `allow` (a `kind: Dependency`) still admits one peer of another namespace —
//! and no user rule removes it (S1, audit 62 P0-3/P0-4).

use delonix_model::records::{ContainerFw, FwRule};
use delonix_net_rules::policy::{Action, Direction, Peer, Policy, PortRange, Proto, Rule};
use delonix_net_rules::Cidr;

use crate::Error;

/// Both directions of one workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPolicy {
    /// Traffic to the workload.
    pub ingress: Policy,
    /// Traffic from the workload.
    pub egress: Policy,
}

/// The namespace a record belongs to, as the isolation guardrail names it.
fn namespace_of(fw: &ContainerFw) -> String {
    let ns = fw.namespace.trim();
    if ns.is_empty() {
        "default".to_string()
    } else {
        ns.to_string()
    }
}

/// `fw` as the policy IR, or the reason the whole set is refused.
///
/// A disabled record is two open policies with no rule and no guardrail —
/// the empty chain the holder installs for it today.
pub fn from_container_fw(fw: &ContainerFw) -> Result<TargetPolicy, Error> {
    if !fw.enabled {
        return Ok(TargetPolicy {
            ingress: Policy::open(Direction::Ingress),
            egress: Policy::open(Direction::Egress),
        });
    }
    let default_of = |field: &str, value: &str| match value {
        "" | "allow" => Ok(Action::Allow),
        "deny" => Ok(Action::Deny),
        other => Err(refuse(format!(
            "{field} {other:?} is neither `allow` nor `deny`"
        ))),
    };
    let mut ingress = Policy {
        direction: Direction::Ingress,
        default: default_of("policyIn", &fw.policy_in)?,
        rules: Vec::new(),
    };
    let mut egress = Policy {
        direction: Direction::Egress,
        default: default_of("policyOut", &fw.policy_out)?,
        rules: Vec::new(),
    };
    for (i, r) in fw.rules.iter().enumerate() {
        let rule = rule_of(r).map_err(|why| refuse(format!("rule #{}: {why}", i + 1)))?;
        match r.dir.as_str() {
            "in" => ingress.rules.push(rule),
            "out" => egress.rules.push(rule),
            other => {
                return Err(refuse(format!(
                    "rule #{}: direction {other:?} is neither `in` nor `out`",
                    i + 1
                )))
            }
        }
    }
    // The namespace isolation guardrail, after the user rules. With an
    // explicit inbound intent (a rule or a deny default) only another
    // namespace's new flows are cut and the same namespace falls to the
    // default; without one, the same namespace is admitted first.
    let ns = namespace_of(fw);
    let explicit_in = ingress.default == Action::Deny || !ingress.rules.is_empty();
    if !explicit_in {
        ingress
            .rules
            .push(guardrail(Action::Allow, Peer::Namespace(ns.clone())));
    }
    ingress
        .rules
        .push(guardrail(Action::Deny, Peer::OtherNamespaces(ns)));
    Ok(TargetPolicy { ingress, egress })
}

fn guardrail(action: Action, peer: Peer) -> Rule {
    Rule {
        guardrail: true,
        ..Rule::new(action, Proto::Any, peer)
    }
}

fn refuse(why: String) -> Error {
    Error::PolicyNotRepresentable(format!(
        "firewall refused, nothing of this set was taken (ADR-0059 D6): {why}"
    ))
}

/// One persisted rule as an IR rule.
fn rule_of(r: &FwRule) -> Result<Rule, String> {
    let action = match r.action.as_str() {
        "allow" => Action::Allow,
        "deny" => Action::Deny,
        other => return Err(format!("action {other:?} is neither `allow` nor `deny`")),
    };
    let proto = match r.proto.as_str() {
        "" | "any" => Proto::Any,
        "tcp" => Proto::Tcp,
        "udp" => Proto::Udp,
        other => return Err(format!("proto {other:?} is not tcp, udp or any")),
    };
    let ports = match r.port.as_str() {
        "" | "*" => None,
        p => Some(port_range(p).ok_or_else(|| format!("port {p:?} is not a port or a range"))?),
    };
    let peer = peer_of(&r.src)?;
    Ok(Rule {
        ports,
        origin: r.origin.clone(),
        ..Rule::new(action, proto, peer)
    })
}

/// `n` or `n-m`, each in 1..=65535, `n <= m`.
fn port_range(p: &str) -> Option<PortRange> {
    let port = |s: &str| s.parse::<u16>().ok().filter(|n| *n >= 1);
    match p.split_once('-') {
        Some((a, b)) => {
            let (first, last) = (port(a)?, port(b)?);
            (first <= last).then_some(PortRange { first, last })
        }
        None => port(p).map(PortRange::one),
    }
}

/// The other end: empty, `*` and `0.0.0.0/0` are any address; otherwise an
/// IPv4 address or prefix. IPv6 is refused (audit 62 P3), never dropped.
fn peer_of(src: &str) -> Result<Peer, String> {
    match src {
        "" | "*" | "0.0.0.0/0" => Ok(Peer::Any),
        s if s.contains(':') => Err(format!(
            "peer {s:?} is IPv6; the policy is IPv4-only (audit 62 P3)"
        )),
        s if s.contains('/') => Cidr::parse(s)
            .filter(|_| {
                s.split('/')
                    .next()
                    .is_some_and(|a| a.split('.').count() == 4)
            })
            .map(Peer::Cidr)
            .ok_or_else(|| format!("peer {s:?} is not an IPv4 prefix")),
        s => Cidr::parse_addr(s)
            .map(|base| Peer::Cidr(Cidr { base, len: 32 }))
            .ok_or_else(|| format!("peer {s:?} is not an IPv4 address")),
    }
}

/// One direction of the IR as the perimeter filter rules of a gateway
/// appliance (ADR-0059 D6, F3d), for traffic to (`ingress`) or from
/// (`egress`) `target`: an alias name, a prefix or an address the appliance
/// resolves.
///
/// Each rule's identity on the appliance is its description, `<name>#<n>`,
/// and its position is `sequence = first_sequence + n`: measured on OPNsense
/// 26.1.2_5, pf loads filter rules in `sequence` order, so the IR's first
/// match is the appliance's. The default verdict is one more rule at the end,
/// `<name>#default`, so the policy means the same on an appliance whose own
/// default differs.
///
/// Refused, naming the rule, because the appliance has no engine namespace:
/// a namespace, other-namespaces or selector peer (never expanded into an
/// address snapshot that goes stale), and an engine guardrail. An ICMP type
/// is refused too: the appliance has the field, the lowering was not measured
/// against it.
pub fn gateway_rules(
    target: &str,
    policy: &Policy,
    name: &str,
    first_sequence: u32,
) -> Result<Vec<crate::gateway::GatewayRule>, Error> {
    use crate::gateway::{GatewayAction, GatewayRule};
    let refuse = |n: usize, what: &str| {
        Err(Error::PolicyNotRepresentable(format!(
            "gateway policy '{name}', rule #{n}: {what} has no form on the appliance (ADR-0059 D6)"
        )))
    };
    let action = |a: Action| match a {
        Action::Allow => GatewayAction::Pass,
        Action::Deny => GatewayAction::Block,
    };
    let ends = |peer: String| match policy.direction {
        Direction::Ingress => (peer, target.to_string()),
        Direction::Egress => (target.to_string(), peer),
    };
    let mut out = Vec::with_capacity(policy.rules.len() + 1);
    for (i, r) in policy.rules.iter().enumerate() {
        let n = i + 1;
        if r.guardrail {
            return refuse(n, "an engine guardrail (namespace isolation)");
        }
        if r.icmp_type.is_some() {
            return refuse(n, "an ICMP type");
        }
        let peer = match &r.peer {
            Peer::Any => "any".to_string(),
            Peer::Cidr(c) => {
                let text = c.to_string_cidr();
                text.strip_suffix("/32").map(str::to_string).unwrap_or(text)
            }
            Peer::Namespace(_) | Peer::OtherNamespaces(_) | Peer::Selector(_) => {
                return refuse(n, "a peer that names this engine's workloads");
            }
        };
        let protocol = match (r.proto, r.ports.is_some()) {
            (Proto::Tcp, _) => Some("TCP"),
            (Proto::Udp, _) => Some("UDP"),
            (Proto::Any, true) => Some("TCP/UDP"),
            (Proto::Any, false) => None,
            (Proto::Icmp, false) => Some("ICMP"),
            (Proto::Icmp, true) => return refuse(n, "an ICMP rule with a port"),
        };
        let (source, destination) = ends(peer);
        out.push(GatewayRule {
            description: format!("{name}#{n}"),
            source,
            destination,
            protocol: protocol.map(str::to_string),
            action: action(r.action),
            destination_port: r.ports.map(|p| {
                if p.first == p.last {
                    p.first.to_string()
                } else {
                    format!("{}-{}", p.first, p.last)
                }
            }),
            log: r.log,
            stateful: r.stateful,
            sequence: Some(first_sequence + i as u32),
        });
    }
    let (source, destination) = ends("any".to_string());
    out.push(GatewayRule {
        description: format!("{name}#default"),
        source,
        destination,
        action: action(policy.default),
        sequence: Some(first_sequence + policy.rules.len() as u32),
        ..GatewayRule::default()
    });
    Ok(out)
}

/// The golden table of ADR-0059 D6: a firewall record, a packet, and the
/// verdict the nft chain in the holder gives it today — the S1 fixes
/// included (one inbound rule keeps the namespace isolation; `any` with a
/// port is TCP and UDP only). Public so every lowering, in whatever crate,
/// is checked against the same cells: a lowering that gives a different
/// verdict for any of them is wrong.
pub mod golden {
    use super::*;
    use delonix_net_rules::policy::Packet;
    use Action::{Allow, Deny};
    use Direction::{Egress, Ingress};

    /// One cell.
    #[derive(Debug, Clone)]
    pub struct GoldenCase {
        /// What the cell checks.
        pub name: &'static str,
        /// The persisted record.
        pub record: ContainerFw,
        /// The direction the packet travels.
        pub direction: Direction,
        /// The packet.
        pub packet: Packet,
        /// The verdict.
        pub want: Action,
    }

    /// A record in namespace `team-a` with these defaults and rules
    /// (`dir, proto, port, peer, action`).
    pub fn fw(
        policy_in: &str,
        policy_out: &str,
        rules: &[(&str, &str, &str, &str, &str)],
    ) -> ContainerFw {
        ContainerFw {
            enabled: true,
            policy_in: policy_in.into(),
            policy_out: policy_out.into(),
            rules: rules
                .iter()
                .map(|(dir, proto, port, src, action)| FwRule {
                    dir: (*dir).into(),
                    proto: (*proto).into(),
                    port: (*port).into(),
                    src: (*src).into(),
                    action: (*action).into(),
                    ..Default::default()
                })
                .collect(),
            namespace: "team-a".into(),
        }
    }

    /// Every cell.
    pub fn cases() -> Vec<GoldenCase> {
        let c = |name, record, direction, packet, want| GoldenCase {
            name,
            record,
            direction,
            packet,
            want,
        };
        let open = || fw("", "", &[]);
        let deny22 = || fw("", "", &[("in", "tcp", "22", "", "deny")]);
        let deny_allow80 = || fw("deny", "", &[("in", "tcp", "80", "", "allow")]);
        let any9999 = || fw("deny", "", &[("in", "any", "9999", "", "allow")]);
        let ordered = || {
            fw(
                "",
                "",
                &[
                    ("in", "tcp", "22", "10.0.0.0/8", "allow"),
                    ("in", "tcp", "22", "", "deny"),
                ],
            )
        };
        let egress = || fw("", "deny", &[("out", "tcp", "443", "1.2.3.0/24", "allow")]);
        let host = || fw("deny", "", &[("in", "tcp", "22", "172.16.31.103", "allow")]);
        let gateway = Packet::tcp("10.200.0.1", 80);
        let same = |port| Packet::tcp("10.200.0.7", port).from_workload("team-a");
        let other = |port| Packet::tcp("10.200.0.9", port).from_workload("team-b");
        vec![
            // No rule: the same namespace and outsiders pass, another
            // namespace's new flows do not, and its returns do.
            c("open: same namespace", open(), Ingress, same(80), Allow),
            c("open: gateway", open(), Ingress, gateway.clone(), Allow),
            c("open: other namespace", open(), Ingress, other(80), Deny),
            c(
                "open: other namespace, return",
                open(),
                Ingress,
                other(80).established(),
                Allow,
            ),
            // One inbound deny keeps the isolation (S1 C1: any `in` rule
            // used to switch it off).
            c("deny 22: same ns to 22", deny22(), Ingress, same(22), Deny),
            c("deny 22: same ns to 80", deny22(), Ingress, same(80), Allow),
            c(
                "deny 22: other ns to 80",
                deny22(),
                Ingress,
                other(80),
                Deny,
            ),
            c(
                "deny 22: gateway to 80",
                deny22(),
                Ingress,
                gateway.clone(),
                Allow,
            ),
            // A deny default with one allow: the allow admits another
            // namespace's peer (a Dependency), the guardrail cuts the rest,
            // and the same namespace falls to the default.
            c(
                "deny + allow 80: other ns to 80",
                deny_allow80(),
                Ingress,
                other(80),
                Allow,
            ),
            c(
                "deny + allow 80: other ns to 22",
                deny_allow80(),
                Ingress,
                other(22),
                Deny,
            ),
            c(
                "deny + allow 80: same ns to 22",
                deny_allow80(),
                Ingress,
                same(22),
                Deny,
            ),
            c(
                "deny + allow 80: internet to 80",
                deny_allow80(),
                Ingress,
                Packet::tcp("8.8.8.8", 80),
                Allow,
            ),
            // `any` with a port is TCP and UDP only (the widening defect).
            c(
                "any 9999: tcp",
                any9999(),
                Ingress,
                Packet::tcp("8.8.8.8", 9999),
                Allow,
            ),
            c(
                "any 9999: udp",
                any9999(),
                Ingress,
                Packet::tcp("8.8.8.8", 9999).with_proto(Proto::Udp),
                Allow,
            ),
            c(
                "any 9999: icmp",
                any9999(),
                Ingress,
                Packet::tcp("8.8.8.8", 9999).with_proto(Proto::Icmp),
                Deny,
            ),
            c(
                "any 9999: tcp 80",
                any9999(),
                Ingress,
                Packet::tcp("8.8.8.8", 80),
                Deny,
            ),
            // First match wins.
            c(
                "order: 10/8 to 22",
                ordered(),
                Ingress,
                Packet::tcp("10.1.1.1", 22),
                Allow,
            ),
            c(
                "order: outside to 22",
                ordered(),
                Ingress,
                Packet::tcp("11.1.1.1", 22),
                Deny,
            ),
            // Egress: a deny default with one allowed destination.
            c(
                "egress: allowed",
                egress(),
                Egress,
                Packet::tcp("1.2.3.4", 443),
                Allow,
            ),
            c(
                "egress: other port",
                egress(),
                Egress,
                Packet::tcp("1.2.3.4", 80),
                Deny,
            ),
            c(
                "egress: other host",
                egress(),
                Egress,
                Packet::tcp("5.5.5.5", 443),
                Deny,
            ),
            c(
                "egress: return",
                egress(),
                Egress,
                Packet::tcp("5.5.5.5", 443).established(),
                Allow,
            ),
            // A bare address is a /32.
            c(
                "host: that host",
                host(),
                Ingress,
                Packet::tcp("172.16.31.103", 22),
                Allow,
            ),
            c(
                "host: its neighbour",
                host(),
                Ingress,
                Packet::tcp("172.16.31.104", 22),
                Deny,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delonix_net_rules::policy::evaluate;

    use golden::fw;
    use Action::Deny;

    /// Every cell of the golden table, through the reference evaluator.
    #[test]
    fn the_ir_gives_the_verdicts_the_holder_chain_gives() {
        for case in golden::cases() {
            let t =
                from_container_fw(&case.record).unwrap_or_else(|e| panic!("{}: {e}", case.name));
            let policy = match case.direction {
                Direction::Ingress => &t.ingress,
                Direction::Egress => &t.egress,
            };
            assert_eq!(evaluate(policy, &case.packet), case.want, "{}", case.name);
        }
    }

    #[test]
    fn a_disabled_record_is_open_both_ways_with_no_guardrail() {
        let mut record = fw("deny", "deny", &[("in", "tcp", "22", "", "deny")]);
        record.enabled = false;
        let t = from_container_fw(&record).unwrap();
        assert_eq!(t.ingress, Policy::open(Direction::Ingress));
        assert_eq!(t.egress, Policy::open(Direction::Egress));
    }

    /// Removing every user rule keeps the guardrail (P0-4), and the guardrail
    /// is marked as the engine's.
    #[test]
    fn the_namespace_guardrail_survives_every_user_rule() {
        let t = from_container_fw(&fw("", "", &[])).unwrap();
        assert!(t.ingress.rules.iter().all(|r| r.guardrail));
        assert!(t
            .ingress
            .rules
            .iter()
            .any(|r| r.peer == Peer::OtherNamespaces("team-a".into()) && r.action == Deny));
        let t = from_container_fw(&fw("deny", "", &[("in", "tcp", "80", "", "allow")])).unwrap();
        assert_eq!(t.ingress.rules.len(), 2);
        assert!(!t.ingress.rules[0].guardrail && t.ingress.rules[1].guardrail);
        assert!(
            t.egress.rules.is_empty(),
            "the guardrail guards ingress only"
        );
    }

    /// One rule the IR cannot hold refuses the whole set, and the error names
    /// the rule; it is an invalid intent (DX-1380), not a skipped rule.
    #[test]
    fn a_rule_the_ir_cannot_hold_refuses_the_whole_set() {
        let bad = [
            ("in", "tcp", "22", "::1/128", "allow"),
            ("in", "icmpv6", "", "", "allow"),
            ("in", "tcp", "0", "", "allow"),
            ("in", "tcp", "90-80", "", "allow"),
            ("in", "tcp", "22", "10.0.0/8", "allow"),
            ("in", "tcp", "22", "", "reject"),
            ("sideways", "tcp", "22", "", "allow"),
        ];
        for b in bad {
            let record = fw("", "", &[("in", "tcp", "80", "", "allow"), b]);
            let e = from_container_fw(&record).unwrap_err();
            assert!(e.to_string().contains("rule #2"), "{b:?}: {e}");
            assert_eq!(e.number(), 1380, "{b:?}");
        }
        let e = from_container_fw(&fw("drop", "", &[])).unwrap_err();
        assert!(e.to_string().contains("policyIn"), "{e}");
    }

    #[test]
    fn the_origin_travels_with_its_rule() {
        let mut record = fw("", "", &[("in", "tcp", "80", "", "allow")]);
        record.rules[0].origin = Some("web-access".into());
        let t = from_container_fw(&record).unwrap();
        assert_eq!(t.ingress.rules[0].origin.as_deref(), Some("web-access"));
    }

    /// The verdict the APPLIANCE gives a packet from the rules it was sent:
    /// an established flow passes on the state a keep-state rule created;
    /// otherwise pf's quick rules are evaluated in `sequence` order and the
    /// first match decides. The lowering always ends in a default rule, so a
    /// packet no rule matches is a lowering bug, and panics.
    fn appliance_verdict(
        rules: &[crate::gateway::GatewayRule],
        direction: Direction,
        target: u32,
        pkt: &delonix_net_rules::policy::Packet,
    ) -> Action {
        use crate::gateway::GatewayAction;
        if !pkt.new && rules.iter().all(|r| r.stateful) {
            return Action::Allow;
        }
        let (src, dst) = match direction {
            Direction::Ingress => (pkt.peer, target),
            Direction::Egress => (target, pkt.peer),
        };
        let addr_ok = |text: &str, addr: u32| match text {
            "any" => true,
            t if t.contains('/') => Cidr::parse(t).unwrap().contains(addr),
            t => Cidr::parse_addr(t).unwrap() == addr,
        };
        let mut sorted: Vec<_> = rules.iter().collect();
        sorted.sort_by_key(|r| r.sequence);
        for r in sorted {
            let proto_ok = match r.protocol.as_deref() {
                None => true,
                Some("TCP") => pkt.proto == Proto::Tcp,
                Some("UDP") => pkt.proto == Proto::Udp,
                Some("TCP/UDP") => matches!(pkt.proto, Proto::Tcp | Proto::Udp),
                Some("ICMP") => pkt.proto == Proto::Icmp,
                Some(other) => panic!("the lowering sent protocol {other}"),
            };
            let port_ok = match &r.destination_port {
                None => true,
                Some(p) => pkt.dport.is_some_and(|d| match p.split_once('-') {
                    Some((a, b)) => (a.parse().unwrap()..=b.parse().unwrap()).contains(&d),
                    None => p.parse::<u16>().unwrap() == d,
                }),
            };
            if proto_ok && port_ok && addr_ok(&r.source, src) && addr_ok(&r.destination, dst) {
                return match r.action {
                    GatewayAction::Pass => Action::Allow,
                    GatewayAction::Block => Action::Deny,
                };
            }
        }
        panic!("no rule matched: the default rule is missing")
    }

    /// Every golden cell through the gateway lowering: the IR the record
    /// parses to, minus the namespace guardrails (the appliance has no engine
    /// namespace, and the lowering refuses them — checked here), lowered for
    /// the workload's address. The appliance's verdict has to be the
    /// reference evaluator's verdict for the same IR.
    #[test]
    fn the_gateway_rules_give_every_golden_verdict_of_the_ir_they_came_from() {
        let target = "10.200.0.5";
        let addr = Cidr::parse_addr(target).unwrap();
        let cases = golden::cases();
        assert_eq!(
            cases.len(),
            24,
            "the golden table changed; recount what this covers"
        );
        for case in cases {
            let t = from_container_fw(&case.record).unwrap();
            let mut ir = match case.direction {
                Direction::Ingress => t.ingress,
                Direction::Egress => t.egress,
            };
            if ir.rules.iter().any(|r| r.guardrail) {
                let e = gateway_rules(target, &ir, "p", 100).unwrap_err();
                assert!(e.to_string().contains("guardrail"), "{}: {e}", case.name);
                ir.rules.retain(|r| !r.guardrail);
            }
            let rules = gateway_rules(target, &ir, "p", 100).unwrap();
            assert_eq!(
                appliance_verdict(&rules, case.direction, addr, &case.packet),
                evaluate(&ir, &case.packet),
                "{}: {rules:?}",
                case.name
            );
        }
    }

    #[test]
    fn a_gateway_lowering_names_positions_and_ends_in_the_default() {
        use crate::gateway::GatewayAction;
        let ir = Policy {
            direction: Direction::Egress,
            default: Action::Deny,
            rules: vec![Rule {
                ports: Some(PortRange {
                    first: 8000,
                    last: 8080,
                }),
                log: true,
                stateful: false,
                ..Rule::new(
                    Action::Allow,
                    Proto::Any,
                    Peer::Cidr(Cidr::parse("10.9.0.0/24").unwrap()),
                )
            }],
        };
        let rules = gateway_rules("web", &ir, "egress-web", 200).unwrap();
        assert_eq!(rules.len(), 2);
        let r = &rules[0];
        assert_eq!(
            (r.description.as_str(), r.sequence),
            ("egress-web#1", Some(200))
        );
        assert_eq!(
            (r.source.as_str(), r.destination.as_str()),
            ("web", "10.9.0.0/24")
        );
        assert_eq!(r.protocol.as_deref(), Some("TCP/UDP"));
        assert_eq!(r.destination_port.as_deref(), Some("8000-8080"));
        assert!(r.log && !r.stateful);
        assert_eq!(r.action, GatewayAction::Pass);
        let d = &rules[1];
        assert_eq!(
            (d.description.as_str(), d.sequence),
            ("egress-web#default", Some(201))
        );
        assert_eq!((d.source.as_str(), d.destination.as_str()), ("web", "any"));
        assert_eq!(d.action, GatewayAction::Block);
    }

    #[test]
    fn what_the_appliance_cannot_hold_is_refused_by_name() {
        let base = || Rule::new(Action::Allow, Proto::Tcp, Peer::Any);
        for (rule, word) in [
            (
                Rule {
                    peer: Peer::Namespace("a".into()),
                    ..base()
                },
                "workloads",
            ),
            (
                Rule {
                    peer: Peer::OtherNamespaces("a".into()),
                    ..base()
                },
                "workloads",
            ),
            (
                Rule {
                    peer: Peer::Selector(vec![]),
                    ..base()
                },
                "workloads",
            ),
            (
                Rule {
                    guardrail: true,
                    ..base()
                },
                "guardrail",
            ),
            (
                Rule {
                    proto: Proto::Icmp,
                    icmp_type: Some(8),
                    ..base()
                },
                "ICMP type",
            ),
        ] {
            let ir = Policy {
                direction: Direction::Ingress,
                default: Action::Deny,
                rules: vec![rule],
            };
            let e = gateway_rules("t", &ir, "p", 1).unwrap_err();
            assert!(
                e.to_string().contains(word) && e.to_string().contains("rule #1"),
                "{e}"
            );
            assert_eq!(e.number(), 1380, "{e}");
        }
    }
}

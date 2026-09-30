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
}

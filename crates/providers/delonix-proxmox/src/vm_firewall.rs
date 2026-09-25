//! The engine's per-VM firewall port, answered by the node's OWN firewall
//! (ADR-0052).
//!
//! A `NetworkPolicy` with `scope: vm` is one direction of one VM, whole: the
//! default verdict plus the ordered rules. On Proxmox that lands in three
//! places, and a VM's rules filter nothing unless all three agree — which is
//! why [`apply`] checks or sets each of them instead of trusting that a rule
//! written is a rule enforced:
//!
//! 1. the cluster's DATACENTER firewall `enable` — read, never written: turning
//!    it on changes what every node of the cluster accepts, so it is refused
//!    with [`Error::DatacenterFirewallDisabled`] instead;
//! 2. the VM's own `enable` and `policy_in`/`policy_out` options;
//! 3. `firewall=1` on the VM's `net0`.
//!
//! # Ownership
//!
//! The node keeps rules by POSITION and nothing else, and an operator can add
//! rules by hand in the node's UI. So every rule this engine writes carries a
//! comment `delonix-managed:<n>` (`n` = the index of the policy rule it came
//! from), and only rules carrying it are ever deleted or read back. A hand-made
//! rule is never touched — and never reported as drift either, which is the
//! stated limit: the engine owns ITS rules, not the VM's whole table.
//!
//! The node inserts a new rule at the TOP (position 0), so the engine's rules
//! are written in reverse to come out in policy order, above any hand-made
//! rule. The live case reads the order back rather than trusting this.

use crate::error::{Error, Result};
use crate::{Client, FirewallRuleOpts, Ledger};
use delonix_vm::firewall::{Direction, Policy, Proto, Rule};

/// The comment prefix that marks a rule as written by this engine.
pub const MANAGED: &str = "delonix-managed";

/// One rule as the node stores it — what [`node_rules`] plans and [`apply`]
/// sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRule {
    /// Index of the policy rule this came from (a `proto: any` with a port is
    /// TWO node rules with the same index — the node refuses a `dport` without
    /// a `proto`).
    pub index: usize,
    pub action: &'static str,
    pub proto: Option<&'static str>,
    /// `n` or `n:m` — the node's own range syntax.
    pub dport: Option<String>,
    pub peer: Option<String>,
}

impl NodeRule {
    pub fn comment(&self) -> String {
        format!("{MANAGED}:{}", self.index)
    }
}

/// The policy's rules, as the node rules that express them, in policy order.
/// Pure.
pub fn node_rules(policy: &Policy) -> Vec<NodeRule> {
    let mut out = Vec::new();
    for (index, r) in policy.rules.iter().enumerate() {
        let action = if r.allow { "ACCEPT" } else { "DROP" };
        let dport = r.port.as_ref().map(|p| p.replace('-', ":"));
        let peer = r.peer.clone().filter(|p| p != "0.0.0.0/0" && !p.is_empty());
        let protos: Vec<Option<&'static str>> = match (r.proto, dport.is_some()) {
            (Proto::Tcp, _) => vec![Some("tcp")],
            (Proto::Udp, _) => vec![Some("udp")],
            (Proto::Any, true) => vec![Some("tcp"), Some("udp")],
            (Proto::Any, false) => vec![None],
        };
        for proto in protos {
            out.push(NodeRule {
                index,
                action,
                proto,
                dport: dport.clone(),
                peer: peer.clone(),
            });
        }
    }
    out
}

/// The positions of this engine's rules in one direction, highest first — the
/// order they have to be deleted in, since deleting shifts every rule below.
/// Pure.
pub fn managed_positions(rules: &[serde_json::Value], direction: Direction) -> Vec<u32> {
    let mut out: Vec<u32> = rules
        .iter()
        .filter(|r| {
            is_managed(r) && r.get("type").and_then(|t| t.as_str()) == Some(direction.as_str())
        })
        .filter_map(|r| r.get("pos").and_then(|p| p.as_u64()).map(|p| p as u32))
        .collect();
    out.sort_unstable_by(|a, b| b.cmp(a));
    out
}

fn is_managed(r: &serde_json::Value) -> bool {
    r.get("comment")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.starts_with(&format!("{MANAGED}:")))
}

/// The policy the node holds for one direction: the default verdict from the
/// VM's options, and this engine's rules in position order, a TCP+UDP pair of
/// the same index folded back into `any`. Pure.
///
/// An absent `policy_in` is `DROP` and an absent `policy_out` is `ACCEPT` —
/// the node's own defaults for a VM, which is what it enforces when nobody set
/// them.
pub fn policy_from_node(
    options: &serde_json::Value,
    rules: &[serde_json::Value],
    direction: Direction,
) -> Policy {
    let key = match direction {
        Direction::In => "policy_in",
        Direction::Out => "policy_out",
    };
    let verdict = options
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(match direction {
            Direction::In => "DROP",
            Direction::Out => "ACCEPT",
        });
    let mut mine: Vec<&serde_json::Value> = rules
        .iter()
        .filter(|r| {
            is_managed(r) && r.get("type").and_then(|t| t.as_str()) == Some(direction.as_str())
        })
        .collect();
    mine.sort_by_key(|r| r.get("pos").and_then(|p| p.as_u64()).unwrap_or(u64::MAX));
    let peer_key = match direction {
        Direction::In => "source",
        Direction::Out => "dest",
    };
    let mut out: Vec<(String, Rule)> = Vec::new();
    for r in mine {
        let s = |k: &str| r.get(k).and_then(|v| v.as_str()).filter(|v| !v.is_empty());
        let comment = s("comment").unwrap_or_default().to_string();
        let proto = match s("proto") {
            Some("tcp") => Proto::Tcp,
            Some("udp") => Proto::Udp,
            _ => Proto::Any,
        };
        let rule = Rule {
            allow: s("action") == Some("ACCEPT"),
            proto,
            port: s("dport").map(|p| p.replace(':', "-")),
            peer: s(peer_key).map(str::to_string),
        };
        // The second half of an expanded `any`: same comment, same everything
        // but the transport.
        if let Some((c, prev)) = out.last_mut() {
            if *c == comment
                && prev.port == rule.port
                && prev.peer == rule.peer
                && prev.allow == rule.allow
                && matches!(
                    (prev.proto, rule.proto),
                    (Proto::Tcp, Proto::Udp) | (Proto::Udp, Proto::Tcp)
                )
            {
                prev.proto = Proto::Any;
                continue;
            }
        }
        out.push((comment, rule));
    }
    Policy {
        direction,
        default_allow: verdict == "ACCEPT",
        rules: out.into_iter().map(|(_, r)| r).collect(),
    }
}

/// `net0` with `firewall=1`, or `None` when it already has it. Pure.
pub fn net0_with_firewall(net0: &str) -> Option<String> {
    let parts: Vec<&str> = net0.split(',').collect();
    if parts.iter().any(|p| p.trim() == "firewall=1") {
        return None;
    }
    let mut kept: Vec<&str> = parts
        .into_iter()
        .filter(|p| !p.trim().starts_with("firewall="))
        .collect();
    kept.push("firewall=1");
    Some(kept.join(","))
}

/// Whether the datacenter-level switch is on. Pure. The node answers the
/// option as a number; a string is accepted too rather than read as off.
pub fn datacenter_enabled(options: &serde_json::Value) -> bool {
    match options.get("enable") {
        Some(v) => v.as_u64() == Some(1) || v.as_str() == Some("1"),
        None => false,
    }
}

/// Replaces this engine's rules and default verdict for one direction of a VM.
///
/// Order, and why: the datacenter check first (refuse before touching
/// anything); then the NIC switch and the VM's `enable` — so the VM is never
/// left with rules that look present and filter nothing; then the old managed
/// rules out, the new ones in, and the default verdict LAST, so a `deny`
/// default is never in force with the allow rules not yet written.
pub fn apply(client: &Client, ledger: &Ledger, vmid: u32, policy: &Policy) -> Result<()> {
    let dc = client.cluster_firewall_options()?;
    if !datacenter_enabled(&dc) {
        return Err(Error::DatacenterFirewallDisabled(format!(
            "proxmox: the datacenter firewall of this cluster is off, so no rule of VM {vmid} \
             would filter anything — enable it (Datacenter › Firewall › Options) before applying \
             a `scope: vm` policy; this engine never turns it on"
        )));
    }
    client.settle_pending(ledger, vmid)?;
    client.ensure_nic_firewall(ledger, vmid)?;
    client.set_firewall_enabled(ledger, vmid, true)?;

    let existing = client.firewall_rules(vmid)?;
    for pos in managed_positions(&existing, policy.direction) {
        client.delete_firewall_rule(ledger, vmid, pos)?;
    }
    // The node inserts at the top: write in reverse to read back in order.
    for r in node_rules(policy).iter().rev() {
        let comment = r.comment();
        let (source, dest) = match policy.direction {
            Direction::In => (r.peer.as_deref(), None),
            Direction::Out => (None, r.peer.as_deref()),
        };
        let opts = FirewallRuleOpts {
            enable: Some(true),
            comment: Some(&comment),
            source,
            dest,
            proto: r.proto,
            dport: r.dport.as_deref(),
            sport: None,
            iface: None,
            macro_name: None,
            rule_type: None,
            action: None,
        };
        client.add_firewall_rule(ledger, vmid, policy.direction.as_str(), r.action, &opts)?;
    }
    let verdict = if policy.default_allow {
        "ACCEPT"
    } else {
        "DROP"
    };
    client.set_firewall_policy(ledger, vmid, policy.direction.as_str(), verdict)
}

/// What the node holds for one direction (see [`policy_from_node`]).
pub fn read(client: &Client, vmid: u32, direction: Direction) -> Result<Policy> {
    let options = client.firewall_options(vmid)?;
    let rules = client.firewall_rules(vmid)?;
    Ok(policy_from_node(&options, &rules, direction))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(allow: bool, proto: Proto, port: Option<&str>, peer: Option<&str>) -> Rule {
        Rule {
            allow,
            proto,
            port: port.map(str::to_string),
            peer: peer.map(str::to_string),
        }
    }

    fn policy(dir: Direction, rules: Vec<Rule>) -> Policy {
        Policy {
            direction: dir,
            default_allow: false,
            rules,
        }
    }

    #[test]
    fn any_with_a_port_becomes_tcp_and_udp_because_the_node_needs_a_proto() {
        let p = policy(
            Direction::In,
            vec![rule(true, Proto::Any, Some("53"), None)],
        );
        let n = node_rules(&p);
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].proto, Some("tcp"));
        assert_eq!(n[1].proto, Some("udp"));
        assert!(n
            .iter()
            .all(|r| r.index == 0 && r.comment() == "delonix-managed:0"));
    }

    #[test]
    fn any_without_a_port_stays_one_rule_with_no_proto() {
        let p = policy(
            Direction::In,
            vec![rule(true, Proto::Any, None, Some("10.0.0.0/8"))],
        );
        let n = node_rules(&p);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].proto, None);
        assert_eq!(n[0].peer.as_deref(), Some("10.0.0.0/8"));
    }

    #[test]
    fn a_range_uses_the_nodes_colon_and_everyone_is_no_peer() {
        let p = policy(
            Direction::Out,
            vec![rule(
                false,
                Proto::Tcp,
                Some("8000-8080"),
                Some("0.0.0.0/0"),
            )],
        );
        let n = node_rules(&p);
        assert_eq!(n[0].dport.as_deref(), Some("8000:8080"));
        assert_eq!(n[0].peer, None);
        assert_eq!(n[0].action, "DROP");
    }

    fn node(
        pos: u64,
        ty: &str,
        comment: Option<&str>,
        proto: Option<&str>,
        dport: Option<&str>,
        source: Option<&str>,
    ) -> serde_json::Value {
        let mut v = json!({"pos": pos, "type": ty, "action": "ACCEPT", "enable": 1});
        if let Some(c) = comment {
            v["comment"] = json!(c);
        }
        if let Some(p) = proto {
            v["proto"] = json!(p);
        }
        if let Some(d) = dport {
            v["dport"] = json!(d);
        }
        if let Some(s) = source {
            v["source"] = json!(s);
        }
        v
    }

    #[test]
    fn only_managed_rules_of_the_direction_are_deleted_highest_first() {
        let rules = vec![
            node(
                0,
                "in",
                Some("delonix-managed:0"),
                Some("tcp"),
                Some("22"),
                None,
            ),
            node(1, "in", Some("hand-made"), Some("tcp"), Some("443"), None),
            node(2, "out", Some("delonix-managed:0"), None, None, None),
            node(
                3,
                "in",
                Some("delonix-managed:1"),
                Some("udp"),
                Some("53"),
                None,
            ),
            node(4, "in", None, None, None, None),
        ];
        assert_eq!(managed_positions(&rules, Direction::In), vec![3, 0]);
        assert_eq!(managed_positions(&rules, Direction::Out), vec![2]);
    }

    #[test]
    fn the_readback_is_the_policy_that_was_written() {
        let p = Policy {
            direction: Direction::In,
            default_allow: false,
            rules: vec![
                rule(true, Proto::Any, Some("53"), Some("10.0.0.0/8")),
                rule(true, Proto::Tcp, Some("22"), None),
                rule(false, Proto::Any, None, Some("192.168.1.0/24")),
            ],
        };
        // What the node would hold after `apply`: the node rules, in order,
        // positions from 0, plus a hand-made one below that must not appear.
        let mut stored: Vec<serde_json::Value> = node_rules(&p)
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mut v =
                    json!({"pos": i, "type": "in", "action": r.action, "comment": r.comment()});
                if let Some(x) = r.proto {
                    v["proto"] = json!(x);
                }
                if let Some(x) = &r.dport {
                    v["dport"] = json!(x);
                }
                if let Some(x) = &r.peer {
                    v["source"] = json!(x);
                }
                v
            })
            .collect();
        stored.push(node(
            9,
            "in",
            Some("hand-made"),
            Some("tcp"),
            Some("80"),
            None,
        ));
        let back = policy_from_node(
            &json!({"enable": 1, "policy_in": "DROP"}),
            &stored,
            Direction::In,
        );
        assert_eq!(back, p);
    }

    #[test]
    fn absent_options_read_as_the_nodes_own_defaults() {
        let opts = json!({});
        assert!(!policy_from_node(&opts, &[], Direction::In).default_allow);
        assert!(policy_from_node(&opts, &[], Direction::Out).default_allow);
    }

    #[test]
    fn net0_gets_firewall_once_and_keeps_its_mac() {
        let n = "virtio=BC:24:11:F4:F9:9C,bridge=vmbr0";
        assert_eq!(
            net0_with_firewall(n).as_deref(),
            Some("virtio=BC:24:11:F4:F9:9C,bridge=vmbr0,firewall=1")
        );
        assert_eq!(
            net0_with_firewall("virtio=AA,bridge=vmbr0,firewall=1"),
            None
        );
        assert_eq!(
            net0_with_firewall("virtio=AA,firewall=0,bridge=vmbr0").as_deref(),
            Some("virtio=AA,bridge=vmbr0,firewall=1")
        );
    }

    #[test]
    fn a_cluster_that_never_enabled_it_answers_only_a_digest_and_that_is_off() {
        // Measured on PVE 9.2.2 (the lab node), 2026-09-25.
        assert!(!datacenter_enabled(&json!({"digest": "da39a3ee"})));
        assert!(datacenter_enabled(&json!({"enable": 1})));
        assert!(!datacenter_enabled(&json!({"enable": 0})));
    }
}

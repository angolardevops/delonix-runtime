//! The nft lowering of the policy IR (ADR-0059 D6): the reference enforcement
//! point, the per-workload chain in the holder.
//!
//! [`chain_body`] renders a [`TargetPolicy`] as the rule lines of one address's
//! part of the chain, in the order the holder has always used: the ingress
//! user rules, the egress user rules, the namespace guardrail, then each
//! direction's default. The conntrack prologue is emitted once per chain by
//! `infra::fw_chain_prologue`, because state belongs to the flow and not to an
//! address.
//!
//! Every value that reaches the text comes from a typed IR field — an address,
//! a port number, a hashed set name — so nothing a record says is
//! interpolated raw. What nft here cannot hold with the IR's meaning is
//! refused by name instead of approximated.

use delonix_net_rules::policy::{Action, Direction, Peer, Policy, PortRange, Proto, Rule};
use delonix_networking::policy::TargetPolicy;

use crate::infra::DLXALL_SET;

/// The rule lines for the workload at `ip` (dotted IPv4). `ns_set` names the
/// nft set holding a namespace's workloads. `Err` names what the IR carries
/// and this lowering cannot represent.
pub fn chain_body(
    ip: &str,
    t: &TargetPolicy,
    ns_set: &dyn Fn(&str) -> String,
) -> Result<String, String> {
    let mut body = String::new();
    for r in t.ingress.rules.iter().filter(|r| !r.guardrail) {
        body.push_str(&user_line(ip, Direction::Ingress, r)?);
    }
    for r in t.egress.rules.iter().filter(|r| !r.guardrail) {
        body.push_str(&user_line(ip, Direction::Egress, r)?);
    }
    if t.egress.rules.iter().any(|r| r.guardrail) {
        return Err("an egress guardrail has no nft form here".into());
    }
    let guards: Vec<&Rule> = t.ingress.rules.iter().filter(|r| r.guardrail).collect();
    for (i, r) in guards.iter().enumerate() {
        let after_same_ns = i > 0
            && matches!(
                (&guards[i - 1].peer, &r.peer),
                (Peer::Namespace(a), Peer::OtherNamespaces(b)) if a == b
            );
        body.push_str(&guard_line(ip, r, after_same_ns, ns_set)?);
    }
    body.push_str(&default_line(ip, &t.ingress));
    body.push_str(&default_line(ip, &t.egress));
    Ok(body)
}

fn verdict(a: Action) -> &'static str {
    match a {
        Action::Allow => "counter accept",
        Action::Deny => "counter drop",
    }
}

fn ports(p: PortRange) -> String {
    if p.first == p.last {
        p.first.to_string()
    } else {
        format!("{}-{}", p.first, p.last)
    }
}

/// One user rule: the own-address anchor, the peer, the L4 match, the verdict.
fn user_line(ip: &str, dir: Direction, r: &Rule) -> Result<String, String> {
    if !r.stateful {
        return Err("a stateless rule (the holder chain is stateful)".into());
    }
    if r.log {
        return Err("a logged rule".into());
    }
    if r.icmp_type.is_some() || r.proto == Proto::Icmp {
        return Err("an ICMP rule".into());
    }
    let (self_dir, peer_dir) = match dir {
        Direction::Ingress => ("daddr", "saddr"),
        Direction::Egress => ("saddr", "daddr"),
    };
    let mut line = format!("\t\tip {self_dir} {ip} ");
    match &r.peer {
        Peer::Any => {}
        Peer::Cidr(c) => {
            let text = c.to_string_cidr();
            // A single host renders as a bare address, as the kernel lists it —
            // the counter reader matches on the listed text.
            let text = text.strip_suffix("/32").unwrap_or(&text);
            line.push_str(&format!("ip {peer_dir} {text} "));
        }
        Peer::Namespace(_) | Peer::OtherNamespaces(_) | Peer::Selector(_) => {
            return Err("a namespace or selector peer on a user rule".into());
        }
    }
    match (r.proto, r.ports) {
        (Proto::Tcp | Proto::Udp, p) => {
            line.push_str(if r.proto == Proto::Tcp { "tcp" } else { "udp" });
            if let Some(p) = p {
                line.push_str(&format!(" dport {}", ports(p)));
            }
            line.push(' ');
        }
        (Proto::Any, Some(p)) => {
            line.push_str(&format!(
                "meta l4proto {{ tcp, udp }} th dport {} ",
                ports(p)
            ));
        }
        (Proto::Any, None) => {}
        (Proto::Icmp, _) => unreachable!("refused above"),
    }
    line.push_str(verdict(r.action));
    line.push('\n');
    Ok(line)
}

/// One namespace guardrail line. The cut of other namespaces only needs the
/// `!= @<own set>` when the same namespace was not admitted just before it.
fn guard_line(
    ip: &str,
    r: &Rule,
    after_same_ns: bool,
    ns_set: &dyn Fn(&str) -> String,
) -> Result<String, String> {
    match (&r.peer, r.action) {
        (Peer::Namespace(ns), Action::Allow) => Ok(format!(
            "\t\tip daddr {ip} ip saddr @{} counter accept\n",
            ns_set(ns)
        )),
        (Peer::OtherNamespaces(_), Action::Deny) if after_same_ns => Ok(format!(
            "\t\tip daddr {ip} ip saddr @{DLXALL_SET} ct state new counter drop\n"
        )),
        (Peer::OtherNamespaces(ns), Action::Deny) => Ok(format!(
            "\t\tip daddr {ip} ip saddr @{DLXALL_SET} ip saddr != @{} ct state new counter drop\n",
            ns_set(ns)
        )),
        _ => Err("a guardrail other than the namespace isolation".into()),
    }
}

fn default_line(ip: &str, p: &Policy) -> String {
    match (p.direction, p.default) {
        (_, Action::Allow) => String::new(),
        (Direction::Ingress, Action::Deny) => format!("\t\tip daddr {ip} counter drop\n"),
        (Direction::Egress, Action::Deny) => format!("\t\tip saddr {ip} counter drop\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delonix_net_rules::policy::Packet;
    use delonix_net_rules::Cidr;
    use delonix_networking::policy::{from_container_fw, golden};

    const IP: &str = "10.200.0.5";

    fn ns_set(ns: &str) -> String {
        crate::infra::dlxns_set(&crate::infra::namespace_isolation_key(ns))
    }

    /// The verdict the rendered nft text gives a packet, read from the text
    /// itself: every line is parsed, every match is evaluated, and a token
    /// this evaluator does not know fails the test instead of being skipped.
    fn nft_verdict(body: &str, dir: Direction, pkt: &Packet) -> Action {
        let own = Cidr::parse_addr(IP).unwrap();
        let (src, dst) = match dir {
            Direction::Ingress => (pkt.peer, own),
            Direction::Egress => (own, pkt.peer),
        };
        let in_set = |set: &str| match &pkt.peer_namespace {
            None => false,
            Some(_) if set == DLXALL_SET => true,
            Some(ns) => ns_set(ns) == set,
        };
        let addr_matches = |value: &str, addr: u32, negate: bool| -> bool {
            let hit = if let Some(set) = value.strip_prefix('@') {
                // Sets hold workload addresses; only the peer can be in one.
                addr == pkt.peer && in_set(set)
            } else if value.contains('/') {
                Cidr::parse(value).expect("cidr").contains(addr)
            } else {
                Cidr::parse_addr(value).expect("addr") == addr
            };
            hit != negate
        };
        let port_matches = |spec: &str| -> bool {
            let Some(port) = pkt.dport else { return false };
            match spec.split_once('-') {
                Some((a, b)) => (a.parse().unwrap()..=b.parse().unwrap()).contains(&port),
                None => spec.parse::<u16>().unwrap() == port,
            }
        };
        for line in body.lines() {
            let t: Vec<&str> = line.split_whitespace().collect();
            let mut i = 0;
            let mut matched = true;
            let mut verdict = None;
            while i < t.len() {
                match t[i] {
                    "ip" => {
                        let addr = match t[i + 1] {
                            "saddr" => src,
                            "daddr" => dst,
                            other => panic!("unknown ip selector {other} in {line}"),
                        };
                        let (negate, value, step) = if t[i + 2] == "!=" {
                            (true, t[i + 3], 4)
                        } else {
                            (false, t[i + 2], 3)
                        };
                        matched &= addr_matches(value, addr, negate);
                        i += step;
                    }
                    "tcp" | "udp" => {
                        let want = if t[i] == "tcp" {
                            Proto::Tcp
                        } else {
                            Proto::Udp
                        };
                        matched &= pkt.proto == want;
                        if t.get(i + 1) == Some(&"dport") {
                            matched &= port_matches(t[i + 2]);
                            i += 3;
                        } else {
                            i += 1;
                        }
                    }
                    "meta" => {
                        assert_eq!(
                            &t[i..i + 9],
                            [
                                "meta",
                                "l4proto",
                                "{",
                                "tcp,",
                                "udp",
                                "}",
                                "th",
                                "dport",
                                t[i + 8]
                            ],
                            "{line}"
                        );
                        matched &=
                            matches!(pkt.proto, Proto::Tcp | Proto::Udp) && port_matches(t[i + 8]);
                        i += 9;
                    }
                    "ct" => {
                        assert_eq!(t[i + 1], "state", "{line}");
                        matched &= match t[i + 2] {
                            "new" => pkt.new,
                            "established,related" => !pkt.new,
                            "invalid" => false,
                            other => panic!("unknown ct state {other} in {line}"),
                        };
                        i += 3;
                    }
                    "counter" => i += 1,
                    "accept" => {
                        verdict = Some(Action::Allow);
                        i += 1;
                    }
                    "drop" => {
                        verdict = Some(Action::Deny);
                        i += 1;
                    }
                    other => panic!("unknown token {other} in {line}"),
                }
            }
            if matched {
                return verdict.unwrap_or_else(|| panic!("no verdict in {line}"));
            }
        }
        // Falling off the chain returns to the dispatch, which lets it pass.
        Action::Allow
    }

    /// Every golden cell, through the TEXT the lowering renders: the prologue
    /// the holder emits once per chain, then the body. The same verdict as
    /// the reference evaluator, or the lowering is wrong (ADR-0059 D6).
    #[test]
    fn the_rendered_chain_gives_every_golden_verdict() {
        for case in golden::cases() {
            let t = from_container_fw(&case.record).unwrap();
            let body = format!(
                "{}{}",
                crate::infra::fw_chain_prologue(&case.record),
                chain_body(IP, &t, &ns_set).unwrap()
            );
            assert_eq!(
                nft_verdict(&body, case.direction, &case.packet),
                case.want,
                "{}:\n{body}",
                case.name
            );
        }
    }

    /// The rendered chain of every golden record is a script nft accepts.
    /// Needs `nft` and an unprivileged user namespace (`unshare -rn`). The
    /// hosted CI runner blocks the latter, so there it returns without checking
    /// anything; it runs on a developer machine and in the lab, and the verdict
    /// test above does not depend on it.
    #[test]
    fn nft_accepts_every_rendered_chain() {
        let probe = std::process::Command::new("unshare")
            .args(["-rn", "nft", "--version"])
            .output();
        if !matches!(probe, Ok(ref o) if o.status.success()) {
            return;
        }
        for case in golden::cases() {
            let t = from_container_fw(&case.record).unwrap();
            let body = format!(
                "{}{}",
                crate::infra::fw_chain_prologue(&case.record),
                chain_body(IP, &t, &ns_set).unwrap()
            );
            let script = format!(
                "table ip t {{\n\tset {DLXALL_SET} {{ type ipv4_addr; }}\n\tset {} {{ type ipv4_addr; }}\n\tchain c {{\n{body}\t}}\n}}\n",
                ns_set("team-a")
            );
            let mut child = std::process::Command::new("unshare")
                .args(["-rn", "nft", "--check", "-f", "-"])
                .stdin(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}: nft refused\n{script}\n{}",
                case.name,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The generator this module replaced, kept verbatim as the oracle for the
    /// byte-for-byte check below (its `nft_safe` skip dropped: a record the IR
    /// refuses never reaches the comparison).
    fn legacy_body(ip: &str, fw: &delonix_model::records::ContainerFw) -> String {
        use crate::infra::{dlxns_set, fw_rule_tail, namespace_isolation_key};
        let mut body = String::new();
        if !fw.enabled {
            return body;
        }
        for r in &fw.rules {
            let self_dir = if r.dir == "out" { "saddr" } else { "daddr" };
            let tail = fw_rule_tail(r).unwrap();
            body.push_str(&format!("\t\tip {self_dir} {ip} {tail}\n"));
        }
        let nsset = dlxns_set(&namespace_isolation_key(&fw.namespace));
        let has_explicit_in = fw.policy_in == "deny" || fw.rules.iter().any(|r| r.dir == "in");
        if has_explicit_in {
            body.push_str(&format!(
                "\t\tip daddr {ip} ip saddr @{DLXALL_SET} ip saddr != @{nsset} ct state new counter drop\n"
            ));
        } else {
            body.push_str(&format!(
                "\t\tip daddr {ip} ip saddr @{nsset} counter accept\n"
            ));
            body.push_str(&format!(
                "\t\tip daddr {ip} ip saddr @{DLXALL_SET} ct state new counter drop\n"
            ));
        }
        if fw.policy_in == "deny" {
            body.push_str(&format!("\t\tip daddr {ip} counter drop\n"));
        }
        if fw.policy_out == "deny" {
            body.push_str(&format!("\t\tip saddr {ip} counter drop\n"));
        }
        body
    }

    /// The lowering emits the lines the holder emitted before it, for every
    /// golden record and a few shapes the table does not cover (egress rules
    /// before the guardrail, a peer prefix, a /32 peer, a range, a disabled
    /// record). A chain that changes text changes what `ingress ls` can read
    /// back, even with the same verdicts.
    ///
    /// One difference, and it is the only one: the old generator interleaved
    /// inbound and outbound rules in record order, the IR keeps them per
    /// direction. The order WITHIN a direction is the same, and that is the
    /// order that decides: an inbound line anchors on `ip daddr <workload>`, an
    /// outbound one on `ip saddr <workload>`, and a forwarded packet never has
    /// the workload's address at both ends, so no packet matches one of each.
    #[test]
    fn the_lowering_emits_the_text_the_holder_always_emitted() {
        let mut records: Vec<_> = golden::cases().into_iter().map(|c| c.record).collect();
        records.push(golden::fw(
            "deny",
            "deny",
            &[
                ("out", "udp", "53", "10.0.0.0/8", "allow"),
                ("in", "tcp", "8000-8010", "172.16.31.103/32", "allow"),
                ("in", "any", "9999", "", "deny"),
                ("out", "tcp", "", "*", "allow"),
                ("in", "", "", "0.0.0.0/0", "allow"),
            ],
        ));
        records.push(golden::fw("", "", &[("out", "tcp", "443", "", "allow")]));
        let mut off = golden::fw("deny", "", &[("in", "tcp", "22", "", "allow")]);
        off.enabled = false;
        records.push(off);
        for fw in records {
            let t = from_container_fw(&fw).unwrap();
            let (new, old) = (chain_body(IP, &t, &ns_set).unwrap(), legacy_body(IP, &fw));
            for anchor in [format!("\t\tip daddr {IP} "), format!("\t\tip saddr {IP} ")] {
                let side = |b: &str| -> Vec<String> {
                    b.lines()
                        .map(|l| format!("{l}\n"))
                        .filter(|l| l.starts_with(&anchor))
                        .collect()
                };
                assert_eq!(side(&new), side(&old), "{anchor:?} in {fw:?}");
            }
            assert_eq!(new.len(), old.len(), "{fw:?}");
        }
    }

    #[test]
    fn what_the_holder_chain_cannot_hold_is_refused_by_name() {
        let mut t =
            from_container_fw(&golden::fw("", "", &[("in", "tcp", "22", "", "deny")])).unwrap();
        t.ingress.rules[0].log = true;
        assert!(chain_body(IP, &t, &ns_set).unwrap_err().contains("logged"));
        t.ingress.rules[0].log = false;
        t.ingress.rules[0].stateful = false;
        assert!(chain_body(IP, &t, &ns_set)
            .unwrap_err()
            .contains("stateless"));
        t.ingress.rules[0].stateful = true;
        t.ingress.rules[0].proto = Proto::Icmp;
        assert!(chain_body(IP, &t, &ns_set).unwrap_err().contains("ICMP"));
        t.ingress.rules[0].proto = Proto::Tcp;
        t.ingress.rules[0].peer = Peer::Selector(vec![("app".into(), "web".into())]);
        assert!(chain_body(IP, &t, &ns_set)
            .unwrap_err()
            .contains("selector"));
    }
}

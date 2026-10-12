#![no_main]

//! Fuzzes the CIDR and address rules of `delonix_net_rules` — the pure layer
//! every network decision of the engine goes through. The text reaches it from
//! an untrusted manifest (`kind: Network` `subnet:`, a `NetworkPolicy` peer,
//! an `IPPool`), from `network create --subnet`, and from the node API's
//! `CreateNetwork`, which a local client drives. A panic here is a crashed
//! `stack apply` or a crashed API handler.
//!
//! Beyond «no panic», the target checks the invariants the callers lean on:
//! a parsed prefix prints back to a string that parses to the same prefix,
//! and an address derived inside a prefix is inside it.

use delonix_net_rules::Cidr;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // The first line is the prefix; the rest feeds the helpers that take a
    // second string (an id, a MAC, an address).
    let (prefix, rest) = text.split_once('\n').unwrap_or((text, ""));
    let _ = Cidr::parse_addr(prefix);
    if let Some(c) = Cidr::parse(prefix) {
        let printed = c.to_string_cidr();
        let again = Cidr::parse(&printed).expect("a printed prefix parses back");
        assert_eq!(again.to_string_cidr(), printed, "print/parse is a fixed point");
        let _ = c.size();
        let _ = c.gateway();
        let _ = c.last();
        let _ = c.usable_for_network();
        let _ = c.overlaps(&again);
        if let Some(a) = Cidr::parse_addr(rest.trim()) {
            let _ = c.contains(a);
        }
        let ip = delonix_net_rules::derive_ip_in(prefix, rest);
        if let Some(a) = Cidr::parse_addr(&ip) {
            assert!(c.contains(a) || c.size() < 4, "derived {ip} outside {printed}");
        }
        let _ = delonix_net_rules::valid_ip_in_subnet(prefix, rest);
        let _ = delonix_net_rules::vm_dhcp_pool(prefix);
        let _ = delonix_net_rules::vm_dhcp_lease_ip(prefix, rest);
        let _ = delonix_net_rules::in_vm_dhcp_pool(prefix, rest);
    }
    let _ = delonix_net_rules::parse_overlay_peer(text);
    let _ = delonix_net_rules::bridge_name(text);
    let _ = delonix_net_rules::parse_iptables_save(text);
});

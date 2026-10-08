//! The routes a MULTI-HOMED workload needs in order to use a declared network
//! route (ADR-0013 tier B).
//!
//! A `NetworkRoute` from A to B is one element of the `@netpair` exemption map:
//! `iifname <bridge A> . oifname <bridge B> : accept`. It serves a packet that
//! ENTERS the holder by A's bridge. A workload whose primary network is A sends
//! everything to A's gateway, so it uses the route with nothing more. A workload
//! attached to A as an ADDITIONAL network does not: `do_attach_extra` gives it an
//! address on A and leaves its routing table alone, so a packet for B leaves by
//! the default route — through the primary network's bridge — and never matches
//! the pair. The route reported `Ready` and carried nothing (measured live,
//! 2026-10-08: 100 % loss both ways with the pair installed, 0 % to either
//! gateway).
//!
//! What closes it is a route INSIDE the workload: «B's subnet is reached through
//! A's gateway, on the interface I have on A». And the mirror on the far end —
//! a workload attached to B as an additional network needs «A's subnet through
//! B's gateway», or its replies leave by its own default route with a source
//! address the anti-spoofing of that interface refuses.
//!
//! **This grants nothing.** A route in a workload's table says where a packet
//! goes OUT; whether it is forwarded is still the pair's decision (directed:
//! B cannot open a conversation towards A through an `A → B` route) and the
//! `fwcont` chains' after it. The mirror route only lets replies of an
//! established conversation travel back the way they came.
//!
//! Pure on purpose: what to install is computed here and asserted in unit tests;
//! the holder only executes it (`infra::sync_workload_routes`).

use crate::Cidr;

/// The routing-protocol id that marks the routes this engine installs in a
/// workload. It is what lets the holder tell ITS routes from anything else in
/// the table, and so remove exactly those when a network route closes. Any
/// value the kernel and the routing daemons leave alone works; this one is in
/// no `rt_protos` list.
pub const WORKLOAD_ROUTE_PROTO: &str = "118";

/// An interface of a workload, with the subnet it is directly connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iface {
    pub name: String,
    pub subnet: Cidr,
}

/// A network as the holder knows it: its bridge, its prefix, and the address
/// the bridge itself answers on (the gateway of every workload attached to it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Net {
    pub bridge: String,
    pub cidr: Cidr,
    pub gateway: String,
}

/// One route to hold in a workload's table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadRoute {
    pub dest: Cidr,
    pub via: String,
    pub dev: String,
}

/// The routes a workload needs, given its interfaces, the interface its default
/// route leaves by, the LIVE route pairs (`from bridge`, `to bridge`) and the
/// networks.
///
/// For every interface that is NOT the default one, and every pair that touches
/// the network that interface is on — in EITHER direction, see the module note
/// on the mirror route — the other network's subnet is routed through this
/// network's gateway, on this interface.
///
/// Three things are left alone, each for a reason that has bitten a routing
/// table before:
///
/// * the default interface: the default route already covers it, and a second
///   route to the same place is one more thing to remove later;
/// * a destination the workload is directly connected to: the kernel's
///   connected route is better, and `ip route replace` on that prefix would
///   REPLACE it;
/// * a destination already routed through an earlier interface: one
///   destination, one route. Interfaces are taken in name order so the choice
///   is the same on every run.
#[must_use]
pub fn plan(
    ifaces: &[Iface],
    default_dev: Option<&str>,
    pairs: &[(String, String)],
    nets: &[Net],
) -> Vec<WorkloadRoute> {
    let net_of = |subnet: &Cidr| nets.iter().find(|n| n.cidr == *subnet);
    let mut order: Vec<&Iface> = ifaces
        .iter()
        .filter(|i| Some(i.name.as_str()) != default_dev)
        .collect();
    order.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out: Vec<WorkloadRoute> = Vec::new();
    for iface in order {
        let Some(here) = net_of(&iface.subnet) else {
            continue;
        };
        for (from, to) in pairs {
            let other = if *from == here.bridge {
                to
            } else if *to == here.bridge {
                from
            } else {
                continue;
            };
            let Some(there) = nets.iter().find(|n| n.bridge == *other) else {
                continue;
            };
            let connected = ifaces.iter().any(|i| i.subnet.overlaps(&there.cidr));
            let routed = out.iter().any(|r| r.dest.overlaps(&there.cidr));
            if connected || routed {
                continue;
            }
            out.push(WorkloadRoute {
                dest: there.cidr,
                via: here.gateway.clone(),
                dev: iface.name.clone(),
            });
        }
    }
    out
}

/// The interfaces in `ip -o -4 addr show` — `lo` left out. One line per
/// address: `7: eth1    inet 10.81.111.153/16 scope global eth1\ ...`. An
/// interface named `eth1@if12` (a veth seen from inside) is `eth1`.
#[must_use]
pub fn parse_ifaces(listing: &str) -> Vec<Iface> {
    listing
        .lines()
        .filter_map(|line| {
            let mut t = line.split_whitespace();
            let name = t.nth(1)?.split('@').next()?.trim_end_matches(':');
            let addr = t.skip_while(|w| *w != "inet").nth(1)?;
            let (ip, len) = addr.split_once('/')?;
            let len: u8 = len.parse().ok().filter(|l| *l <= 32)?;
            let ip = Cidr::parse_addr(ip)?;
            let mask = if len == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(len))
            };
            (name != "lo").then(|| Iface {
                name: name.to_string(),
                subnet: Cidr {
                    base: ip & mask,
                    len,
                },
            })
        })
        .collect()
}

/// The interface the default route leaves by, from `ip route show default`
/// (`default via 10.201.0.1 dev eth0`).
#[must_use]
pub fn parse_default_dev(listing: &str) -> Option<String> {
    let line = listing.lines().find(|l| l.starts_with("default"))?;
    line.split_whitespace()
        .skip_while(|w| *w != "dev")
        .nth(1)
        .map(str::to_string)
}

/// The destinations in `ip route show proto <id>` — the first token of each
/// line (`10.82.0.0/16 via 10.81.0.1 dev eth1`). A bare address is a `/32`.
#[must_use]
pub fn parse_route_dests(listing: &str) -> Vec<Cidr> {
    listing
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|d| {
            if d.contains('/') {
                Cidr::parse(d)
            } else {
                Cidr::parse(&format!("{d}/32"))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(s: &str) -> Cidr {
        Cidr::parse(s).unwrap()
    }

    fn net(bridge: &str, c: &str) -> Net {
        let cidr = cidr(c);
        Net {
            bridge: bridge.into(),
            cidr,
            gateway: cidr.gateway().unwrap(),
        }
    }

    fn iface(name: &str, c: &str) -> Iface {
        Iface {
            name: name.into(),
            subnet: cidr(c),
        }
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.into(), b.into())
    }

    fn nets() -> Vec<Net> {
        vec![
            net("dlxnbase", "10.201.0.0/16"),
            net("dlxna", "10.81.0.0/16"),
            net("dlxnb", "10.82.0.0/16"),
        ]
    }

    /// The measured defect: attached to A as an additional network, with the
    /// pair A → B installed, the workload sent B's traffic out of `eth0`.
    #[test]
    fn an_additional_interface_on_the_source_network_gets_the_route() {
        let ifaces = [
            iface("eth0", "10.201.0.0/16"),
            iface("eth1", "10.81.0.0/16"),
        ];
        let got = plan(&ifaces, Some("eth0"), &[pair("dlxna", "dlxnb")], &nets());
        assert_eq!(
            got,
            vec![WorkloadRoute {
                dest: cidr("10.82.0.0/16"),
                via: "10.81.0.1".into(),
                dev: "eth1".into()
            }]
        );
    }

    /// The far end needs the way BACK, or its replies leave by its own default
    /// route. It is the same pair, read from the other side.
    #[test]
    fn the_destination_side_gets_the_mirror_route() {
        let ifaces = [
            iface("eth0", "10.201.0.0/16"),
            iface("eth1", "10.82.0.0/16"),
        ];
        let got = plan(&ifaces, Some("eth0"), &[pair("dlxna", "dlxnb")], &nets());
        assert_eq!(
            got,
            vec![WorkloadRoute {
                dest: cidr("10.81.0.0/16"),
                via: "10.82.0.1".into(),
                dev: "eth1".into()
            }]
        );
    }

    /// A workload whose PRIMARY network is the routed one already works (the
    /// case ADR-0013's spike measured): nothing is added to it.
    #[test]
    fn the_default_interface_is_left_alone() {
        let ifaces = [iface("eth0", "10.81.0.0/16")];
        assert!(plan(&ifaces, Some("eth0"), &[pair("dlxna", "dlxnb")], &nets()).is_empty());
    }

    /// No pair touching the network → no route. A pair between two OTHER
    /// networks is not this workload's business.
    #[test]
    fn a_pair_that_does_not_touch_the_workloads_networks_adds_nothing() {
        let ifaces = [
            iface("eth0", "10.201.0.0/16"),
            iface("eth1", "10.81.0.0/16"),
        ];
        assert!(plan(&ifaces, Some("eth0"), &[], &nets()).is_empty());
        let mut n = nets();
        n.push(net("dlxnc", "10.83.0.0/16"));
        assert!(plan(&ifaces, Some("eth0"), &[pair("dlxnb", "dlxnc")], &n).is_empty());
    }

    /// Attached to BOTH ends: the destination is directly connected, and a
    /// `via` route to that prefix would replace the kernel's connected route.
    #[test]
    fn a_directly_connected_destination_is_never_routed_through_a_gateway() {
        let ifaces = [
            iface("eth0", "10.201.0.0/16"),
            iface("eth1", "10.81.0.0/16"),
            iface("eth2", "10.82.0.0/16"),
        ];
        assert!(plan(
            &ifaces,
            Some("eth0"),
            &[pair("dlxna", "dlxnb"), pair("dlxnb", "dlxna")],
            &nets()
        )
        .is_empty());
    }

    /// Two additional networks both routed to the same destination: one route,
    /// and the same one on every run.
    #[test]
    fn one_destination_gets_one_route_through_the_first_interface_by_name() {
        let mut n = nets();
        n.push(net("dlxnc", "10.83.0.0/16"));
        let ifaces = [
            iface("eth2", "10.83.0.0/16"),
            iface("eth0", "10.201.0.0/16"),
            iface("eth1", "10.81.0.0/16"),
        ];
        let pairs = [pair("dlxnc", "dlxnb"), pair("dlxna", "dlxnb")];
        let got = plan(&ifaces, Some("eth0"), &pairs, &n);
        assert_eq!(
            got,
            vec![WorkloadRoute {
                dest: cidr("10.82.0.0/16"),
                via: "10.81.0.1".into(),
                dev: "eth1".into()
            }]
        );
    }

    /// A peering is the two pairs: still one route per destination.
    #[test]
    fn both_directions_declared_do_not_duplicate_the_route() {
        let ifaces = [
            iface("eth0", "10.201.0.0/16"),
            iface("eth1", "10.81.0.0/16"),
        ];
        let got = plan(
            &ifaces,
            Some("eth0"),
            &[pair("dlxna", "dlxnb"), pair("dlxnb", "dlxna")],
            &nets(),
        );
        assert_eq!(got.len(), 1);
    }

    /// With no default route at all, every interface is an additional one.
    #[test]
    fn without_a_default_route_every_interface_is_considered() {
        let ifaces = [iface("eth0", "10.81.0.0/16")];
        let got = plan(&ifaces, None, &[pair("dlxna", "dlxnb")], &nets());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].dev, "eth0");
    }

    #[test]
    fn the_ip_listings_are_read_as_ip_prints_them() {
        let addrs = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever\n\
                     7: eth0    inet 10.201.111.153/16 scope global eth0\\       valid_lft forever preferred_lft forever\n\
                     12: eth1@if13    inet 10.81.111.153/16 scope global eth1\\       valid_lft forever preferred_lft forever\n";
        assert_eq!(
            parse_ifaces(addrs),
            vec![
                iface("eth0", "10.201.0.0/16"),
                iface("eth1", "10.81.0.0/16")
            ]
        );
        assert!(parse_ifaces("").is_empty());
        assert!(parse_ifaces("7: eth0    inet banana/16 scope global eth0\n").is_empty());

        assert_eq!(
            parse_default_dev(
                "default via 10.201.0.1 dev eth0 \n10.81.0.0/16 dev eth1 scope link\n"
            )
            .as_deref(),
            Some("eth0")
        );
        assert_eq!(
            parse_default_dev("10.81.0.0/16 dev eth1 scope link\n"),
            None
        );

        let routes = "10.82.0.0/16 via 10.81.0.1 dev eth1 \n10.83.4.7 via 10.81.0.1 dev eth1 \n";
        assert_eq!(
            parse_route_dests(routes),
            vec![cidr("10.82.0.0/16"), cidr("10.83.4.7/32")]
        );
        assert!(parse_route_dests("").is_empty());
    }
}

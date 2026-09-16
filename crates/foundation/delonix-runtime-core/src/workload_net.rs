//! IPv4 address space of the Ingress **workloads** (`10.200.0.0` to
//! `10.254.255.255`) — shared between `delonix-net` (the real owner of the boundary:
//! DNAT/firewall in the infra netns) and `delonix-tunnel` (the "no-bypass" guard: a
//! tunnel can never forward directly to a workload IP, bypassing the
//! Ingress firewall).
//!
//! Defined **only once** here — the two crates previously depended on
//! `10.200`/`10.254` literals repeated independently; if the range changed in one
//! place, the other's security guard silently went stale
//! with no compilation error to warn (finding from the architecture review).

use std::net::Ipv4Addr;

/// Start of the workload space (`10.200.0.0`).
pub const WORKLOAD_IPV4_LO: Ipv4Addr = Ipv4Addr::new(10, 200, 0, 0);
/// End of the workload space (`10.254.255.255`).
pub const WORKLOAD_IPV4_HI: Ipv4Addr = Ipv4Addr::new(10, 254, 255, 255);

/// `true` if `ip` falls in the workload address space (networks `10.200/16`
/// to `10.254/16` inclusive) — pure numeric range comparison, without
/// network/broadcast address exceptions (whoever needs those exceptions,
/// e.g. `delonix-net::infra::is_ingress_ip`, applies them on top of this base).
pub fn is_workload_ipv4(ip: Ipv4Addr) -> bool {
    let n = u32::from(ip);
    n >= u32::from(WORKLOAD_IPV4_LO) && n <= u32::from(WORKLOAD_IPV4_HI)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_inclusivo_nos_extremos() {
        assert!(is_workload_ipv4(WORKLOAD_IPV4_LO));
        assert!(is_workload_ipv4(WORKLOAD_IPV4_HI));
        assert!(is_workload_ipv4(Ipv4Addr::new(10, 227, 3, 9)));
    }

    #[test]
    fn fora_do_range_recusado() {
        assert!(!is_workload_ipv4(Ipv4Addr::new(10, 199, 255, 255)));
        assert!(!is_workload_ipv4(Ipv4Addr::new(10, 255, 0, 0)));
        assert!(!is_workload_ipv4(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(!is_workload_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
    }
}

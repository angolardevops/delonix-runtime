//! Espaço de endereços IPv4 dos **workloads** do Ingress (`10.200.0.0` a
//! `10.254.255.255`) — partilhado entre `delonix-net` (dono real da fronteira:
//! DNAT/firewall no netns de infra) e `delonix-tunnel` (guard "no-bypass": um
//! túnel nunca pode encaminhar directamente para um IP de workload, saltando o
//! firewall do Ingress).
//!
//! Definido **uma só vez** aqui — os dois crates dependiam antes de literais
//! `10.200`/`10.254` repetidos independentemente; se o range mudasse num
//! sítio, o guard de segurança do outro ficava silenciosamente desactualizado
//! sem nenhum erro de compilação a avisar (achado da revisão de arquitetura).

use std::net::Ipv4Addr;

/// Início do espaço de workloads (`10.200.0.0`).
pub const WORKLOAD_IPV4_LO: Ipv4Addr = Ipv4Addr::new(10, 200, 0, 0);
/// Fim do espaço de workloads (`10.254.255.255`).
pub const WORKLOAD_IPV4_HI: Ipv4Addr = Ipv4Addr::new(10, 254, 255, 255);

/// `true` se `ip` cai no espaço de endereços de workloads (redes `10.200/16`
/// a `10.254/16` inclusive) — comparação numérica pura do range, sem
/// exceções de endereço de rede/broadcast (quem precisar dessas exceções,
/// ex. `delonix-net::infra::is_ingress_ip`, aplica-as por cima desta base).
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

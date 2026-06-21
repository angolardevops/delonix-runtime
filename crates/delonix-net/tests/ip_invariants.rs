//! Invariantes de alocação de IP via **property-based testing** (Sprint 5 — Damas:
//! os invariantes que os tipos não capturam, prova-os o `proptest` sobre milhares
//! de entradas geradas, em vez de uns poucos exemplos escolhidos à mão).

use proptest::prelude::*;

/// Um `id` hexadecimal como os que o Delonix gera (1..=16 nibbles).
fn hex_id() -> impl Strategy<Value = String> {
    "[0-9a-f]{1,16}"
}

proptest! {
    /// DETERMINISMO: o mesmo `(prefixo, id)` dá sempre o mesmo IP. Um motor de
    /// containers depende disto (reconciliação, DNS estável).
    #[test]
    fn alloc_ip_in_is_deterministic(prefix in "10\\.[0-9]{1,2}", id in hex_id()) {
        let a = delonix_net::alloc_ip_in(&prefix, &id);
        let b = delonix_net::alloc_ip_in(&prefix, &id);
        prop_assert_eq!(a, b);
    }

    /// LIMITES: o último octeto nunca é `.0` (rede), `.1` (gateway) nem `.255`
    /// (broadcast) — está sempre em `[2, 254]`. Evita colisões com infra de rede.
    #[test]
    fn alloc_ip_in_last_octet_is_a_valid_host(prefix in "10\\.[0-9]{1,2}", id in hex_id()) {
        let ip = delonix_net::alloc_ip_in(&prefix, &id);
        let last: u32 = ip.rsplit('.').next().unwrap().parse().unwrap();
        prop_assert!((2..=254).contains(&last), "octeto inválido em {ip}");
        prop_assert!(ip.starts_with(&format!("{prefix}.")), "{ip} fora do prefixo {prefix}");
    }

    /// PERTENÇA À SUBNET: `alloc_ip_cidr` devolve sempre um endereço DENTRO da
    /// subnet, e nunca a rede, o `.1` ou o broadcast.
    #[test]
    fn alloc_ip_cidr_stays_in_subnet(
        b in 0u8..=255, c in 0u8..=255, plen in 8u32..=30, id in hex_id(),
    ) {
        let subnet = format!("10.{b}.{c}.0/{plen}");
        let ip = delonix_net::alloc_ip_cidr(&subnet, &id)
            .expect("subnet /8../30 com hosts deve alocar");

        // Reconstrói rede/broadcast a partir da subnet e confirma a pertença.
        let to_u32 = |s: &str| -> u32 {
            let o: Vec<u32> = s.split('.').map(|p| p.parse().unwrap()).collect();
            (o[0] << 24) | (o[1] << 16) | (o[2] << 8) | o[3]
        };
        let base = to_u32(&format!("10.{b}.{c}.0")) & (u32::MAX << (32 - plen));
        let size = 1u32 << (32 - plen);
        let net = base;
        let broadcast = base + size - 1;
        let val = to_u32(&ip);

        prop_assert!(val > net && val < broadcast, "{ip} fora de ]{net}, {broadcast}[");
        prop_assert!(val != net + 1, "{ip} é o gateway (.1)");
    }
}

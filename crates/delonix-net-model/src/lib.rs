//! Regras de rede PURAS — o que se pode calcular sem tocar no kernel.
//!
//! Existe por causa da migração do PaaS para falar com o motor por API. Ao
//! contar o que o control-plane chamava do `delonix-net` (153 sítios), uma parte
//! não era mecanismo nenhum: derivar o nome de uma bridge, atribuir um IP dentro
//! de um prefixo, ler `10mbit` como bits/segundo, decidir se um IP cabe numa
//! sub-rede. São funções determinísticas, sem I/O, sem privilégio.
//!
//! Pôr isso atrás de um endpoint HTTP seria pagar um salto de rede — e um modo
//! de falha novo — para calcular o que ambos os lados sabem calcular. Pior: a
//! resposta TEM de ser idêntica dos dois lados (o nome da bridge que o PaaS
//! espera é o que o motor cria), e duas implementações que têm de concordar são
//! duas implementações que um dia não concordam.
//!
//! Por isso não é uma API: é um crate partilhado, SEM DEPENDÊNCIAS, que os dois
//! lados compilam. O `delonix-net` re-exporta tudo o que está aqui, portanto
//! nenhum consumidor existente teve de mudar.
//!
//! É UMA CABEÇA-DE-PONTE, e o resto tem um portão com nome. Ao classificar as
//! candidatas descobri que a maioria não é tão pura quanto parecia:
//!
//!   `alloc_ip_in`/`alloc_ip`   delegam no `ipam::lookup`, que LÊ o registo
//!   `derive_ip_in`             usa `Cidr`
//!   `valid_ip_in_subnet`       usa `Cidr`
//!   `parse_net_rate`           devolve o `Error` do `delonix-runtime-core`
//!
//! O `Cidr` é o portão: é ele próprio puro (zero I/O no seu `impl`), mas está em
//! 53 sítios de três ficheiros. Trazê-lo para aqui desbloqueia as três do meio
//! de uma vez — e é o passo seguinte, próprio, e não um extra deste.
//!
//! (A minha primeira classificação dizia que estas eram todas puras. Estava
//! errada: procurei I/O no CORPO de cada uma e não no que elas chamam. Fica
//! escrito porque o erro é fácil de repetir.)

/// Parses an overlay peer entry: `<node_ip>` (flat VXLAN) OR
/// `<node_ip>=<wg_pubkey>=<wg_ip>` (encrypted). Returns (node_ip, Option<(pubkey, wg_ip)>).
pub fn parse_overlay_peer(s: &str) -> (String, Option<(String, String)>) {
    // Format `node_ip=wg_pubkey=wg_ip`. The pubkey is base64 and ENDS in `=`
    // (padding) — it collides with the delimiter. Since node_ip and wg_ip are IPs (never
    // contain `=`), we delimit by the FIRST and the LAST `=`; what remains in the
    // middle is the pubkey WITH its padding intact. (Flat VXLAN peer = just `node_ip`.)
    match (s.find('='), s.rfind('=')) {
        (Some(first), Some(last)) if last > first => {
            let node = &s[..first];
            let pubkey = &s[first + 1..last];
            let wgip = &s[last + 1..];
            if !pubkey.is_empty() && !wgip.is_empty() {
                return (
                    node.to_string(),
                    Some((pubkey.to_string(), wgip.to_string())),
                );
            }
            (node.to_string(), None)
        }
        _ => (s.split('=').next().unwrap_or_default().to_string(), None),
    }
}

/// 32-bit FNV-1a hash (to derive a network's subnet/bridge from its name).
/// Público porque o `delonix-net` o re-exporta internamente. Não faz parte da
/// superfície que interessa a quem consome este crate — é o detalhe de que o
/// [`bridge_name`] depende.
pub fn fnv32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for byte in s.bytes() {
        h ^= byte as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// The bridge device name of a user network — **the single formula**, shared by
/// the two stores that talk about the same device (the same
/// generator-and-reader-share-the-format discipline as `fw_rule_tail`/
/// `antispoof_rule_args`).
///
/// The authority is the physical plane: `infra::network_create{,_with}` writes
/// this into the `NetDef` and it is that name the holder actually creates the
/// link with (`netadd`/`link_exists`/`resolve_net`). The declarative
/// `NetworkStore` only ever REPORTS it (`network ls`/`inspect`/`describe`), and
/// it derives it here rather than recomputing — it had its own formula
/// (`dlxn{base:02x}{hash:04x}`) and printed a device that does not exist on the
/// host, in the very column an operator reads to go and debug the device.
///
/// Note this deliberately does NOT depend on the base octet: the name alone is
/// unique in both stores, and adding the base is what made the two diverge.
/// 12 chars, comfortably inside IFNAMSIZ (15).
pub fn bridge_name(name: &str) -> String {
    format!("dlxn{:08x}", fnv32(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O nome da bridge é um CONTRATO entre o motor e o control-plane: o
    /// dispositivo que o PaaS espera tem de ser o que o motor cria. Estes valores
    /// estão aqui fixados de propósito — mudá-los é mudar o contrato, e o teste
    /// obriga a que isso seja uma decisão e não um acidente de refactor.
    #[test]
    fn bridge_name_e_determinista_e_estavel() {
        assert_eq!(bridge_name("app"), bridge_name("app"));
        assert_ne!(bridge_name("app"), bridge_name("backend"));
        // 4 + 8 hex: cabe no IFNAMSIZ do kernel (15 chars úteis) com folga.
        let n = bridge_name("qualquer-nome-muito-comprido-mesmo");
        assert!(n.starts_with("dlxn"), "prefixo inesperado: {n}");
        assert_eq!(n.len(), 12, "o nome tem de caber no IFNAMSIZ: {n}");
    }

    #[test]
    fn peer_plano_e_peer_cifrado() {
        // VXLAN em claro: só o IP do nó.
        assert_eq!(parse_overlay_peer("10.0.0.7"), ("10.0.0.7".into(), None));

        // Cifrado: a chave pública é base64 e ACABA em `=`, que colide com o
        // delimitador. Delimitar pelo primeiro e pelo último `=` é o que mantém o
        // padding intacto — partir pelo `=` daria uma chave truncada e um túnel
        // que nunca sobe.
        let (no, wg) =
            parse_overlay_peer("10.0.0.7=Xp5e/U8A8nQLNqhr0CbrW2YInV12wZM+z0H4qNHOoUQ==10.42.0.2");
        assert_eq!(no, "10.0.0.7");
        let (chave, ip) = wg.expect("devia ter identidade wireguard");
        assert_eq!(chave, "Xp5e/U8A8nQLNqhr0CbrW2YInV12wZM+z0H4qNHOoUQ=");
        assert_eq!(ip, "10.42.0.2");
    }

    #[test]
    fn fnv32_bate_certo_com_a_referencia() {
        // Vector conhecido do FNV-1a de 32 bits. Sem isto, uma "optimização" que
        // mude a dispersão renomeia TODAS as bridges de uma instalação viva.
        assert_eq!(fnv32(""), 0x811c_9dc5);
        assert_eq!(fnv32("a"), 0xe40c_292c);
        assert_eq!(fnv32("foobar"), 0xbf9c_f968);
    }
}

//! Regras de rede PURAS — o que se pode calcular sem tocar no kernel.
//!
//! `delonix-net-RULES`, e não `-model`: o `delonix-paas` já tem um crate com
//! esse nome, e é outra coisa — o modelo de domínio tipado de uma rede
//! (`Network`, `Subnet`, `Port`, IPAM, reconciliação). Dois crates com o mesmo
//! nome, em repositórios que dependem um do outro, colidem no dia em que o
//! segundo consumir o primeiro. Este são REGRAS: funções e um tipo de valor.
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
//! O `Cidr` vive aqui, e é ele que faz este crate valer a pena. Foi desenhado
//! desde o início sem dependências — sem o crate `ipnet`, sem IPv6 (desligado
//! por decisão de segurança neste motor), aritmética à mão — e por isso
//! atravessa a fronteira sem arrastar nada. Com ele vieram as duas funções que
//! dele dependiam.
//!
//! FICA DE FORA o que não é puro, e a lista é o resultado de uma classificação
//! que eu próprio errei à primeira (procurei I/O no CORPO de cada função e não
//! no que elas chamam):
//!
//!   `alloc_ip_in`/`alloc_ip`   delegam no `ipam::lookup`, que LÊ o registo
//!   `parse_net_rate`           devolve o `Error` do `delonix-runtime-core`
//!
//! O primeiro par é mecanismo a sério — atribuir um endereço exige ver quais já
//! estão atribuídos, e isso é estado partilhado, não aritmética. Esse pertence à
//! API. O `parse_net_rate` é puro, e só espera por um tipo de erro que este
//! crate possa devolver sem depender do runtime-core.

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

/// A network's address space, as a real prefix instead of the two octets of a
/// hardcoded `/16`.
///
/// **Why this type exists (ADR-0013 tier A).** Everything about a network used
/// to be derived from ONE octet: the record on disk held `210`, the bridge was
/// named from it, the gateway was `10.<n>.0.1`, and the IPAM allocated inside
/// `10.<n>.0.0/16` because that was the only shape there was. `--subnet` could
/// only ever pick which octet. That is a fine default and a poor contract: a
/// network engineer has an address plan, and «any /16 you like as long as it is
/// `10.<200-254>`» does not meet it.
///
/// Kept deliberately small — no dependency, no `ipnet` crate (this repo does not
/// grow its supply chain for arithmetic), and no IPv6: v6 is DISABLED by design
/// in this engine (v0.37.1, it was a complete bypass of the policy model) and
/// re-enabling it is a security decision, not a widening of this struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    /// Network address, host bits already cleared.
    pub base: u32,
    pub len: u8,
}

impl Cidr {
    /// Parses `a.b.c.d/len`, and ALSO the legacy two-octet form (`10.210`),
    /// which every record written before this meant as `10.210.0.0/16`.
    ///
    /// Accepting the old form here is what makes the migration a non-event:
    /// a record holding a bare octet keeps meaning exactly what it always meant,
    /// and is rewritten in the new shape on the next write. Same promotion the
    /// `base=<n>` line already does.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (addr, len) = match s.split_once('/') {
            Some((a, l)) => (a, l.parse::<u8>().ok()?),
            // Legacy `10.210` — two octets, always a /16.
            None if s.split('.').count() == 2 => (s, 16),
            None => return None,
        };
        if len > 32 {
            return None;
        }
        let mut oct = [0u8; 4];
        let parts: Vec<&str> = addr.split('.').collect();
        if parts.is_empty() || parts.len() > 4 {
            return None;
        }
        for (i, p) in parts.iter().enumerate() {
            oct[i] = p.parse::<u8>().ok()?;
        }
        let raw = u32::from_be_bytes(oct);
        // Host bits are CLEARED rather than rejected: `10.210.5.7/16` is how
        // people write «the /16 that address is in», and refusing it would be
        // pedantry that helps nobody.
        let mask = Self::mask(len);
        Some(Self {
            base: raw & mask,
            len,
        })
    }

    fn mask(len: u8) -> u32 {
        if len == 0 {
            0
        } else {
            u32::MAX << (32 - len)
        }
    }

    /// How many addresses the prefix holds, saturating — a `/0` does not fit in
    /// a `u32` and nothing here ever wants one.
    pub fn size(&self) -> u32 {
        1u32.checked_shl(32 - self.len as u32).unwrap_or(u32::MAX)
    }

    /// The gateway: the FIRST usable address — `None` when the prefix has none.
    ///
    /// **It returns an `Option` because the honest answer is sometimes «there
    /// isn't one», and the first version of this could not say that.** It was
    /// `base + 1` unconditionally, and that is wrong twice:
    ///
    /// * on a `/32` (one address) and a `/31` (RFC 3021 point-to-point, two
    ///   addresses and no room for a gateway), `base + 1` lands OUTSIDE the
    ///   prefix or on the peer. Measured: `10.0.0.0/32` answered `10.0.0.1`;
    /// * on `255.255.255.255/32` it **overflowed** — a panic in debug, and in
    ///   release a silent wrap to `0.0.0.0`, which is the dangerous half: a
    ///   network whose gateway is the null address, configured without a word.
    ///
    /// `checked_add` and the length guard together make both impossible. A
    /// caller that needs a gateway has to handle the `None`, which is the point.
    pub fn gateway(&self) -> Option<String> {
        // A gateway needs the network address, itself, and at least one host —
        // so /31 and /32 are excluded by arithmetic, not by taste.
        if self.len >= 31 {
            return None;
        }
        self.base.checked_add(1).map(Self::fmt_u32)
    }

    /// The last address of the prefix (the broadcast, on a /24 and wider).
    pub fn last(&self) -> u32 {
        self.base | !Self::mask(self.len)
    }

    /// Is this prefix usable as a WORKLOAD network here?
    ///
    /// Separate from `parse` on purpose: parsing answers «is this a prefix»,
    /// this answers «can a network live in it», and conflating the two would
    /// make the type unusable for reading a peer's address or a route.
    ///
    /// The floor is /28 (16 addresses: network, gateway, 13 hosts, broadcast) —
    /// below that a network cannot hold enough workloads to be worth declaring,
    /// and the ceiling is /8 so one network cannot swallow a whole private
    /// range by typo.
    pub fn usable_for_network(&self) -> std::result::Result<(), String> {
        if !(8..=28).contains(&self.len) {
            return Err(format!(
                "prefix /{} is outside the usable range /8–/28 (a /29 or smaller has no room for \
                 a gateway plus hosts; wider than /8 swallows a whole private range)",
                self.len
            ));
        }
        Ok(())
    }

    pub fn contains(&self, ip: u32) -> bool {
        ip & Self::mask(self.len) == self.base
    }

    /// Do the two prefixes share any address? The question `network create` has
    /// to answer before handing out a second network that overlaps the first.
    pub fn overlaps(&self, other: &Cidr) -> bool {
        let shorter = self.len.min(other.len);
        self.base & Self::mask(shorter) == other.base & Self::mask(shorter)
    }

    pub fn to_string_cidr(&self) -> String {
        format!("{}/{}", Self::fmt_u32(self.base), self.len)
    }

    /// Formata um `u32` como endereço com pontos.
    ///
    /// Pública porque o `ipam` do `delonix-net` a usa para escrever o candidato
    /// que acabou de calcular — era `pub(crate)` quando o `Cidr` vivia lá, e a
    /// mudança de crate transformou isso num erro de compilação, não numa
    /// decisão. Fica exposta com o mesmo âmbito de sempre: um detalhe de
    /// formatação do próprio tipo.
    pub fn fmt_u32(v: u32) -> String {
        let b = v.to_be_bytes();
        format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }

    /// Parses a dotted address into a `u32`.
    pub fn parse_addr(ip: &str) -> Option<u32> {
        let p: Vec<&str> = ip.trim().split('.').collect();
        if p.len() != 4 {
            return None;
        }
        let mut o = [0u8; 4];
        for (i, x) in p.iter().enumerate() {
            o[i] = x.parse::<u8>().ok()?;
        }
        Some(u32::from_be_bytes(o))
    }
}

/// **Preferred** IP (deterministic, pure) in an arbitrary `/16` (`<prefix>.A.B`),
/// derived from the id. It's just the starting point: on its own it collides by the birthday
/// paradox at ~300 containers (32 bits of the id → 16 bits of host). Real uniqueness comes from
/// the lease registry + probing in [`ipam::allocate`]; see [`alloc_ip_in`].
pub fn derive_ip_in(prefix: &str, id: &str) -> String {
    let hex = &id[..id.len().min(8)];
    let n = u32::from_str_radix(hex, 16).unwrap_or(2);
    // O caminho do /16 fica BYTE A BYTE como estava, e isso é deliberado: toda a
    // rede que existe hoje tem leases derivados desta fórmula exacta, e a
    // generalização não tem nada a ganhar em mexer na base instalada. A fórmula
    // geral abaixo NÃO é equivalente a esta (o clamp do último octeto não é o
    // mesmo que o resto da divisão sobre o espaço todo) — por isso as duas
    // coexistem em vez de uma fingir ser a outra.
    if let Some(net) = Cidr::parse(prefix) {
        if net.len != 16 {
            return derive_ip_general(&net, n);
        }
    }
    let a = ((n >> 8) & 0xff) as u8;
    let mut b = (n & 0xff) as u8;
    if b < 2 {
        b = 2;
    }
    if b == 255 {
        b = 254;
    }
    format!("{prefix}.{a}.{b}")
}

/// Validates that `ip` is a usable unicast address in `prefix`'s `/16` subnet
/// (e.g.: prefix `10.88`): 4 octets, first two == prefix, not the gateway
/// (`prefix.0.1`), the network (`prefix.0.0`) or the broadcast (`prefix.255.255`).
pub fn valid_ip_in_subnet(prefix: &str, ip: &str) -> bool {
    // `prefix` chega aqui em DUAS formas e ambas têm de continuar a valer: a
    // legada de dois octetos (`10.210`, que sempre quis dizer `10.210.0.0/16`) e
    // um CIDR a sério. O `Cidr::parse` resolve as duas, por isso não há aqui um
    // caminho novo — há o mesmo caminho a saber mais.
    let Some(net) = Cidr::parse(prefix) else {
        return false;
    };
    let Some(addr) = Cidr::parse_addr(ip) else {
        return false;
    };
    if !net.contains(addr) {
        return false;
    }
    // Exclui a rede, o gateway e o broadcast. No `/16` isto é exactamente o que
    // a versão anterior excluía à mão (`.0.0`, `.0.1`, `.255.255`) — a regra
    // geral não é uma mudança de comportamento, é a mesma regra dita de uma
    // forma que também serve um /22.
    addr != net.base && addr != net.base.wrapping_add(1) && addr != net.last()
}

/// O preferido para um prefixo de tamanho arbitrário.
///
/// Salta a rede e o gateway (os dois primeiros) e o broadcast (o último), e
/// distribui o resto pelo espaço utilizável. Num prefixo pequeno de mais para
/// ter hosts devolve o próprio gateway — o `valid_ip_in_subnet` recusa-o a
/// seguir e o `probe_free` também não encontra nada, que é a resposta correcta:
/// não há endereço para dar, e é o `allocate` que o diz com um erro.
fn derive_ip_general(net: &Cidr, n: u32) -> String {
    let uteis = net.size().saturating_sub(3); // rede + gateway + broadcast
    if uteis == 0 {
        return Cidr::fmt_u32(net.base.wrapping_add(1));
    }
    let off = n % uteis;
    Cidr::fmt_u32(net.base + 2 + off)
}

/// O que um ficheiro `iptables-save` contém, contado sem o interpretar.
///
/// Existe para o `network import-iptables`, que ANALISA e não aplica: diz quanta
/// coisa há e mostra um exemplo traduzido, para quem está a migrar poder ver a
/// forma antes de decidir. Nada aqui toca no host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IptablesSummary {
    pub tables: usize,
    pub chains: usize,
    pub rules: usize,
    /// A primeira regra encontrada, para servir de exemplo na tradução.
    pub sample: Option<String>,
}

/// Conta o que há num `iptables-save`.
///
/// Puro: é leitura de texto, e por isso vive aqui e não no mecanismo. A forma do
/// ficheiro é estável há décadas — `*tabela`, `:CADEIA`, `-A regra` — e é toda a
/// gramática de que isto precisa.
pub fn parse_iptables_save(texto: &str) -> IptablesSummary {
    let mut s = IptablesSummary::default();
    for linha in texto.lines() {
        let l = linha.trim();
        if l.starts_with('*') {
            s.tables += 1;
        } else if l.starts_with(':') {
            s.chains += 1;
        } else if l.starts_with("-A") {
            s.rules += 1;
            if s.sample.is_none() {
                s.sample = Some(l.to_string());
            }
        }
    }
    s
}

/// O VIP estável de um serviço, derivado do nome (`10.90.a.b`).
///
/// Fora do espaço dos containers de propósito: o tráfego para o VIP tem de
/// passar pelo caminho que o balanceia, e um endereço dentro da subrede seria
/// entregue directamente ao container.
///
/// Puro e determinístico — o control-plane precisa de calcular o mesmo VIP que o
/// motor, e é por isso que vive aqui e não atrás de uma API. Os extremos `.0`,
/// `.1` e `.255` são evitados: o primeiro não é um endereço de host, o segundo é
/// por convenção o gateway, e o último é o broadcast.
pub fn service_vip(key: &str) -> String {
    let h = fnv32(key);
    let a = ((h >> 8) & 0xff) as u8;
    let mut b = (h & 0xff) as u8;
    if b < 2 {
        b = 2;
    }
    if b == 255 {
        b = 254;
    }
    format!("10.90.{a}.{b}")
}

/// Whether a `matchLabels` selector selects a workload carrying `labels`.
///
/// PURE, shared by design (ADR-0032): `kind: Service` needs this to compute its
/// backend set for the DNS index, and `kind: FirewallPolicy`'s own planned
/// selector (ADR-0024, still unimplemented) is meant to reuse the exact same
/// function rather than grow a second, divergence-prone `matchLabels` reader.
/// Lives here — not in `delonix-runtime-bin`, where both Kinds' CLI code lives
/// — because `delonix-net::infra::build_dns_index` (which computes a
/// `Service`'s live membership) cannot depend on the bin crate.
///
/// An EMPTY `match_labels` selects NOTHING, deliberately fail-closed: a
/// `spec.selector.matchLabels: {}` that silently meant "every workload on the
/// node" would be exactly the accept-and-widen footgun this codebase's own
/// audits keep finding and removing elsewhere (`ingress ls`'s `0.0.0.0/0`
/// default, `--net-connect` bypassing the firewall, ...). A caller that wants
/// "no selection" states so by omitting the `Service` document, not by writing
/// an empty selector.
pub fn matches_labels(
    labels: &std::collections::BTreeMap<String, String>,
    match_labels: &std::collections::BTreeMap<String, String>,
) -> bool {
    !match_labels.is_empty() && match_labels.iter().all(|(k, v)| labels.get(k) == Some(v))
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
    /// O `Cidr` atravessa a fronteira, e por isso as suas garantias passam a ser
    /// contrato entre os dois lados — não detalhe interno de um crate.
    #[test]
    fn cidr_le_as_duas_formas_que_existem_em_disco() {
        // A forma completa.
        let c = Cidr::parse("10.220.0.0/16").expect("cidr");
        assert_eq!(c.len, 16);
        assert_eq!(c.to_string_cidr(), "10.220.0.0/16");

        // E a LEGADA de dois octetos, que todo o registo escrito antes do /16
        // explícito significava como `10.210.0.0/16`. Recusá-la aqui seria
        // perder as redes de quem já corre isto.
        let l = Cidr::parse("10.210").expect("forma legada");
        assert_eq!((l.len, l.to_string_cidr().as_str()), (16, "10.210.0.0/16"));
    }

    #[test]
    fn cidr_limpa_os_bits_de_host() {
        // `10.220.5.9/16` é a MESMA rede que `10.220.0.0/16`. Guardar a base sem
        // limpar faria duas redes iguais compararem diferente.
        let c = Cidr::parse("10.220.5.9/16").expect("cidr");
        assert_eq!(c.to_string_cidr(), "10.220.0.0/16");
        assert_eq!(c, Cidr::parse("10.220.0.0/16").unwrap());
    }

    #[test]
    fn cidr_recusa_o_que_nao_e_endereco() {
        for mau in ["", "10.220.0.0/33", "999.1.1.1/16", "10.220.0.0/", "abc"] {
            assert!(Cidr::parse(mau).is_none(), "aceitou {mau:?}");
        }
    }

    #[test]
    fn ip_derivado_cai_dentro_da_rede_e_e_estavel() {
        // Determinístico: o mesmo id dá sempre o mesmo IP — é o que permite ao
        // control-plane prever o endereço sem perguntar ao motor.
        let a = derive_ip_in("10.220", "deadbeef");
        assert_eq!(a, derive_ip_in("10.220", "deadbeef"));
        assert!(valid_ip_in_subnet("10.220", &a), "{a} fora de 10.220/16");

        // E ids diferentes não colidem trivialmente.
        assert_ne!(a, derive_ip_in("10.220", "cafebabe"));
    }

    #[test]
    fn valid_ip_in_subnet_separa_dentro_de_fora() {
        assert!(valid_ip_in_subnet("10.220", "10.220.5.9"));
        assert!(!valid_ip_in_subnet("10.220", "10.221.5.9"));
        assert!(!valid_ip_in_subnet("10.220", "nao-e-um-ip"));
    }
    /// Um `iptables-save` de verdade, encurtado. A gramática é `*tabela`,
    /// `:CADEIA politica [contadores]`, `-A regra`.
    const SAVE: &str = r#"# Generated by iptables-save
*filter
:INPUT ACCEPT [0:0]
:FORWARD DROP [0:0]
:OUTPUT ACCEPT [0:0]
-A INPUT -i lo -j ACCEPT
-A INPUT -p tcp --dport 22 -j ACCEPT
-A FORWARD -i docker0 -o eth0 -j ACCEPT
COMMIT
*nat
:PREROUTING ACCEPT [0:0]
-A PREROUTING -p tcp --dport 80 -j DNAT --to-destination 10.0.0.5:8080
COMMIT
"#;

    #[test]
    fn conta_tabelas_cadeias_e_regras() {
        let s = parse_iptables_save(SAVE);
        assert_eq!(s.tables, 2, "*filter e *nat");
        assert_eq!(s.chains, 4, "três do filter e uma do nat");
        assert_eq!(s.rules, 4);
        // A primeira regra serve de exemplo para a tradução — e tem de ser a
        // PRIMEIRA, não uma qualquer: quem lê o relatório compara-a com o
        // ficheiro que lhe deu.
        assert_eq!(s.sample.as_deref(), Some("-A INPUT -i lo -j ACCEPT"));
    }

    #[test]
    fn comentarios_e_commit_nao_contam() {
        // `# Generated by` e `COMMIT` não são nem tabela, nem cadeia, nem regra.
        // Contá-los inflacionaria o relatório de quem está a decidir se migra.
        let s = parse_iptables_save("# comentário\nCOMMIT\n\n");
        assert_eq!((s.tables, s.chains, s.rules), (0, 0, 0));
        assert!(s.sample.is_none());
    }

    #[test]
    fn ficheiro_vazio_nao_rebenta() {
        assert_eq!(parse_iptables_save(""), IptablesSummary::default());
    }
    /// O VIP é um CONTRATO: o control-plane calcula-o para escrever regras e o
    /// motor para as aplicar. Se divergirem, o tráfego vai para um endereço que
    /// ninguém está a escutar.
    #[test]
    fn service_vip_e_estavel_e_evita_os_extremos() {
        assert_eq!(service_vip("api"), service_vip("api"));
        assert_ne!(service_vip("api"), service_vip("db"));

        // Fora do espaço dos containers (10.200-254): tem de passar pelo
        // caminho que balanceia.
        for nome in ["api", "db", "cache", "web", "worker", "queue"] {
            let vip = service_vip(nome);
            assert!(vip.starts_with("10.90."), "{nome} -> {vip}");
            let ultimo: u8 = vip.rsplit('.').next().unwrap().parse().unwrap();
            // `.0` não é endereço de host, `.1` é o gateway por convenção,
            // `.255` é broadcast.
            assert!((2..=254).contains(&ultimo), "{nome} -> {vip}");
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn um_selector_vazio_nao_seleciona_nada() {
        let container = labels(&[("app", "web")]);
        assert!(!matches_labels(&container, &labels(&[])));
    }

    #[test]
    fn todas_as_chaves_do_selector_tem_de_bater() {
        let container = labels(&[("app", "web"), ("tier", "frontend")]);
        assert!(matches_labels(&container, &labels(&[("app", "web")])));
        assert!(matches_labels(
            &container,
            &labels(&[("app", "web"), ("tier", "frontend")])
        ));
        // Uma chave a mais no selector que o container não tem: falha.
        assert!(!matches_labels(
            &container,
            &labels(&[("app", "web"), ("env", "prod")])
        ));
        // Mesma chave, valor diferente: falha.
        assert!(!matches_labels(&container, &labels(&[("app", "worker")])));
    }

    #[test]
    fn um_container_sem_labels_nao_bate_com_selector_nenhum() {
        assert!(!matches_labels(&labels(&[]), &labels(&[("app", "web")])));
    }
}

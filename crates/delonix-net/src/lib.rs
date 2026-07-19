//! `delonix-net` — rede e firewall do Delonix Engine.
//!
//! Filosofia (da arquitectura): **netfilter nativo, não reinventar firewall**.
//! Esta crate orquestra as ferramentas do kernel — `ip` (iproute2) para
//! `bridge`/`veth`/`netns` e `nft` (nftables) para NAT e firewall — atrás de
//! uma API Rust limpa. É o mesmo padrão do `dockerd`, que invoca `iptables`.
//!
//! Modelo de rede (bridge, estilo `docker0`):
//! - bridge `delonix0` num `/16` **auto-detectado livre** (evita colisão com o
//!   Docker `172.17/16`, o Podman `10.88/16` e as redes do host), com IP
//!   forwarding e `MASQUERADE`;
//! - cada container recebe um `veth` (`eth0`) ligado à bridge, com um IP
//!   determinístico derivado do seu id;
//! - o firewall por container é um `set` de IPs bloqueados numa chain `forward`
//!   dedicada (tabela `ip delonix`) — reversível por elemento.
//!
//! A ligação ao container faz-se ao estilo CNI: o runtime cria o `netns`
//! (`CLONE_NEWNET`); [`Net::attach`] configura-o a partir do host, pelo PID.

use delonix_runtime_core::{Error, Result};
use std::process::{Command, Stdio};

pub mod bpf;
pub mod cni;
pub mod discover;
pub mod infra;
pub mod ipam;
pub mod wg;

pub use discover::{discover_ports, DiscoveredPort};

const BRIDGE: &str = "delonix0";
const TABLE: &str = "delonix"; // tabela nft dedicada (família ip)
const VIP_SUBNET: &str = "10.90.0.0/16"; // VIPs de serviço (FORA da subnet dos containers)

/// Octeto-base (`10.<base>.0.0/16`) da rede por omissão. Para **não colidir**
/// com o Docker (`172.17.0.0/16`), o Podman (`10.88.0.0/16`) nem com as redes
/// já presentes no host, detectamos um `/16` livre na PRIMEIRA criação da bridge
/// e **persistimo-lo** — os IPs derivados têm de ser estáveis entre invocações.
/// `DELONIX_SUBNET_BASE` força um valor; senão lê o ficheiro persistido; senão
/// varre o host e escolhe um livre.
fn default_base() -> u8 {
    if let Ok(Ok(b)) = std::env::var("DELONIX_SUBNET_BASE").map(|s| s.trim().parse::<u8>()) {
        return b;
    }
    let path = net_state_path();
    if let Ok(Ok(b)) = std::fs::read_to_string(&path).map(|s| s.trim().parse::<u8>()) {
        return b;
    }
    let base = pick_free_base();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, base.to_string());
    base
}

fn net_state_path() -> std::path::PathBuf {
    let root = std::env::var("DELONIX_ROOT").unwrap_or_else(|_| "/var/lib/delonix".into());
    std::path::Path::new(&root).join("net").join("default-base")
}

/// Octetos `10.X` já em uso em rotas/endereços do host (evita colisão com o
/// host, Docker, Podman e outras redes do Delonix activas).
fn used_10_octets() -> std::collections::HashSet<u8> {
    let mut used = std::collections::HashSet::new();
    used.insert(88); // default do Podman
    used.insert(90); // VIPs de serviço do Delonix
    for args in [["-o", "addr"].as_slice(), ["route"].as_slice()] {
        if let Ok(out) = capture("ip", args) {
            for tok in out.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
                if let Some(rest) = tok.strip_prefix("10.") {
                    if let Some(Ok(b)) = rest.split('.').next().map(|o| o.parse::<u8>()) {
                        used.insert(b);
                    }
                }
            }
        }
    }
    used
}

/// Escolhe um octeto-base `10.X` livre (preferindo `200..=239`, longe dos
/// defaults de Docker/Podman e das redes de utilizador mais comuns).
fn pick_free_base() -> u8 {
    let used = used_10_octets();
    (200..=239).chain(11..=87).chain(91..=199).find(|b| !used.contains(b)).unwrap_or(201)
}

/// O `prefix`/`gateway`/`subnet` da rede por omissão (derivados do octeto-base).
fn default_prefix() -> String {
    format!("10.{}", default_base())
}
fn default_gateway() -> String {
    format!("10.{}.0.1", default_base())
}
fn default_subnet() -> String {
    format!("10.{}.0.0/16", default_base())
}

/// A1 — **default-deny de entrada** para a subnet de uma rede Delonix. Bloqueia
/// ligações NOVAS encaminhadas PARA um container que não sejam:
///   - tráfego de retorno (qualquer estado != `new` passa, ex.: `established`), ou
///   - uma porta **publicada** (`ct status dnat`, i.e. já passou por DNAT no ingress).
///
/// Mantém a SAÍDA totalmente aberta (a regra casa só `ip daddr <subnet>`) e NÃO
/// toca na política do hook `forward` — encaminhamento não-Delonix do host fica
/// intacto (ao contrário de um `policy drop`, que afetaria Docker/k8s/libvirt no
/// mesmo host). As duas regras são autossuficientes (o `dnat accept` precede o
/// `new drop`), idempotentes, e funcionam em tabelas novas ou pré-existentes.
///
/// Desligável com `DELONIX_FORWARD_OPEN=1` (restaura o encaminhamento aberto
/// histórico — acesso direto ao IP do container a partir de outras redes).
fn forward_inbound_deny(subnet: &str) {
    if std::env::var_os("DELONIX_FORWARD_OPEN").is_some() {
        // NET-03: o opt-out reverte para default-allow — não deixar isto silencioso.
        eprintln!(
            "delonix: AVISO DE SEGURANÇA — DELONIX_FORWARD_OPEN activo: o inbound-deny do\n\
             \x20        forward está DESLIGADO (containers acessíveis directamente de outras\n\
             \x20        redes/host). Só para depuração — NÃO usar em produção."
        );
        return;
    }
    let drop_needle = format!("ip daddr {subnet} ct state new drop");
    if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "forward"]) {
        if out.contains(&drop_needle) {
            return; // já aplicado
        }
    }
    // 1.º: deixa passar o tráfego de portas publicadas (DNAT); 2.º: nega o resto.
    run_ok("nft", &["add", "rule", "ip", TABLE, "forward", "ip", "daddr", subnet, "ct", "status", "dnat", "accept"]);
    run_ok("nft", &["add", "rule", "ip", TABLE, "forward", "ip", "daddr", subnet, "ct", "state", "new", "drop"]);
}

/// O gestor de rede do Delonix.
pub struct Net;

// ---- helpers de processo -------------------------------------------------

/// Corre um comando; erro se o código de saída não for zero.
fn run(prog: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| Error::Runtime {
            context: "spawn",
            message: format!("{prog}: {e}"),
        })?;
    if !out.status.success() {
        return Err(Error::Runtime {
            context: "net cmd",
            message: format!(
                "{prog} {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(())
}

/// Corre um comando ignorando o resultado (para passos idempotentes/cleanup).
fn run_ok(prog: &str, args: &[&str]) {
    let _ = Command::new(prog).args(args).output();
}

/// Corre um comando e devolve o stdout.
fn capture(prog: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| Error::Runtime {
            context: "spawn",
            message: format!("{prog}: {e}"),
        })?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse de uma entrada de peer de overlay: `<node_ip>` (VXLAN plano) OU
/// `<node_ip>=<wg_pubkey>=<wg_ip>` (cifrado). Devolve (node_ip, Option<(pubkey, wg_ip)>).
pub fn parse_overlay_peer(s: &str) -> (String, Option<(String, String)>) {
    // Formato `node_ip=wg_pubkey=wg_ip`. A pubkey é base64 e TERMINA em `=`
    // (padding) — colide com o delimitador. Como node_ip e wg_ip são IPs (nunca
    // contêm `=`), delimitamos pelo PRIMEIRO e pelo ÚLTIMO `=`; o que sobra ao
    // meio é a pubkey COM o seu padding intacto. (Peer VXLAN plano = só `node_ip`.)
    match (s.find('='), s.rfind('=')) {
        (Some(first), Some(last)) if last > first => {
            let node = &s[..first];
            let pubkey = &s[first + 1..last];
            let wgip = &s[last + 1..];
            if !pubkey.is_empty() && !wgip.is_empty() {
                return (node.to_string(), Some((pubkey.to_string(), wgip.to_string())));
            }
            (node.to_string(), None)
        }
        _ => (s.split('=').next().unwrap_or_default().to_string(), None),
    }
}

fn link_exists(name: &str) -> bool {
    Command::new("ip")
        .args(["link", "show", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Lista as bridges do Delonix presentes no host (`delonix0` + `dlxn*` das redes
/// de utilizador) — usado para construir as regras de isolamento entre redes.
fn list_delonix_bridges() -> Vec<String> {
    let out = capture("ip", &["-o", "link", "show", "type", "bridge"]).unwrap_or_default();
    let mut names = Vec::new();
    for line in out.lines() {
        // formato: "N: nome: <...>" (o nome pode ter "@" para o peer).
        if let Some(name) = line.split(':').nth(1).map(|s| s.trim().split('@').next().unwrap_or("").trim()) {
            if name == BRIDGE || name.starts_with("dlxn") {
                names.push(name.to_string());
            }
        }
    }
    names
}

fn table_exists() -> bool {
    Command::new("nft")
        .args(["list", "table", "ip", TABLE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---- nomes e IPs determinísticos ----------------------------------------

fn netns_name(id: &str) -> String {
    format!("delonix-{}", &id[..id.len().min(12)])
}

/// Remove as regras anti-spoofing (`iifname "<hv>" ip saddr != … drop`) do `forward`
/// da tabela ROOT, pelo handle (idempotência). Espelho de `infra.rs::clear_antispoof`
/// para o caminho root/legacy. Best-effort.
fn clear_antispoof_root(hv: &str) {
    let listed = capture("nft", &["-a", "list", "chain", "ip", TABLE, "forward"]).unwrap_or_default();
    let needle = format!("iifname \"{hv}\"");
    for line in listed.lines() {
        if line.contains(&needle) && line.contains("saddr") && line.contains("drop") {
            if let Some(h) = line.rsplit("# handle ").next().and_then(|x| x.trim().parse::<u32>().ok()) {
                run_ok("nft", &["delete", "rule", "ip", TABLE, "forward", "handle", &h.to_string()]);
            }
        }
    }
}
fn host_veth(id: &str) -> String {
    format!("dlx{}", &id[..id.len().min(8)]) // <= 15 chars (IFNAMSIZ)
}
fn peer_veth(id: &str) -> String {
    format!("dlxp{}", &id[..id.len().min(8)])
}
// Veths de uma interface *extra* (multi-homing): sufixadas pelo índice (>=1) para
// não colidirem com as da interface primária nem entre redes. <= 15 chars.
fn host_veth_n(id: &str, idx: u32) -> String {
    format!("dlx{}{idx}", &id[..id.len().min(6)])
}
fn peer_veth_n(id: &str, idx: u32) -> String {
    format!("dlxp{}{idx}", &id[..id.len().min(6)])
}

/// Valida que `ip` é um endereço unicast utilizável na subnet `/16` de `prefix`
/// (ex.: prefix `10.88`): 4 octetos, dois primeiros == prefix, não é a gateway
/// (`prefix.0.1`), a rede (`prefix.0.0`) nem o broadcast (`prefix.255.255`).
pub fn valid_ip_in_subnet(prefix: &str, ip: &str) -> bool {
    let oct: Vec<&str> = ip.split('.').collect();
    if oct.len() != 4 {
        return false;
    }
    let nums: Vec<u16> = match oct.iter().map(|o| o.parse::<u16>()).collect::<std::result::Result<_, _>>() {
        Ok(v) => v,
        Err(_) => return false,
    };
    if nums.iter().any(|&n| n > 255) {
        return false;
    }
    let pfx = format!("{}.{}", nums[0], nums[1]);
    if pfx != prefix {
        return false;
    }
    let host = (nums[2], nums[3]);
    // exclui rede (.0.0), gateway (.0.1) e broadcast (.255.255).
    !(host == (0, 0) || host == (0, 1) || host == (255, 255))
}

/// IP determinístico em `10.88.A.B`, derivado do id (evita .0/.1/.255).
/// Interpreta `hostPort:contPort[/tcp|udp]`, `contPort` ou `hp:cp`. Devolve
/// `(host_port, cont_port, proto)`.
pub fn parse_publish(spec: &str) -> Result<(String, String, String)> {
    let (mapping, proto) = match spec.split_once('/') {
        Some((m, p)) => (m, p.to_lowercase()),
        None => (spec, "tcp".to_string()),
    };
    if proto != "tcp" && proto != "udp" {
        return Err(Error::Invalid(format!("protocolo inválido em '{spec}' (tcp|udp)")));
    }
    let (host_port, cont_port) = match mapping.rsplit_once(':') {
        Some((h, c)) => (h.trim(), c.trim()),
        None => (mapping.trim(), mapping.trim()),
    };
    let valid = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit());
    if !valid(host_port) || !valid(cont_port) {
        return Err(Error::Invalid(format!("porta inválida em '{spec}' (ex.: 8080:80)")));
    }
    Ok((host_port.to_string(), cont_port.to_string(), proto))
}

/// Especificação de limite de largura de banda de rede de um container.
/// `rate_bit` é o caudal em bits/segundo; `burst_bytes` é o balde (token bucket)
/// do TBF/police, em bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetRate {
    pub rate_bit: u64,
    pub burst_bytes: u64,
}

impl NetRate {
    /// Caudal no formato que o `tc` aceita (ex.: `10000000bit`).
    fn tc_rate(&self) -> String {
        format!("{}bit", self.rate_bit)
    }
    /// Burst (bytes) no formato que o `tc` aceita (número simples = bytes).
    fn tc_burst(&self) -> String {
        self.burst_bytes.to_string()
    }
}

/// Separa um valor com sufixo `k`/`m`/`g`/`t` do seu multiplicador (base 1000
/// para caudais de rede, 1024 para tamanhos de buffer). Sem sufixo, mult. = 1.
fn split_unit(s: &str, base: u64) -> (&str, u64) {
    let mult = match s.chars().last().map(|c| c.to_ascii_lowercase()) {
        Some('k') => base,
        Some('m') => base * base,
        Some('g') => base * base * base,
        Some('t') => base * base * base * base,
        _ => return (s, 1),
    };
    (&s[..s.len() - 1], mult)
}

/// Caudal humano (`10mbit`, `1g`, `512k`, `1000000`) → bits/segundo. Os sufixos
/// são decimais (k=10³, m=10⁶, g=10⁹), como é convenção em redes; os tokens
/// `bit`/`bps` finais são ignorados.
fn parse_rate_bits(s: &str) -> Result<u64> {
    let lower = s.trim().to_lowercase();
    let body = lower
        .strip_suffix("bps")
        .or_else(|| lower.strip_suffix("bit"))
        .unwrap_or(lower.as_str());
    let (num, mult) = split_unit(body.trim(), 1000);
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| Error::Invalid(format!("--net-bps inválido: '{s}'")))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(Error::Invalid(format!("--net-bps tem de ser positivo: '{s}'")));
    }
    Ok((n * mult as f64) as u64)
}

/// Tamanho humano em bytes (`256k`, `1m`, `4096`). Sufixos binários (k=1024, …);
/// um `b`/`B` final é aceite (`256kb`). Devolve `None` se inválido.
fn parse_size_bytes(s: &str) -> Option<u64> {
    let lower = s.trim().to_lowercase();
    let body = lower.strip_suffix('b').unwrap_or(lower.as_str());
    let (num, mult) = split_unit(body.trim(), 1024);
    let n: f64 = num.trim().parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    Some((n * mult as f64) as u64)
}

/// Interpreta um limite de largura de banda: um caudal (`--net-bps`) e um burst
/// opcional em bytes (`--net-burst`). Sem burst, usa ~100 ms de caudal, com um
/// piso de 16 KiB (suficiente para o token bucket não estrangular o arranque).
pub fn parse_net_rate(rate: &str, burst: Option<&str>) -> Result<NetRate> {
    let rate_bit = parse_rate_bits(rate)?;
    let burst_bytes = match burst {
        Some(b) => {
            let v = parse_size_bytes(b)
                .ok_or_else(|| Error::Invalid(format!("--net-burst inválido: '{b}'")))?;
            if v == 0 {
                return Err(Error::Invalid("--net-burst não pode ser zero".into()));
            }
            v
        }
        None => (rate_bit / 8 / 10).max(16 * 1024),
    };
    Ok(NetRate { rate_bit, burst_bytes })
}

/// VIP estável de um serviço (hash FNV-1a → `10.90.a.b`), fora da subnet dos
/// containers para que o tráfego passe pelo host (onde o nftables balanceia).
pub fn service_vip(key: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for byte in key.bytes() {
        h ^= byte as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
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

/// IP **preferido** (determinístico, puro) num `/16` arbitrário (`<prefix>.A.B`),
/// derivado do id. É só o ponto de partida: sozinho colide por aniversário aos
/// ~300 containers (32 bits do id → 16 bits de host). A unicidade real vem do
/// registo de leases + sondagem em [`ipam::allocate`]; ver [`alloc_ip_in`].
pub fn derive_ip_in(prefix: &str, id: &str) -> String {
    let hex = &id[..id.len().min(8)];
    let n = u32::from_str_radix(hex, 16).unwrap_or(2);
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

/// IP de um container no `/16` de `prefix`, para **recomputar** o IP a partir do
/// id (limpeza: detach/publish/firewall/egress). Consulta o lease persistido
/// primeiro (o IP REAL atribuído no attach, que pode ter sido sondado por cima de
/// uma colisão) e só cai no IP derivado do hash se não houver lease (container
/// pré-registo ou ainda não atacado). **Não cria** lease — quem aloca é
/// [`ipam::allocate`], chamado nos pontos de attach.
pub fn alloc_ip_in(prefix: &str, id: &str) -> String {
    ipam::lookup(prefix, id).unwrap_or_else(|| derive_ip_in(prefix, id))
}

pub fn alloc_ip(id: &str) -> String {
    alloc_ip_in(&default_prefix(), id)
}

/// Converte um IPv4 `a.b.c.d` em `u32`.
fn ipv4_to_u32(ip: &str) -> Option<u32> {
    let o: Vec<u8> = ip.split('.').filter_map(|p| p.parse().ok()).collect();
    if o.len() != 4 {
        return None;
    }
    Some(((o[0] as u32) << 24) | ((o[1] as u32) << 16) | ((o[2] as u32) << 8) | o[3] as u32)
}

fn u32_to_ipv4(n: u32) -> String {
    format!("{}.{}.{}.{}", (n >> 24) & 0xff, (n >> 16) & 0xff, (n >> 8) & 0xff, n & 0xff)
}

/// Aloca um IP determinístico dentro de uma subnet CIDR (ex.: `192.168.1.0/24`),
/// derivado do `id`. Evita o endereço de rede, o broadcast e o `.1` (gateway
/// típico). Usado pelos drivers `macvlan`/`ipvlan`, cuja subnet é a LAN física.
/// Devolve `None` se a subnet for inválida ou não houver hosts suficientes.
pub fn alloc_ip_cidr(subnet: &str, id: &str) -> Option<String> {
    let (base, plen) = subnet.split_once('/')?;
    let plen: u32 = plen.parse().ok()?;
    if plen >= 31 {
        return None;
    }
    let net = ipv4_to_u32(base)? & (u32::MAX << (32 - plen));
    let host_bits = 32 - plen;
    let size = 1u32 << host_bits; // total de endereços
    // Hosts utilizáveis: [2 .. size-2] (salta rede, .1=gateway e broadcast).
    let usable = size.saturating_sub(3);
    if usable == 0 {
        return None;
    }
    let hex = &id[..id.len().min(8)];
    let n = u32::from_str_radix(hex, 16).unwrap_or(2);
    let offset = 2 + (n % usable);
    Some(u32_to_ipv4(net + offset))
}

/// O comprimento do prefixo (`/24`) de uma subnet CIDR, ou `24` por omissão.
pub fn cidr_prefix_len(subnet: &str) -> u32 {
    subnet.rsplit_once('/').and_then(|(_, p)| p.parse().ok()).unwrap_or(24)
}

/// Hash FNV-1a de 32 bits (para derivar a subnet/bridge de uma rede pelo nome).
fn fnv32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for byte in s.bytes() {
        h ^= byte as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// O nome da rede por omissão (a bridge `delonix0`, estilo `docker0`).
pub const DEFAULT_NET: &str = "bridge";

/// Uma rede do Delonix: a bridge por omissão (`delonix0`/`10.88.0.0/16`) ou uma
/// **rede definida pelo utilizador** (bridge + subnet próprias, isolada das
/// outras). Tudo é determinístico a partir do nome + octeto-base.
#[derive(Clone, Debug)]
pub struct Network {
    pub name: String,
    pub bridge: String,
    pub gateway: String,
    pub prefix: String, // ex.: "10.88"
    pub subnet: String, // ex.: "10.88.0.0/16"
    /// Driver: `"bridge"` (omissão), `"macvlan"` ou `"ipvlan"`. Os dois últimos
    /// põem o container directamente na LAN física (interface própria, sem veth).
    pub driver: String,
    /// NIC-pai do host (só `macvlan`/`ipvlan`): a interface física sobre a qual
    /// se cria a sub-interface do container (ex.: `eno1`).
    pub parent: Option<String>,
    /// VXLAN Network Identifier (só `overlay`): o segmento L2 partilhado entre nós.
    pub vni: Option<u32>,
    /// IPs dos nós-pares (só `overlay`): cada entrada é `<node_ip>` (VXLAN plano)
    /// OU `<node_ip>=<wg_pubkey>=<wg_ip>` (overlay CIFRADO via túnel WireGuard).
    pub peers: Vec<String>,
    /// IP de túnel WireGuard DESTE nó (só overlay cifrado, req #6). Presente ⇒
    /// `ensure_overlay_wg` sobe a wg e o FDB do VXLAN usa os `wg_ip` dos peers,
    /// cifrando o transporte.
    pub wg_ip: Option<String>,
}

/// Driver `bridge` (o caso por omissão de uma rede de utilizador/`delonix0`).
pub const DRIVER_BRIDGE: &str = "bridge";
/// Driver `macvlan` — cada container ganha um MAC próprio na LAN do `parent`.
pub const DRIVER_MACVLAN: &str = "macvlan";
/// Driver `ipvlan` — como macvlan mas partilha o MAC do `parent` (modo L2).
pub const DRIVER_IPVLAN: &str = "ipvlan";
/// Driver `overlay` — bridge com uplink VXLAN: L2 partilhado entre vários nós.
pub const DRIVER_OVERLAY: &str = "overlay";
/// Porta UDP do VXLAN (a registada pelo IANA, igual ao Docker/Linux).
pub const VXLAN_PORT: &str = "4789";

impl Network {
    /// `true` se o driver põe o container na LAN física (sem bridge/veth).
    pub fn is_lan_driver(&self) -> bool {
        self.driver == DRIVER_MACVLAN || self.driver == DRIVER_IPVLAN
    }
    /// Nome do device VXLAN desta rede overlay (ex.: `dlxvx0042`).
    pub fn vxlan_dev(&self) -> Option<String> {
        self.vni.map(|v| format!("dlxvx{v:04x}"))
    }
}

impl Network {
    /// A rede por omissão (`delonix0`).
    pub fn default_bridge() -> Self {
        Network {
            name: DEFAULT_NET.to_string(),
            bridge: BRIDGE.to_string(),
            gateway: default_gateway(),
            prefix: default_prefix(),
            subnet: default_subnet(),
            driver: DRIVER_BRIDGE.to_string(),
            parent: None,
            vni: None,
            peers: Vec::new(),
            wg_ip: None,
        }
    }

    /// Constrói uma rede de utilizador com um octeto-base dado (`10.<base>.0.0/16`).
    /// O nome da bridge inclui o base + um hash do nome (único, ≤ 15 chars).
    fn user_with_base(name: &str, base: u8) -> Self {
        let bridge = format!("dlxn{:02x}{:04x}", base, fnv32(name) & 0xffff);
        Network {
            name: name.to_string(),
            bridge,
            gateway: format!("10.{base}.0.1"),
            prefix: format!("10.{base}"),
            subnet: format!("10.{base}.0.0/16"),
            driver: DRIVER_BRIDGE.to_string(),
            parent: None,
            vni: None,
            peers: Vec::new(),
            wg_ip: None,
        }
    }

    /// Constrói uma rede `overlay`: igual a uma bridge de utilizador (mesmo
    /// `/16`/gateway/veth), mas com um uplink VXLAN (`vni`) escravizado à bridge
    /// e FDB para os `peers` — o segmento L2 estende-se a vários nós.
    fn overlay_with_base(name: &str, base: u8, vni: u32, peers: Vec<String>, wg_ip: Option<String>) -> Self {
        let mut n = Self::user_with_base(name, base);
        n.driver = DRIVER_OVERLAY.to_string();
        n.vni = Some(vni);
        n.peers = peers;
        n.wg_ip = wg_ip;
        n
    }

    /// Constrói uma rede `macvlan`/`ipvlan` a partir de um registo: o container
    /// fica na LAN física do `parent`, logo subnet/gateway são da própria LAN
    /// (dados pelo utilizador, não derivados). `prefix` guarda a subnet em CIDR.
    fn lan(name: &str, driver: &str, parent: &str, subnet: &str, gateway: &str) -> Self {
        Network {
            name: name.to_string(),
            bridge: parent.to_string(), // p/ macvlan o "master" é o NIC físico
            gateway: gateway.to_string(),
            prefix: subnet.to_string(), // CIDR completo (ex.: "192.168.1.0/24")
            subnet: subnet.to_string(),
            driver: driver.to_string(),
            parent: Some(parent.to_string()),
            vni: None,
            peers: Vec::new(),
            wg_ip: None,
        }
    }

    /// O octeto-base candidato a partir do nome (intervalo `[100, 239]`, fora de
    /// 88 = default e 90 = VIPs de serviço).
    /// O 2.º octeto da rede, derivado do nome. **TEM de cair no espaço de
    /// workload do ingress** (`10.200.x`–`10.254.x`, ver
    /// `delonix_runtime_core::workload_net`): é lá que o DNAT/firewall do
    /// ingress aceita publicar portas.
    ///
    /// Estava `100 + (fnv32 % 140)` → `10.100.x`–`10.239.x`, e o ingress só
    /// aceita de 200 para cima: **71% dos nomes de rede geravam uma rede onde o
    /// `-p` falhava** com "IP ... fora do espaço de ingress". Era uma lotaria —
    /// `dlx-delonix` calhava em 10.207 (funcionava) e `dlx-delonix-01` em
    /// 10.173 (rebentava). Os limites vêm da constante partilhada, não de
    /// números repetidos à mão: essa fronteira também sustenta o guard
    /// "no-bypass" do túnel, e duplicá-la aqui era o que criou a divergência.
    fn base_for(name: &str) -> u8 {
        let lo = delonix_runtime_core::workload_net::WORKLOAD_IPV4_LO.octets()[1];
        let hi = delonix_runtime_core::workload_net::WORKLOAD_IPV4_HI.octets()[1];
        let span = (hi - lo) as u32 + 1;
        lo + (fnv32(name) % span) as u8
    }
}

/// Registo persistente das redes de utilizador, em `<root>/networks/<nome>`
/// (o ficheiro guarda só o octeto-base; o resto é derivado do nome). A rede
/// `bridge` é implícita (não tem ficheiro).
pub struct NetworkStore {
    dir: std::path::PathBuf,
}

impl NetworkStore {
    pub fn open(root: impl AsRef<std::path::Path>) -> Result<Self> {
        let dir = root.as_ref().join("networks");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.join(name)
    }

    /// Resolve uma rede pelo nome (`bridge`/vazio → a rede por omissão).
    ///
    /// Formato do ficheiro (retrocompatível): um **inteiro simples** = rede
    /// `bridge` com esse octeto-base (formato antigo); ou linhas `chave=valor`
    /// (`driver`/`parent`/`subnet`/`gateway`/`base`) para os drivers novos.
    pub fn get(&self, name: &str) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Ok(Network::default_bridge());
        }
        let body = std::fs::read_to_string(self.path(name))
            .map_err(|_| Error::NotFound(format!("network {name}")))?;
        let trimmed = body.trim();
        // Formato antigo: só o octeto-base → rede bridge.
        if let Ok(base) = trimmed.parse::<u8>() {
            return Ok(Network::user_with_base(name, base));
        }
        // Formato novo: chave=valor.
        let mut kv = std::collections::HashMap::new();
        for line in trimmed.lines() {
            if let Some((k, v)) = line.split_once('=') {
                kv.insert(k.trim(), v.trim().to_string());
            }
        }
        let driver = kv.get("driver").map(String::as_str).unwrap_or(DRIVER_BRIDGE);
        match driver {
            DRIVER_MACVLAN | DRIVER_IPVLAN => {
                let parent = kv.get("parent").cloned().ok_or_else(|| {
                    Error::Invalid(format!("rede '{name}' ({driver}) sem parent"))
                })?;
                let subnet = kv.get("subnet").cloned().ok_or_else(|| {
                    Error::Invalid(format!("rede '{name}' ({driver}) sem subnet"))
                })?;
                let gateway = kv.get("gateway").cloned().unwrap_or_default();
                Ok(Network::lan(name, driver, &parent, &subnet, &gateway))
            }
            DRIVER_OVERLAY => {
                let base: u8 = kv
                    .get("base")
                    .and_then(|b| b.parse().ok())
                    .ok_or_else(|| Error::Invalid(format!("rede '{name}' corrompida")))?;
                let vni: u32 = kv
                    .get("vni")
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| Error::Invalid(format!("rede '{name}' (overlay) sem vni")))?;
                let peers: Vec<String> = kv
                    .get("peers")
                    .map(|p| p.split(',').filter(|s| !s.trim().is_empty()).map(|s| s.trim().to_string()).collect())
                    .unwrap_or_default();
                let wg_ip = kv.get("wgip").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                Ok(Network::overlay_with_base(name, base, vni, peers, wg_ip))
            }
            _ => {
                let base: u8 = kv
                    .get("base")
                    .and_then(|b| b.parse().ok())
                    .ok_or_else(|| Error::Invalid(format!("rede '{name}' corrompida")))?;
                Ok(Network::user_with_base(name, base))
            }
        }
    }

    /// Lista as redes de utilizador (não inclui a `bridge` por omissão).
    pub fn list(&self) -> Result<Vec<Network>> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for entry in rd.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(n) = self.get(name) {
                        out.push(n);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Cria uma rede de utilizador (subnet livre, sem colisão com as existentes).
    pub fn create(&self, name: &str) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Err(Error::Invalid("'bridge' é a rede por omissão (reservada)".into()));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(Error::Invalid(format!("nome de rede inválido: '{name}'")));
        }
        if self.path(name).exists() {
            return Err(Error::Invalid(format!("a rede '{name}' já existe")));
        }
        let used: Vec<u8> = self
            .list()?
            .iter()
            .filter_map(|n| n.prefix.rsplit('.').next().and_then(|o| o.parse().ok()))
            .collect();
        // procura um octeto-base livre a partir do candidato.
        let mut base = Network::base_for(name);
        for _ in 0..140 {
            if !used.contains(&base) {
                break;
            }
            // Wrap DENTRO do espaço de workload (não 100..239, que saía dele).
            base = if base >= delonix_runtime_core::workload_net::WORKLOAD_IPV4_HI.octets()[1] {
                delonix_runtime_core::workload_net::WORKLOAD_IPV4_LO.octets()[1]
            } else {
                base + 1
            };
        }
        std::fs::write(self.path(name), base.to_string())?;
        self.get(name)
    }

    /// Cria uma rede de utilizador com um **octeto-base explícito** (`10.{base}.0.0/16`).
    /// Usado para honrar `spec.subnet` de um `kind: Network` e para ALINHAR o plano
    /// de rede das VMs (infra) a este — o `NetworkStore` é a fonte da verdade do
    /// prefixo. Idempotente: se a rede já existe, devolve-a tal como está.
    pub fn create_with_base(&self, name: &str, base: u8) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Err(Error::Invalid("'bridge' é a rede por omissão (reservada)".into()));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(Error::Invalid(format!("nome de rede inválido: '{name}'")));
        }
        if self.path(name).exists() {
            return self.get(name);
        }
        if !(1..=254).contains(&base) {
            return Err(Error::Invalid(format!(
                "octeto-base /16 inválido: {base} (1..=254)"
            )));
        }
        std::fs::write(self.path(name), base.to_string())?;
        self.get(name)
    }

    /// Octeto-base `/16` livre para o nome dado (evita colisão com as existentes).
    fn free_base(&self, name: &str) -> Result<u8> {
        let used: Vec<u8> = self
            .list()?
            .iter()
            .filter_map(|n| n.prefix.rsplit('.').next().and_then(|o| o.parse().ok()))
            .collect();
        let mut base = Network::base_for(name);
        for _ in 0..140 {
            if !used.contains(&base) {
                break;
            }
            base = if base >= 239 { 100 } else { base + 1 };
        }
        Ok(base)
    }

    /// Cria uma rede `overlay` (bridge + uplink VXLAN): igual a uma rede de
    /// utilizador (`/16` próprio), mas estende-se a vários nós pelo `vni` e pela
    /// lista de `peers` (IPs dos outros nós Delonix). Sem peers, é local mas já
    /// pronta para juntar nós (basta recriar com os mesmos `vni`/`peers` lá).
    pub fn create_overlay(&self, name: &str, vni: u32, peers: &[String], wg_ip: Option<&str>) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Err(Error::Invalid("'bridge' é a rede por omissão (reservada)".into()));
        }
        if name == "host" || name == "none" {
            return Err(Error::Invalid(format!("'{name}' é um driver reservado")));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(Error::Invalid(format!("nome de rede inválido: '{name}'")));
        }
        if self.path(name).exists() {
            return Err(Error::Invalid(format!("a rede '{name}' já existe")));
        }
        if vni == 0 || vni > 0x00ff_ffff {
            return Err(Error::Invalid("VNI inválido (1..16777215)".into()));
        }
        let base = self.free_base(name)?;
        let wgip_line = wg_ip.map(|w| format!("wgip={w}\n")).unwrap_or_default();
        let body = format!(
            "driver=overlay\nbase={base}\nvni={vni}\npeers={}\n{wgip_line}",
            peers.join(",")
        );
        std::fs::write(self.path(name), body)?;
        self.get(name)
    }

    /// Adiciona/atualiza um peer de um overlay existente (idempotente) e devolve a
    /// rede atualizada. `peer` = `<node_ip>` ou `<node_ip>=<pubkey>=<wg_ip>`. É o
    /// bloco do gossip/reconciliador (#6 fase 4): aplicar peers aprendidos. Dedup
    /// pelo `node_ip` (chave/wg_ip podem ter rodado → substitui).
    pub fn add_overlay_peer(&self, name: &str, peer: &str) -> Result<Network> {
        let net = self.get(name)?;
        if net.driver != DRIVER_OVERLAY {
            return Err(Error::Invalid(format!("'{name}' não é um overlay")));
        }
        let (new_ip, _) = parse_overlay_peer(peer);
        if new_ip.is_empty() {
            return Err(Error::Invalid("peer inválido (falta node_ip)".into()));
        }
        let mut peers: Vec<String> = net
            .peers
            .iter()
            .filter(|p| parse_overlay_peer(p).0 != new_ip)
            .cloned()
            .collect();
        peers.push(peer.to_string());
        // re-persiste substituindo SÓ a linha `peers=` (preserva base/vni/wgip).
        let raw = std::fs::read_to_string(self.path(name))
            .map_err(|e| Error::Runtime { context: "ler overlay", message: e.to_string() })?;
        let new_line = format!("peers={}", peers.join(","));
        let mut out: Vec<String> = raw
            .lines()
            .map(|l| if l.starts_with("peers=") { new_line.clone() } else { l.to_string() })
            .collect();
        if !out.iter().any(|l| l.starts_with("peers=")) {
            out.push(new_line);
        }
        std::fs::write(self.path(name), out.join("\n") + "\n")
            .map_err(|e| Error::Runtime { context: "escrever overlay", message: e.to_string() })?;
        self.get(name)
    }

    /// Cria uma rede `macvlan`/`ipvlan`: o container fica directamente na LAN
    /// física do `parent` (ex.: `eno1`), com `subnet`/`gateway` dessa LAN. Valida
    /// o nome, o driver, a existência do NIC-pai e o formato da subnet (CIDR).
    pub fn create_lan(
        &self,
        name: &str,
        driver: &str,
        parent: &str,
        subnet: &str,
        gateway: &str,
    ) -> Result<Network> {
        if name.is_empty() || name == DEFAULT_NET {
            return Err(Error::Invalid("'bridge' é a rede por omissão (reservada)".into()));
        }
        if name == "host" || name == "none" {
            return Err(Error::Invalid(format!("'{name}' é um driver reservado")));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(Error::Invalid(format!("nome de rede inválido: '{name}'")));
        }
        if driver != DRIVER_MACVLAN && driver != DRIVER_IPVLAN {
            return Err(Error::Invalid(format!("driver desconhecido: '{driver}'")));
        }
        if self.path(name).exists() {
            return Err(Error::Invalid(format!("a rede '{name}' já existe")));
        }
        if !link_exists(parent) {
            return Err(Error::Invalid(format!("NIC-pai '{parent}' não existe no host")));
        }
        if alloc_ip_cidr(subnet, "deadbeef").is_none() {
            return Err(Error::Invalid(format!("subnet inválida: '{subnet}' (ex.: 192.168.1.0/24)")));
        }
        // AVISO DE SEGURANÇA (consentimento informado): macvlan/ipvlan põem o container
        // DIRETAMENTE na LAN física do `parent`, com IP/MAC próprios. O tráfego egressa
        // pelo NIC físico ABAIXO da forward chain do host → NÃO é filtrável pela nft do
        // Delonix: SEM firewall por-container, SEM anti-spoof, SEM isolamento inter-rede.
        // É a natureza do macvlan, não um bug — mas o operador tem de o saber. Para
        // isolamento FILTRADO, usa uma rede `bridge` (default). Ver `is_lan_driver`.
        eprintln!(
            "delonix: AVISO DE SEGURANÇA — a rede '{name}' ({driver}) é NÃO-FILTRADA: os \
             containers ficam diretamente na LAN física de '{parent}', FORA do firewall, \
             do anti-spoof e do isolamento do Delonix. Usa uma rede `bridge` se precisares \
             de filtragem."
        );
        let body = format!(
            "driver={driver}\nparent={parent}\nsubnet={subnet}\ngateway={gateway}\n"
        );
        std::fs::write(self.path(name), body)?;
        self.get(name)
    }

    /// Remove o registo de uma rede (não toca na infra-estrutura nft/bridge).
    pub fn remove(&self, name: &str) -> Result<Network> {
        let net = self.get(name)?;
        std::fs::remove_file(self.path(name))
            .map_err(|_| Error::NotFound(format!("network {name}")))?;
        Ok(net)
    }
}

/// Tipos CANÓNICOS da firewall L4 por-container, definidos em `delonix-core`
/// (onde também são persistidos no `Container` record). Re-exportados aqui para
/// que `apply_container_firewall` e a API continuem a usar `delonix_net::ContainerFw`.
pub use delonix_runtime_core::{ContainerFw, FwRule};

/// Nome da chain nft dedicada à firewall de um container (derivado do IP).
fn cfw_chain(ip: &str) -> String {
    format!("cfw{:08x}", fnv32(ip))
}

impl Net {
    /// **Aplica a firewall L4 de um container** (issue Fase-1): traduz as regras
    /// da UI para uma chain nftables dedicada (`cfw<hash-ip>`), com jumps desde o
    /// `forward` para o tráfego de/para o IP do container. Idempotente (reconstrói
    /// a chain a cada chamada). Substitui o "apply" que era só toast na Console.
    pub fn apply_container_firewall(&self, ip: &str, fw: &ContainerFw) -> Result<()> {
        self.ensure_bridge()?; // a tabela `delonix` tem de existir
        let chain = cfw_chain(ip);
        // garante a chain (regular, sem hook — só alvo de jump). NOTA: `capture`
        // devolve Ok mesmo em falha, por isso testa-se o CONTEÚDO, não o erro.
        let exists = capture("nft", &["list", "chain", "ip", TABLE, &chain])
            .map(|o| o.contains(&chain))
            .unwrap_or(false);
        if !exists {
            run_ok("nft", &["add", "chain", "ip", TABLE, &chain]);
        }
        // jumps idempotentes no forward: tráfego PARA (daddr) e DE (saddr) o IP.
        let fwd = capture("nft", &["list", "chain", "ip", TABLE, "forward"]).unwrap_or_default();
        for dir in ["daddr", "saddr"] {
            let needle = format!("ip {dir} {ip} jump {chain}");
            if !fwd.contains(&needle) {
                run_ok("nft", &["add", "rule", "ip", TABLE, "forward", "ip", dir, ip, "jump", &chain]);
            }
        }
        // reconstrói o corpo da chain (regras + política default).
        let mut body = String::new();
        if fw.enabled {
            for r in &fw.rules {
                // Defesa contra injeção nft: salta regras com campos inseguros.
                if !r.nft_safe() {
                    continue;
                }
                let (self_dir, peer_dir) =
                    if r.dir == "out" { ("saddr", "daddr") } else { ("daddr", "saddr") };
                let mut line = format!("ip {self_dir} {ip}");
                if !r.src.is_empty() && r.src != "0.0.0.0/0" && r.src != "*" {
                    line.push_str(&format!(" ip {peer_dir} {}", r.src));
                }
                if !r.proto.is_empty() && r.proto != "any" {
                    line.push_str(&format!(" {}", r.proto));
                    if !r.port.is_empty() && r.port != "*" {
                        line.push_str(&format!(" dport {}", r.port));
                    }
                }
                line.push_str(if r.action == "allow" { " accept" } else { " drop" });
                body.push_str(&format!("\t\t{line}\n"));
            }
            // política por omissão (drop final na direção indicada).
            if fw.policy_in == "deny" {
                body.push_str(&format!("\t\tip daddr {ip} drop\n"));
            }
            if fw.policy_out == "deny" {
                body.push_str(&format!("\t\tip saddr {ip} drop\n"));
            }
        }
        // flush + re-add num único script (a chain mantém-se, os jumps continuam válidos).
        let script = format!(
            "flush chain ip {TABLE} {chain}\ntable ip {TABLE} {{\n\tchain {chain} {{\n{body}\t}}\n}}\n"
        );
        apply_nft(&script)
    }

    /// Remove a firewall de um container: tira os jumps do `forward` (por handle) e
    /// apaga a chain. Chamado no `detach` para não deixar regras órfãs.
    pub fn remove_container_firewall(&self, ip: &str) {
        let chain = cfw_chain(ip);
        // remove os jumps do forward (precisa do handle de cada regra).
        if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, "forward"]) {
            for line in out.lines() {
                if line.contains(&format!("jump {chain}")) {
                    if let Some(h) = line.rsplit("handle ").next().map(|s| s.trim()) {
                        run_ok("nft", &["delete", "rule", "ip", TABLE, "forward", "handle", h]);
                    }
                }
            }
        }
        run_ok("nft", &["delete", "chain", "ip", TABLE, &chain]);
    }

    /// Garante a bridge `delonix0`, o IP forwarding e a tabela nft (NAT + fw).
    pub fn ensure_bridge(&self) -> Result<()> {
        let gateway = default_gateway();
        let subnet = default_subnet();
        if !link_exists(BRIDGE) {
            run("ip", &["link", "add", BRIDGE, "type", "bridge"])?;
            run("ip", &["addr", "add", &format!("{gateway}/16"), "dev", BRIDGE])?;
            run("ip", &["link", "set", BRIDGE, "up"])?;
        }
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");

        if !table_exists() {
            // Uma tabela dedicada: NAT (masquerade + DNAT) + firewall por container.
            let ruleset = format!(
                "table ip {TABLE} {{\n\
                 \tchain postrouting {{\n\
                 \t\ttype nat hook postrouting priority 100;\n\
                 \t\tip saddr {subnet} oifname != \"{BRIDGE}\" masquerade\n\
                 \t}}\n\
                 \tchain prerouting {{\n\
                 \t\ttype nat hook prerouting priority -100;\n\
                 \t}}\n\
                 \tchain output {{\n\
                 \t\ttype nat hook output priority -100;\n\
                 \t}}\n\
                 \tset blocked {{ type ipv4_addr; }}\n\
                 \tchain forward {{\n\
                 \t\ttype filter hook forward priority 0;\n\
                 \t\tip saddr @blocked drop\n\
                 \t\tip daddr @blocked drop\n\
                 \t}}\n\
                 }}\n"
            );
            apply_nft(&ruleset)?;
        }
        // A1: default-deny de entrada na subnet por omissão (idempotente).
        forward_inbound_deny(&subnet);
        Ok(())
    }

    /// Liga um container à rede POR OMISSÃO (`delonix0`). Atalho de [`Net::attach_on`].
    pub fn attach(&self, pid: i32, id: &str) -> Result<String> {
        self.attach_on(&Network::default_bridge(), pid, id)
    }

    /// Garante a infra-estrutura de uma rede de utilizador: a bridge própria, o
    /// `MASQUERADE` da sua subnet e o **isolamento** (drop de forward) face às
    /// outras redes Delonix. A rede por omissão é só a `ensure_bridge`.
    pub fn ensure_network(&self, net: &Network) -> Result<()> {
        // Drivers de LAN (macvlan/ipvlan): não há bridge nem NAT — o container
        // vai directo para a LAN física. Basta garantir o NIC-pai levantado.
        if net.is_lan_driver() {
            if let Some(parent) = &net.parent {
                if !link_exists(parent) {
                    return Err(Error::Invalid(format!("NIC-pai '{parent}' não existe")));
                }
                run_ok("ip", &["link", "set", parent, "up"]);
            }
            return Ok(());
        }
        self.ensure_bridge()?; // a tabela nft vive aqui (vale p/ todas as redes)
        if net.name == DEFAULT_NET {
            return Ok(());
        }
        if !link_exists(&net.bridge) {
            run("ip", &["link", "add", &net.bridge, "type", "bridge"])?;
            run("ip", &["addr", "add", &format!("{}/16", net.gateway), "dev", &net.bridge])?;
            run("ip", &["link", "set", &net.bridge, "up"])?;
        }
        // MASQUERADE da subnet desta rede (saída para a Internet).
        if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "postrouting"]) {
            if !out.contains(&net.subnet) {
                run_ok("nft", &["add", "rule", "ip", TABLE, "postrouting", "ip", "saddr", &net.subnet, "oifname", "!=", &net.bridge, "masquerade"]);
            }
        }
        // A1: default-deny de entrada também na subnet desta rede (idempotente).
        forward_inbound_deny(&net.subnet);
        // Isolamento: bloqueia o forward entre esta bridge e qualquer OUTRA
        // bridge Delonix (containers de redes diferentes não se alcançam).
        for other in list_delonix_bridges() {
            if other != net.bridge {
                self.isolate_pair(&net.bridge, &other);
            }
        }
        // Overlay: cria o uplink VXLAN e escraviza-o à bridge, para o segmento
        // L2 atravessar os nós-pares (FDB por peer).
        if net.driver == DRIVER_OVERLAY {
            self.ensure_vxlan(net)?;
            self.ensure_overlay_wg(net)?; // cifra o transporte VXLAN entre nós (#6)
        }
        Ok(())
    }

    /// Cria (idempotente) o device VXLAN `dlxvx<vni>`, escraviza-o à bridge da
    /// rede overlay e adiciona uma entrada FDB para cada nó-par (frames L2
    /// desconhecidos são replicados para os peers via `dst <ip>`). Multi-nó: cada
    /// nó tem de criar a mesma overlay (mesmo `vni`) listando os outros como peers.
    fn ensure_vxlan(&self, net: &Network) -> Result<()> {
        let Some(vni) = net.vni else { return Ok(()) };
        let Some(dev) = net.vxlan_dev() else { return Ok(()) };
        if !link_exists(&dev) {
            // `nolearning` + FDB manual = controlo determinístico do flooding.
            run(
                "ip",
                &[
                    "link", "add", &dev, "type", "vxlan", "id", &vni.to_string(),
                    "dstport", VXLAN_PORT, "nolearning",
                ],
            )?;
            run_ok("ip", &["link", "set", &dev, "master", &net.bridge]);
            run_ok("ip", &["link", "set", &dev, "up"]);
        }
        // FDB: replica frames L2 desconhecidos (broadcast/unknown-unicast) para
        // cada peer — é o que faz o L2 chegar aos containers nos outros nós.
        let have = capture("bridge", &["fdb", "show", "dev", &dev]).unwrap_or_default();
        for peer in &net.peers {
            // overlay cifrado: o FDB aponta para o wg_ip do peer (não o node_ip),
            // logo o UDP do VXLAN é encaminhado pelo túnel wg = cifrado.
            let (node_ip, wg) = parse_overlay_peer(peer);
            let dst = wg.map(|(_, wgip)| wgip).unwrap_or(node_ip);
            if !have.contains(dst.as_str()) {
                run_ok("bridge", &["fdb", "append", "00:00:00:00:00:00", "dev", &dev, "dst", &dst]);
            }
        }
        Ok(())
    }

    /// WireGuard sobre o overlay (req #6, fase 3): sobe a interface wg do overlay
    /// e configura os peers, de modo a CIFRAR o transporte VXLAN entre nós. Só age
    /// se a rede tiver `wg_ip` (deste nó); senão fica o VXLAN plano (compatível).
    /// O `ensure_vxlan` já aponta o FDB para os `wg_ip` dos peers, por isso o UDP
    /// do VXLAN (4789) viaja pelo túnel wg. Sem `wg` no host → degrada (não cifra).
    fn ensure_overlay_wg(&self, net: &Network) -> Result<()> {
        let Some(my_wg_ip) = net.wg_ip.as_deref() else { return Ok(()) };
        let Some(vni) = net.vni else { return Ok(()) };
        if !crate::wg::available() {
            return Ok(());
        }
        let key = crate::wg::ensure_node_key()?;
        let iface = format!("wgo{vni:06x}"); // ≤ 15 chars
        let port: u16 = 51820;
        crate::wg::ensure_iface(&iface, &key.private, port, &format!("{my_wg_ip}/24"))?;
        for peer in &net.peers {
            let (node_ip, wg) = parse_overlay_peer(peer);
            if let Some((pubkey, wgip)) = wg {
                crate::wg::set_peer(
                    &iface,
                    &crate::wg::Peer {
                        public: pubkey,
                        endpoint: format!("{node_ip}:{port}"),
                        allowed_ips: vec![format!("{wgip}/32")],
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Adiciona (idempotente) as regras de forward que bloqueiam o tráfego
    /// encaminhado entre duas bridges Delonix distintas, nos dois sentidos.
    fn isolate_pair(&self, a: &str, b: &str) {
        let have = capture("nft", &["list", "chain", "ip", TABLE, "forward"]).unwrap_or_default();
        for (i, o) in [(a, b), (b, a)] {
            let needle = format!("iifname \"{i}\" oifname \"{o}\"");
            if !have.contains(&needle) {
                run_ok("nft", &["add", "rule", "ip", TABLE, "forward", "iifname", i, "oifname", o, "drop"]);
            }
        }
    }

    /// Liga um container a uma rede específica (estilo CNI): configura o `netns`
    /// pelo PID na bridge/subnet dessa rede, devolvendo o IP atribuído.
    pub fn attach_on(&self, net: &Network, pid: i32, id: &str) -> Result<String> {
        self.attach_on_ip(net, pid, id, None)
    }

    /// Como [`Net::attach_on`], mas permite **fixar o IP** (`Some(ip)`); `None`
    /// deriva o IP do id (comportamento por omissão). Valida o IP na subnet.
    pub fn attach_on_ip(&self, net: &Network, pid: i32, id: &str, ip: Option<&str>) -> Result<String> {
        self.ensure_network(net)?;
        // Caminho macvlan/ipvlan: cria a sub-interface sobre o NIC-pai e move-a
        // para o netns do container (sem veth, sem bridge — o container fica na
        // LAN física com IP/gateway dessa LAN).
        if net.is_lan_driver() {
            return self.attach_lan(net, pid, id, ip);
        }
        let ip = match ip {
            Some(want) => {
                if !valid_ip_in_subnet(&net.prefix, want) {
                    return Err(Error::Invalid(format!(
                        "IP {want} fora da subnet {} da rede '{}'",
                        net.subnet, net.name
                    )));
                }
                // Regista o IP fixado para a sondagem de outros containers o ver
                // como ocupado (senão auto-alocaria por cima dele).
                ipam::reserve(&net.prefix, id, want);
                want.to_string()
            }
            None => ipam::allocate(&net.prefix, id)?,
        };
        let ns = netns_name(id);
        let hv = host_veth(id);
        let pv = peer_veth(id);

        // Dá um nome ao netns do container (bind-mount de /proc/<pid>/ns/net).
        run("ip", &["netns", "attach", &ns, &pid.to_string()])?;
        // Cria o par veth e move uma ponta para o netns do container.
        run("ip", &["link", "add", &hv, "type", "veth", "peer", "name", &pv])?;
        run("ip", &["link", "set", &pv, "netns", &ns])?;
        // Dentro do container: renomeia para eth0, dá IP, rota e liga.
        run("ip", &["-n", &ns, "link", "set", &pv, "name", "eth0"])?;
        run("ip", &["-n", &ns, "addr", "add", &format!("{ip}/16"), "dev", "eth0"])?;
        run("ip", &["-n", &ns, "link", "set", "eth0", "up"])?;
        run("ip", &["-n", &ns, "link", "set", "lo", "up"])?;
        run("ip", &["-n", &ns, "route", "add", "default", "via", &net.gateway])?;
        // No host: liga a ponta à bridge desta rede e levanta.
        run("ip", &["link", "set", &hv, "master", &net.bridge])?;
        run("ip", &["link", "set", &hv, "up"])?;
        // ANTI-SPOOFING (paridade com o caminho rootless `infra.rs::do_attach`): o
        // container só pode emitir com o SEU IP de origem. Sem isto pode forjar o
        // `saddr` e contornar as regras por-IP do firewall (as garantias por-IP
        // deixariam de ser reais). Idempotente (limpa antes); `insert` põe a regra no
        // TOPO do `forward`, antes dos jumps por-container e dos drops de `@blocked`.
        clear_antispoof_root(&hv);
        run_ok(
            "nft",
            &["insert", "rule", "ip", TABLE, "forward", "iifname", &hv, "ip", "saddr", "!=", &ip, "drop"],
        );
        Ok(ip)
    }

    /// Liga um container a uma rede `macvlan`/`ipvlan`: cria a sub-interface no
    /// host (sobre o `parent`), move-a para o netns, dá IP (da LAN) + rota.
    fn attach_lan(&self, net: &Network, pid: i32, id: &str, ip: Option<&str>) -> Result<String> {
        let parent = net.parent.as_deref().ok_or_else(|| {
            Error::Invalid(format!("rede '{}' sem NIC-pai", net.name))
        })?;
        let ip = match ip {
            Some(want) => want.to_string(),
            None => alloc_ip_cidr(&net.subnet, id).ok_or_else(|| {
                Error::Invalid(format!("não há IP livre na subnet {}", net.subnet))
            })?,
        };
        let plen = cidr_prefix_len(&net.subnet);
        let ns = netns_name(id);
        let dev = peer_veth(id); // nome temporário no host antes de mover/renomear
        let (kind, mode) = if net.driver == DRIVER_IPVLAN {
            ("ipvlan", "l2")
        } else {
            ("macvlan", "bridge")
        };
        run("ip", &["netns", "attach", &ns, &pid.to_string()])?;
        // Cria a sub-interface sobre o NIC-pai e empurra-a para o netns.
        run("ip", &["link", "add", &dev, "link", parent, "type", kind, "mode", mode])?;
        run("ip", &["link", "set", &dev, "netns", &ns])?;
        run("ip", &["-n", &ns, "link", "set", &dev, "name", "eth0"])?;
        run("ip", &["-n", &ns, "addr", "add", &format!("{ip}/{plen}"), "dev", "eth0"])?;
        run("ip", &["-n", &ns, "link", "set", "eth0", "up"])?;
        run("ip", &["-n", &ns, "link", "set", "lo", "up"])?;
        if !net.gateway.is_empty() {
            run_ok("ip", &["-n", &ns, "route", "add", "default", "via", &net.gateway]);
        }
        Ok(ip)
    }

    /// Desliga um container da rede e limpa o `veth`, o nome do netns e o
    /// eventual bloqueio/publicação. `ip` é o IP real do container (de uma rede
    /// de utilizador); se `None`, assume a subnet por omissão.
    pub fn detach(&self, id: &str, ip: Option<&str>) -> Result<()> {
        let ns = netns_name(id);
        let hv = host_veth(id);
        self.clear_net_rate(id); // remove qualquer limite de banda (tc) do veth
        clear_antispoof_root(&hv); // remove a regra anti-spoof do forward (antes do veth)
        run_ok("ip", &["link", "del", &hv]); // remove o par veth
        run_ok("ip", &["netns", "del", &ns]); // remove o nome do netns
        let ip = ip.map(String::from).unwrap_or_else(|| alloc_ip(id));
        run_ok("nft", &["delete", "element", "ip", TABLE, "blocked", &format!("{{ {ip} }}")]);
        self.unpublish_all(&ip); // remove as regras DNAT de portas publicadas
        self.remove_container_firewall(&ip); // remove a chain/jumps de firewall L4
        ipam::release(&ipam::prefix_of(&ip), id); // liberta o lease de IP para reuso
        Ok(())
    }

    /// Liga um container **já em execução** a uma rede ADICIONAL (multi-homing,
    /// estilo `docker network connect`). Cria uma nova interface `eth<idx>` no
    /// netns existente, com veths sufixados por `idx` (>=1) para não colidirem com
    /// a interface primária. NÃO mexe na rota por omissão (essa pertence à rede
    /// primária). Devolve o IP atribuído (fixo se `Some`, senão derivado). O netns
    /// já tem nome (a rede primária fê-lo); se não, nomeia-o pelo `pid`.
    pub fn attach_extra(&self, net: &Network, pid: i32, id: &str, ip: Option<&str>, idx: u32) -> Result<String> {
        self.ensure_network(net)?;
        let ip = match ip {
            Some(want) => {
                if !valid_ip_in_subnet(&net.prefix, want) {
                    return Err(Error::Invalid(format!(
                        "IP {want} fora da subnet {} da rede '{}'",
                        net.subnet, net.name
                    )));
                }
                ipam::reserve(&net.prefix, id, want);
                want.to_string()
            }
            None => ipam::allocate(&net.prefix, id)?,
        };
        let ns = netns_name(id);
        let hv = host_veth_n(id, idx);
        let pv = peer_veth_n(id, idx);
        let eth = format!("eth{idx}");

        // Garante que o netns tem nome (idempotente: ignora "File exists").
        if !std::path::Path::new(&format!("/var/run/netns/{ns}")).exists() {
            run("ip", &["netns", "attach", &ns, &pid.to_string()])?;
        }
        run("ip", &["link", "add", &hv, "type", "veth", "peer", "name", &pv])?;
        run("ip", &["link", "set", &pv, "netns", &ns])?;
        run("ip", &["-n", &ns, "link", "set", &pv, "name", &eth])?;
        run("ip", &["-n", &ns, "addr", "add", &format!("{ip}/16"), "dev", &eth])?;
        run("ip", &["-n", &ns, "link", "set", &eth, "up"])?;
        // Liga a ponta do host à bridge desta rede (sem tocar na rota default).
        run("ip", &["link", "set", &hv, "master", &net.bridge])?;
        run("ip", &["link", "set", &hv, "up"])?;
        Ok(ip)
    }

    /// Desliga uma interface ADICIONAL (`docker network disconnect`): remove o
    /// veth `idx` e as eventuais publicações no IP dessa rede. Não toca no netns
    /// nem na interface primária. Best-effort no veth (pode já não existir).
    pub fn detach_extra(&self, id: &str, idx: u32, ip: &str) -> Result<()> {
        let hv = host_veth_n(id, idx);
        run_ok("ip", &["link", "del", &hv]);
        run_ok("nft", &["delete", "element", "ip", TABLE, "blocked", &format!("{{ {ip} }}")]);
        self.unpublish_all(ip);
        Ok(())
    }

    /// Remove a infra-estrutura de uma rede de utilizador: a bridge e as suas
    /// regras nft (masquerade + isolamento). Best-effort.
    pub fn remove_network(&self, net: &Network) -> Result<()> {
        if net.name == DEFAULT_NET {
            return Err(Error::Invalid("a rede 'bridge' por omissão não se remove (usa `network prune`)".into()));
        }
        // regras de forward (isolamento) que mencionam esta bridge.
        if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, "forward"]) {
            for line in out.lines() {
                if line.contains(&format!("\"{}\"", net.bridge)) {
                    if let Some(h) = line.rsplit("# handle ").next() {
                        run_ok("nft", &["delete", "rule", "ip", TABLE, "forward", "handle", h.trim()]);
                    }
                }
            }
        }
        // regra de masquerade da subnet.
        if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, "postrouting"]) {
            for line in out.lines() {
                if line.contains(&net.subnet) {
                    if let Some(h) = line.rsplit("# handle ").next() {
                        run_ok("nft", &["delete", "rule", "ip", TABLE, "postrouting", "handle", h.trim()]);
                    }
                }
            }
        }
        run_ok("ip", &["link", "del", &net.bridge]);
        Ok(())
    }

    /// Publica uma porta do host → `container_ip:cont_port` (DNAT), acessível de
    /// fora e via `localhost`. `spec` é `hostPort:contPort[/tcp|udp]` ou só `port`.
    pub fn publish_port(&self, container_ip: &str, spec: &str) -> Result<()> {
        self.ensure_bridge()?;
        // localhost → DNAT precisa de route_localnet na bridge.
        let _ = std::fs::write(
            format!("/proc/sys/net/ipv4/conf/{BRIDGE}/route_localnet"),
            "1",
        );
        let (host_port, cont_port, proto) = parse_publish(spec)?;
        let to = format!("{container_ip}:{cont_port}");
        // SEGURO POR OMISSÃO: a porta fica só no LOOPBACK (regra `output` abaixo).
        // Exposição externa (LAN/outras máquinas) exige opt-in explícito via
        // DELONIX_PUBLISH_ADDR ("0.0.0.0" = todas as interfaces; ou um IP do host).
        match std::env::var("DELONIX_PUBLISH_ADDR")
            .ok()
            .filter(|a| a.parse::<std::net::Ipv4Addr>().is_ok())
        {
            Some(ref ip) if ip == "0.0.0.0" => {
                run("nft", &["add", "rule", "ip", TABLE, "prerouting", &proto, "dport", &host_port, "dnat", "to", &to])?;
            }
            Some(ip) => {
                run("nft", &["add", "rule", "ip", TABLE, "prerouting", "ip", "daddr", &ip, &proto, "dport", &host_port, "dnat", "to", &to])?;
            }
            None => {} // loopback-only (default seguro): sem DNAT na prerouting externa
        }
        // Do próprio host (curl localhost:porta) — sempre.
        run("nft", &["add", "rule", "ip", TABLE, "output", "ip", "daddr", "127.0.0.0/8", &proto, "dport", &host_port, "dnat", "to", &to])?;
        // Hairpin: tráfego vindo do loopback tem de ser masquerade, senão o
        // container responde a 127.0.0.1 (a SI próprio) e a resposta nunca volta.
        run("nft", &["add", "rule", "ip", TABLE, "postrouting", "ip", "saddr", "127.0.0.0/8", "ip", "daddr", container_ip, "masquerade"])?;
        Ok(())
    }

    /// Remove todas as regras de publicação (DNAT + hairpin) que mencionam
    /// `container_ip` (limpeza no `rm`/`detach`).
    pub fn unpublish_all(&self, container_ip: &str) {
        for chain in ["prerouting", "output", "postrouting"] {
            if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, chain]) {
                for line in out.lines() {
                    // O `daddr <ip>` / `dnat to <ip>:` identifica regras deste container.
                    if line.contains(&format!("dnat to {container_ip}:"))
                        || line.contains(&format!("daddr {container_ip} "))
                    {
                        if let Some(handle) = line.rsplit("# handle ").next() {
                            let handle = handle.trim();
                            run_ok("nft", &["delete", "rule", "ip", TABLE, chain, "handle", handle]);
                        }
                    }
                }
            }
        }
    }

    /// Remove a publicação de UMA porta de host (DNAT em prerouting+output) de um
    /// container, sem mexer nas outras nem no hairpin partilhado. Best-effort.
    pub fn unpublish_port(&self, container_ip: &str, host_port: &str) {
        for chain in ["prerouting", "output"] {
            if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, chain]) {
                for line in out.lines() {
                    if line.contains(&format!("dnat to {container_ip}:"))
                        && line.contains(&format!("dport {host_port} "))
                    {
                        if let Some(handle) = line.rsplit("# handle ").next() {
                            run_ok("nft", &["delete", "rule", "ip", TABLE, chain, "handle", handle.trim()]);
                        }
                    }
                }
            }
        }
    }

    /// Resumo estruturado da firewall do Delonix (a tabela nft `delonix`): DNAT
    /// (portas publicadas), IPs bloqueados, pares de isolamento entre redes e
    /// masquerades de saída. Para o painel Firewall da consola (#10).
    pub fn firewall_summary(&self) -> FirewallSummary {
        let mut s = FirewallSummary::default();
        // DNAT (portas publicadas) — da chain prerouting.
        if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "prerouting"]) {
            for line in out.lines() {
                let l = line.trim();
                if let Some(i) = l.find("dnat to ") {
                    let to = l[i + 8..].split_whitespace().next().unwrap_or("").to_string();
                    let proto = if l.starts_with("udp") { "udp" } else { "tcp" }.to_string();
                    let dport = l.split("dport ").nth(1).and_then(|x| x.split_whitespace().next()).unwrap_or("").to_string();
                    if !to.is_empty() && !dport.is_empty() {
                        s.dnat.push(DnatRule { proto, host_port: dport, to });
                    }
                }
            }
        }
        // IPs bloqueados (set `blocked`).
        if let Ok(out) = capture("nft", &["list", "set", "ip", TABLE, "blocked"]) {
            if let Some(i) = out.find("elements = {") {
                let rest = &out[i + 12..];
                if let Some(j) = rest.find('}') {
                    for ip in rest[..j].split(',') {
                        let ip = ip.trim();
                        if !ip.is_empty() {
                            s.blocked.push(ip.to_string());
                        }
                    }
                }
            }
        }
        // Isolamento entre redes (forward drops) + masquerades (postrouting).
        if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "forward"]) {
            for line in out.lines() {
                let l = line.trim();
                if l.contains("drop") && l.contains("iifname") && l.contains("oifname") {
                    let a = l.split("iifname ").nth(1).and_then(|x| x.split_whitespace().next()).unwrap_or("").trim_matches('"').to_string();
                    let b = l.split("oifname ").nth(1).and_then(|x| x.split_whitespace().next()).unwrap_or("").trim_matches('"').to_string();
                    if !a.is_empty() && !b.is_empty() {
                        s.isolation.push(format!("{a} ✗ {b}"));
                    }
                }
            }
        }
        if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "postrouting"]) {
            for line in out.lines() {
                let l = line.trim();
                if l.contains("masquerade") {
                    if let Some(sub) = l.split("saddr ").nth(1).and_then(|x| x.split_whitespace().next()) {
                        s.masquerade.push(sub.to_string());
                    }
                }
            }
        }
        s
    }

    /// Garante (uma vez) a regra de masquerade das ligações balanceadas: o tráfego
    /// para um VIP de serviço tem de ser SNAT-ado, senão o backend responde ao
    /// cliente directamente (na bridge) e a resposta não passa pelo un-DNAT.
    fn ensure_vip_masq(&self) {
        if let Ok(out) = capture("nft", &["list", "chain", "ip", TABLE, "postrouting"]) {
            if out.contains(VIP_SUBNET) {
                return;
            }
        }
        let _ = apply_nft(&format!(
            "add rule ip {TABLE} postrouting ct original ip daddr {VIP_SUBNET} masquerade\n"
        ));
    }

    /// (Re)define o balanceamento L4 de um VIP de serviço para os `backends`
    /// (round-robin por-ligação via `numgen inc` + conntrack). Idempotente.
    pub fn set_service_lb(&self, vip: &str, backends: &[String]) -> Result<()> {
        self.set_service_lb_algo(vip, backends, "round-robin")
    }

    /// Como [`set_service_lb`], mas com **algoritmo** selecionável (C6):
    /// - `round-robin` — `numgen inc` (distribui por ligação, por ordem).
    /// - `random` — `numgen random` (distribui aleatoriamente).
    /// - `ip-hash` / `sticky` — `jhash ip saddr` (**afinidade de sessão**: o mesmo
    ///   cliente cai sempre no mesmo backend, enquanto o pool não mudar).
    /// - `weighted` — backends `ip:port#peso` (peso ≥1); repete o backend no map
    ///   proporcionalmente ao peso (sem peso → round-robin).
    /// nftables faz a seleção no kernel (zero cópia em userspace). `least-conn` não é
    /// expressável só com `dnat`/`numgen` — fica para o caminho L7 (follow-up).
    pub fn set_service_lb_algo(&self, vip: &str, backends: &[String], algo: &str) -> Result<()> {
        self.ensure_bridge()?;
        self.ensure_vip_masq();
        self.clear_service_lb(vip);
        if backends.is_empty() {
            return Ok(());
        }
        // peso opcional "ip:port#peso" → expande a lista para o map ponderado.
        let expand = || -> Vec<String> {
            let mut out = Vec::new();
            for b in backends {
                if let Some((ipp, w)) = b.rsplit_once('#') {
                    let n: usize = w.parse().unwrap_or(1).clamp(1, 64);
                    for _ in 0..n { out.push(ipp.to_string()); }
                } else {
                    out.push(b.clone());
                }
            }
            out
        };
        let strip = |b: &str| b.rsplit_once('#').map(|(a, _)| a.to_string()).unwrap_or_else(|| b.to_string());
        let rule = if backends.len() == 1 {
            format!("add rule ip {TABLE} prerouting ip daddr {vip} dnat to {}\n", strip(&backends[0]))
        } else {
            let pool = if algo == "weighted" { expand() } else { backends.iter().map(|b| strip(b)).collect() };
            let map = pool.iter().enumerate().map(|(i, ip)| format!("{i} : {ip}")).collect::<Vec<_>>().join(", ");
            let selector = match algo {
                "random" => format!("numgen random mod {}", pool.len()),
                "ip-hash" | "sticky" => format!("jhash ip saddr mod {}", pool.len()),
                // round-robin (default) e weighted (já expandido) usam numgen inc.
                _ => format!("numgen inc mod {}", pool.len()),
            };
            format!("add rule ip {TABLE} prerouting ip daddr {vip} dnat to {selector} map {{ {map} }}\n")
        };
        apply_nft(&rule)
    }

    /// Remove a regra de LB de um VIP (no `down`/`scale`).
    pub fn clear_service_lb(&self, vip: &str) {
        let needle = format!("ip daddr {vip} ");
        if let Ok(out) = capture("nft", &["-a", "list", "chain", "ip", TABLE, "prerouting"]) {
            for line in out.lines() {
                if line.contains(&needle) && line.contains("dnat") {
                    if let Some(handle) = line.rsplit("# handle ").next() {
                        run_ok("nft", &["delete", "rule", "ip", TABLE, "prerouting", "handle", handle.trim()]);
                    }
                }
            }
        }
    }

    /// Micro-segmentação (B14): nega ou liberta o tráfego ENTRE dois IPs de
    /// container (nos dois sentidos), com regras de `forward`. Activa o
    /// `bridge-nf-call-iptables` para que o filtro também veja o tráfego comutado
    /// na MESMA bridge (senão só apanharia o encaminhado entre subnets).
    pub fn set_policy(&self, from_ip: &str, to_ip: &str, deny: bool) -> Result<()> {
        self.ensure_bridge()?;
        // o filtro IP tem de ver o tráfego em ponte (como faz o Docker).
        let _ = std::fs::write("/proc/sys/net/bridge/bridge-nf-call-iptables", "1");
        for (a, b) in [(from_ip, to_ip), (to_ip, from_ip)] {
            let needle = format!("ip saddr {a} ip daddr {b} ");
            let have = capture("nft", &["-a", "list", "chain", "ip", TABLE, "forward"]).unwrap_or_default();
            if deny {
                if !have.lines().any(|l| l.contains(&needle) && l.contains("drop")) {
                    run("nft", &["add", "rule", "ip", TABLE, "forward", "ip", "saddr", a, "ip", "daddr", b, "drop"])?;
                }
            } else {
                for line in have.lines() {
                    if line.contains(&needle) && line.contains("drop") {
                        if let Some(h) = line.rsplit("# handle ").next() {
                            run_ok("nft", &["delete", "rule", "ip", TABLE, "forward", "handle", h.trim()]);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Firewall por container: bloqueia (ou liberta) o IP no `set` `blocked`.
    pub fn set_egress(&self, id: &str, allow: bool) -> Result<()> {
        self.ensure_bridge()?;
        let ip = alloc_ip(id);
        let elem = format!("{{ {ip} }}");
        if allow {
            run_ok("nft", &["delete", "element", "ip", TABLE, "blocked", &elem]);
        } else {
            run("nft", &["add", "element", "ip", TABLE, "blocked", &elem])?;
        }
        Ok(())
    }

    /// Limita a largura de banda de um container no `veth` do lado do host (a
    /// peça que faltava à história "o host nunca morre": sem isto um container
    /// satura o uplink/bridge e degrada a API e o host). Aplica, no MESMO caudal:
    /// - **DOWNLOAD** (host → container): o host transmite no *egress* do veth,
    ///   por isso modela-se com um TBF (token bucket) na raiz;
    /// - **UPLOAD** (container → host): o tráfego do container chega ao veth como
    ///   *ingress*; aí só dá para `police`+`drop` (descarta acima do caudal), o que
    ///   chega para impedir que sature o uplink.
    ///
    /// Idempotente: limpa qualquer qdisc anterior antes de reaplicar. Verifica-se
    /// com `tc qdisc show dev <veth>` (mostra o `tbf` na raiz e o `ingress`).
    pub fn set_net_rate(&self, id: &str, rate: &NetRate) -> Result<()> {
        let hv = host_veth(id);
        self.clear_net_rate(id); // reaplicação limpa
        let r = rate.tc_rate();
        let b = rate.tc_burst();
        // DOWNLOAD: TBF no egress (a raiz do veth do host).
        run("tc", &["qdisc", "add", "dev", &hv, "root", "tbf", "rate", &r, "burst", &b, "latency", "50ms"])?;
        // UPLOAD: qdisc de ingress + filtro que aplica `police`+`drop` a tudo.
        run("tc", &["qdisc", "add", "dev", &hv, "handle", "ffff:", "ingress"])?;
        run(
            "tc",
            &[
                "filter", "add", "dev", &hv, "parent", "ffff:", "protocol", "all", "prio", "1",
                "u32", "match", "u32", "0", "0", "police", "rate", &r, "burst", &b, "drop",
            ],
        )?;
        Ok(())
    }

    /// Remove o limite de largura de banda do `veth` (best-effort). Chamado no
    /// `detach` e antes de reaplicar. Apagar o `veth` já leva os qdiscs consigo,
    /// mas limpamos explicitamente para o caso de o link sobreviver (reaplicação,
    /// container órfão).
    pub fn clear_net_rate(&self, id: &str) {
        let hv = host_veth(id);
        run_ok("tc", &["qdisc", "del", "dev", &hv, "root"]);
        run_ok("tc", &["qdisc", "del", "dev", &hv, "handle", "ffff:", "ingress"]);
    }

    /// Importa um ficheiro `iptables-save`: lê, resume a intenção do utilizador
    /// e traduz uma amostra para `nft` (NÃO altera o host — preserva, informa).
    pub fn import_iptables(&self, path: &std::path::Path) -> Result<String> {
        let text = std::fs::read_to_string(path)?;
        let mut tables = 0usize;
        let mut chains = 0usize;
        let mut rules = 0usize;
        let mut sample: Option<String> = None;
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with('*') {
                tables += 1;
            } else if l.starts_with(':') {
                chains += 1;
            } else if l.starts_with("-A") {
                rules += 1;
                if sample.is_none() {
                    sample = Some(l.to_string());
                }
            }
        }
        let mut report = format!(
            "iptables-save: {tables} tabela(s), {chains} chain(s), {rules} regra(s) — intenção preservada"
        );
        if let Some(rule) = sample {
            // `iptables-translate` mostra o equivalente nft (dry-run).
            let args: Vec<&str> = rule.split_whitespace().collect();
            if let Ok(nft) = capture("iptables-translate", &args) {
                let nft = nft.trim();
                if !nft.is_empty() {
                    report.push_str(&format!("\n  exemplo: {rule}\n     -> nft {nft}"));
                }
            }
        }
        Ok(report)
    }

    /// Remove toda a infra-estrutura de rede do Delonix (todas as bridges e a
    /// tabela nft) — também as bridges das redes de utilizador.
    pub fn teardown(&self) -> Result<()> {
        run_ok("nft", &["delete", "table", "ip", TABLE]);
        for br in list_delonix_bridges() {
            run_ok("ip", &["link", "del", &br]);
        }
        run_ok("ip", &["link", "del", BRIDGE]);
        Ok(())
    }
}

/// IP/gateway/DNS por omissão do slirp4netns (rede rootless).
pub const SLIRP_IP: &str = "10.0.2.100";
pub const SLIRP_DNS: &str = "10.0.2.3";

/// Liga uma rede **rootless** ao container via `slirp4netns`: cria um `tap0` no
/// netns do container (pelo PID) com NAT em *userspace* — **sem root**. Espera o
/// sinal de pronto (`--ready-fd`) antes de devolver; o processo slirp segue a
/// vida do container (sai quando o netns desaparece). (A13.)
/// Caminho do api-socket do slirp PRÓPRIO de um container (caminho
/// slirp-por-container, sem rede custom), pelo PID do seu init.
///
/// **A convenção do nome vive só aqui.** O `container update` precisa deste
/// caminho para publicar/despublicar portas a quente, e duplicar o `format!`
/// do lado da CLI faria as duas metades divergirem em silêncio no dia em que
/// isto mudasse.
pub fn slirp_container_sock(pid: i32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("delonix-slirp-{pid}.sock"))
}

pub fn slirp_attach(pid: i32, publish: &[String]) -> Result<()> {
    // Se há portas a publicar, abrimos o api-socket do slirp para lhe pedir os
    // *host-forwards* (publicação de portas SEM root, como o Podman rootless).
    let api_sock = if publish.is_empty() { None } else { Some(slirp_container_sock(pid)) };
    let mut fds = [0i32; 2];
    // SAFETY: pipe() preenche 2 fds.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(Error::Runtime { context: "pipe", message: "slirp ready-fd".into() });
    }
    let (rd, wr) = (fds[0], fds[1]);
    let mut args = vec![
        "--configure".to_string(),
        "--mtu=65520".to_string(),
        "--disable-host-loopback".to_string(),
        format!("--ready-fd={wr}"),
    ];
    if let Some(sock) = &api_sock {
        let _ = std::fs::remove_file(sock);
        args.push(format!("--api-socket={}", sock.display()));
    }
    args.push(pid.to_string());
    args.push("tap0".to_string());
    let spawned = Command::new("slirp4netns")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // SAFETY: o pai fecha a sua cópia de escrita; só o slirp a mantém.
    unsafe { libc::close(wr) };
    match spawned {
        Ok(child) => {
            // Espera o byte de "pronto" (rede configurada) antes de continuar.
            let mut b = [0u8; 1];
            // SAFETY: lê 1 byte do read-end; bloqueia até o slirp sinalizar.
            unsafe {
                libc::read(rd, b.as_mut_ptr() as *mut libc::c_void, 1);
                libc::close(rd);
            }
            // Publica as portas via o api-socket (host → container, em userspace).
            if let Some(sock) = &api_sock {
                for spec in publish {
                    if let Ok((hp, cp, proto)) = parse_publish(spec) {
                        if let Err(e) = slirp_add_hostfwd(sock, &hp, &cp, &proto) {
                            std::mem::forget(child);
                            return Err(e);
                        }
                    }
                }
            }
            // O slirp corre durante a vida do container — não esperamos por ele.
            std::mem::forget(child);
            Ok(())
        }
        Err(e) => {
            // SAFETY: fecha o read-end no erro.
            unsafe { libc::close(rd) };
            Err(Error::Runtime { context: "slirp4netns", message: e.to_string() })
        }
    }
}

/// **Reaper de slirp4netns órfãos** (#1 port-leak): quando o processo de um
/// container sai SOZINHO (crash/exit, sem `delonix stop`), o `slirp4netns` que lhe
/// servia a rede pode ficar a correr — segurando a porta de host publicada, o que
/// bloqueia o re-arranque ("add_hostfwd failed"). Varre `/proc` uma vez, identifica
/// os slirp4netns cujo **pid-alvo** (último arg numérico do cmdline) já não existe e
/// mata-os; remove também os api-sockets `delonix-slirp-<pid>.sock` obsoletos.
/// Barato (uma passagem por /proc) e seguro (só mexe em slirp4netns com alvo morto).
/// Devolve quantos reapou.
pub fn reap_orphan_slirp() -> usize {
    // Alvo morto = órfão. `kill(pid, 0)` == 0 ⇒ existe; ESRCH ⇒ morto.
    // SAFETY: kill com sinal 0 não envia sinal — só testa a existência do pid.
    reap_slirp_where(|target| unsafe { libc::kill(target, 0) } != 0)
}

/// **Mata o slirp4netns de UM container** (o que serve `target_pid`) e espera
/// que ele largue mesmo a porta de host. Devolve `true` se matou algum.
///
/// Existe por causa de uma race 100% reproduzível: o `slirp4netns` só sai
/// quando NOTA que o netns do alvo desapareceu, e até lá continua a segurar a
/// porta publicada no host. Um `delonix container stop && delonix container
/// start` — o idioma de restart mais natural que há — falhava sempre com
/// `add_hostfwd: slirp_add_hostfwd failed`, e passava a funcionar uns segundos
/// depois, sozinho. O `stop` tem de largar os recursos que o `run` tomou, de
/// forma síncrona, em vez de deixar isso ao acaso.
///
/// Cirúrgico por desenho: só toca no slirp cujo alvo é EXACTAMENTE este pid.
/// Ao contrário de [`reap_orphan_slirp`], não depende de o alvo já estar morto
/// — o chamador é quem o matou.
pub fn reap_slirp_for(target_pid: i32) -> bool {
    let n = reap_slirp_where(|target| target == target_pid);
    if n == 0 {
        return false;
    }
    // Espera curta até o processo sair de facto: o SIGTERM é assíncrono e sem
    // isto o `start` seguinte voltava a apanhar a porta ainda ocupada — que é
    // exactamente o bug que este código existe para fechar.
    for _ in 0..50 {
        if !slirp_exists_for(target_pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    true
}

/// Varre `/proc` à procura de processos `slirp4netns` e mata (SIGTERM) aqueles
/// cujo pid-alvo satisfaz `should_reap`. Devolve quantos matou.
///
/// A varredura estava embutida em `reap_orphan_slirp`; foi extraída para que a
/// reaper cirúrgica ([`reap_slirp_for`]) partilhe exactamente a mesma lógica de
/// identificação — duas cópias divergiriam no dia em que o argv do slirp mudasse.
fn reap_slirp_where(should_reap: impl Fn(i32) -> bool) -> usize {
    let mut reaped = 0;
    for (pid, target) in list_slirps() {
        if !should_reap(target) {
            continue;
        }
        // SAFETY: SIGTERM a um slirp4netns identificado pelo seu argv.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let _ = std::fs::remove_file(slirp_container_sock(target));
        reaped += 1;
    }
    reaped
}

fn slirp_exists_for(target_pid: i32) -> bool {
    list_slirps().into_iter().any(|(_, t)| t == target_pid)
}

/// `(pid do slirp, pid do container que serve)` de cada slirp4netns a correr.
fn list_slirps() -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let name = e.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<i32>() else {
            continue; // não é um directório de processo
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        // cmdline = args separados por NUL. O argv[0] tem de ser slirp4netns.
        let argv: Vec<&[u8]> = cmdline.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
        if argv.is_empty() || !argv[0].ends_with(b"slirp4netns") {
            continue;
        }
        // o pid-alvo é o penúltimo arg (… <pid> tap0). Acha o último arg numérico.
        let target = argv.iter().rev().find_map(|a| std::str::from_utf8(a).ok().and_then(|s| s.parse::<i32>().ok()));
        if let Some(t) = target {
            out.push((pid, t));
        }
    }
    out
}

/// Pede ao slirp4netns (via o api-socket JSON) um *host-forward* `host_port` →
/// `guest_port` no IP do container ([`SLIRP_IP`]). É como o Podman publica portas
/// em rootless. Tenta brevemente até o socket existir (o slirp cria-o ao arrancar).
pub fn slirp_add_hostfwd(sock: &std::path::Path, host_port: &str, guest_port: &str, proto: &str) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    // SEGURO POR OMISSÃO: liga a porta publicada só ao loopback (127.0.0.1), não a
    // todas as interfaces. Para expor na LAN, opt-in explícito via
    // DELONIX_PUBLISH_ADDR (ex.: "0.0.0.0" ou um IP do host). Validado como IPv4
    // para não injetar no JSON do api-socket do slirp.
    let host_addr = std::env::var("DELONIX_PUBLISH_ADDR")
        .ok()
        .filter(|a| a.parse::<std::net::Ipv4Addr>().is_ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let cmd = format!(
        r#"{{"execute":"add_hostfwd","arguments":{{"proto":"{proto}","host_addr":"{host_addr}","host_port":{host_port},"guest_addr":"{SLIRP_IP}","guest_port":{guest_port}}}}}"#
    );
    let mut last = String::new();
    for _ in 0..50 {
        match UnixStream::connect(sock) {
            Ok(mut s) => {
                let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                s.write_all(cmd.as_bytes()).map_err(|e| reg_io(&e))?;
                let mut resp = String::new();
                let _ = s.read_to_string(&mut resp);
                if resp.contains("\"error\"") {
                    return Err(Error::Runtime {
                        context: "slirp hostfwd",
                        message: format!("porta {host_port}: {}", resp.trim()),
                    });
                }
                return Ok(()); // {"return":{}} = sucesso
            }
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
    }
    Err(Error::Runtime { context: "slirp api-socket", message: last })
}

fn reg_io(e: &std::io::Error) -> Error {
    Error::Runtime { context: "slirp hostfwd", message: e.to_string() }
}

/// Aplica um *ruleset* nftables via `nft -f -` (stdin).
fn apply_nft(ruleset: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Runtime {
            context: "spawn nft",
            message: e.to_string(),
        })?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(ruleset.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(Error::Runtime {
            context: "nft -f",
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// Uma regra DNAT (porta publicada): `host_port`/`proto` → `to` (ip:porta).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct DnatRule {
    pub proto: String,
    pub host_port: String,
    pub to: String,
}

/// Resumo da firewall do Delonix (tabela nft `delonix`) para o painel #10.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct FirewallSummary {
    /// Portas publicadas (DNAT host → container).
    pub dnat: Vec<DnatRule>,
    /// IPs de containers bloqueados (firewall por elemento).
    pub blocked: Vec<String>,
    /// Pares de bridges isoladas (forward drop) — `"a ✗ b"`.
    pub isolation: Vec<String>,
    /// Subnets com masquerade de saída.
    pub masquerade: Vec<String>,
}

/// Uma ligação de rede activa relevante para um container, do `conntrack`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Connection {
    /// `external_in` (alguém de fora → container), `egress` (container → fora),
    /// `internal` (container ↔ container).
    pub kind: String,
    /// Nome do container envolvido (o destino em `external_in`/`internal`-from;
    /// a origem em `egress`).
    pub container: String,
    /// O outro extremo: IP externo (`external_in`/`egress`) ou container (`internal`).
    pub peer: String,
    pub port: String,
    pub proto: String,
}

/// Lê as ligações ACTIVAS via `conntrack -L` (netlink) e classifica as que
/// envolvem containers (`ip2name`: IP do container → nome). É a base do monitor
/// de segurança do **engine** — só o host (netns global, root) vê isto; cada
/// container, no seu próprio netns e sem `CAP_NET_ADMIN`, vê apenas as suas
/// próprias ligações, nunca as de outro. Best-effort: sem `conntrack`, vazio.
pub fn list_connections(ip2name: &std::collections::HashMap<String, String>) -> Vec<Connection> {
    if ip2name.is_empty() {
        return vec![];
    }
    let text = match Command::new("conntrack").arg("-L").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => return vec![],
    };
    let is_cont = |ip: &str| ip2name.contains_key(ip);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let proto = line.split_whitespace().next().unwrap_or("tcp").to_string();
        let mut src = vec![];
        let mut dst = vec![];
        let mut dport = vec![];
        for tok in line.split_whitespace() {
            if let Some(v) = tok.strip_prefix("src=") {
                src.push(v);
            } else if let Some(v) = tok.strip_prefix("dst=") {
                dst.push(v);
            } else if let Some(v) = tok.strip_prefix("dport=") {
                dport.push(v);
            }
        }
        if src.len() < 2 || dst.is_empty() {
            continue;
        }
        let (o_src, o_dst, r_src) = (src[0], dst[0], src[1]);
        let port = dport.first().copied().unwrap_or("").to_string();
        if is_cont(r_src) && !is_cont(o_src) && o_src != "127.0.0.1" {
            let c = ip2name[r_src].clone();
            if seen.insert(format!("in:{o_src}:{c}:{port}")) {
                out.push(Connection { kind: "external_in".into(), container: c, peer: o_src.into(), port, proto });
            }
        } else if is_cont(o_src) && !is_cont(o_dst) && o_dst != "127.0.0.1" {
            let c = ip2name[o_src].clone();
            if seen.insert(format!("out:{c}:{o_dst}")) {
                out.push(Connection { kind: "egress".into(), container: c, peer: o_dst.into(), port, proto });
            }
        } else if is_cont(o_src) && is_cont(o_dst) {
            let (a, b) = (ip2name[o_src].clone(), ip2name[o_dst].clone());
            if seen.insert(format!("int:{a}:{b}")) {
                out.push(Connection { kind: "internal".into(), container: a, peer: b, port, proto });
            }
        }
    }
    out.truncate(200);
    out
}

#[cfg(test)]
mod tests {
    /// REGRESSÃO: o prefixo de QUALQUER nome de rede tem de cair no espaço de
    /// workload do ingress. Estava `100 + (fnv32 % 140)` e o ingress só aceita
    /// de 200 para cima — 71% dos nomes geravam uma rede onde publicar portas
    /// falhava ("IP ... fora do espaço de ingress"). Um teste sobre nomes reais
    /// e aleatórios apanha a divergência mal ela volte.
    #[test]
    fn prefixo_de_rede_cai_sempre_no_espaco_de_ingress() {
        use delonix_runtime_core::workload_net::is_workload_ipv4;
        let mut nomes: Vec<String> = vec![
            "kind".into(), "dlx-delonix".into(), "dlx-delonix-01".into(), "backend".into(),
            "lab-net".into(), "a".into(), "".into(), "rede-com-nome-muito-comprido-mesmo".into(),
        ];
        // Cobertura a sério: 500 nomes gerados, não só os que me lembrei.
        nomes.extend((0..500).map(|i| format!("net-{i}")));
        for n in &nomes {
            let base = Network::base_for(n);
            let ip: std::net::Ipv4Addr = format!("10.{base}.1.2").parse().unwrap();
            assert!(
                is_workload_ipv4(ip),
                "a rede '{n}' ficou em 10.{base}.x — fora do espaço de ingress; o `-p` falharia lá"
            );
        }
    }

    use super::*;

    #[test]
    fn overlay_peer_parse() {
        // VXLAN plano (só node_ip)
        assert_eq!(parse_overlay_peer("10.0.0.2"), ("10.0.0.2".into(), None));
        // cifrado: node_ip=pubkey=wg_ip
        let (ip, wg) = parse_overlay_peer("10.0.0.2=AbCdEf0123/+key=10.250.0.2");
        assert_eq!(ip, "10.0.0.2");
        assert_eq!(wg, Some(("AbCdEf0123/+key".into(), "10.250.0.2".into())));
        // REGRESSÃO: pubkey WireGuard REAL (base64 44c) TERMINA em `=` (padding) —
        // o delimitador colide. O parser tem de preservar o padding e o wg_ip limpo.
        let real = "VpKM6MYFVDIvcMBxnkBkf7/clXq+itJlPaW71o2iK24=";
        let (ip2, wg2) = parse_overlay_peer(&format!("127.0.0.1={real}=10.250.0.1"));
        assert_eq!(ip2, "127.0.0.1");
        assert_eq!(wg2, Some((real.to_string(), "10.250.0.1".into())));
        // malformado → trata como plano (sem wg)
        assert_eq!(parse_overlay_peer("10.0.0.2=").0, "10.0.0.2");
        assert!(parse_overlay_peer("10.0.0.2=").1.is_none());
    }

    #[test]
    fn overlay_add_peer_dedup() {
        let dir = std::env::temp_dir().join(format!("dlx-addpeer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = NetworkStore::open(&dir).unwrap();
        store.create_overlay("ov", 7, &[], Some("10.250.0.1")).unwrap();
        // aprende um peer
        let n = store.add_overlay_peer("ov", "10.0.0.2=PUB2=10.250.0.2").unwrap();
        assert_eq!(n.peers, vec!["10.0.0.2=PUB2=10.250.0.2"]);
        assert_eq!(n.wg_ip.as_deref(), Some("10.250.0.1")); // preserva wgip
        // rotação: mesmo node_ip, chave nova → SUBSTITUI (não duplica)
        let n2 = store.add_overlay_peer("ov", "10.0.0.2=PUBNEW=10.250.0.2").unwrap();
        assert_eq!(n2.peers, vec!["10.0.0.2=PUBNEW=10.250.0.2"]);
        // 2º peer distinto → adiciona
        let n3 = store.add_overlay_peer("ov", "10.0.0.3=PUB3=10.250.0.3").unwrap();
        assert_eq!(n3.peers.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_wgip_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dlx-wgo-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = NetworkStore::open(&dir).unwrap();
        let peers = vec!["10.0.0.2=PUB2=10.250.0.2".to_string()];
        let n = store.create_overlay("ov", 42, &peers, Some("10.250.0.1")).unwrap();
        assert_eq!(n.wg_ip.as_deref(), Some("10.250.0.1"));
        // recarrega do disco → wg_ip persiste
        let n2 = store.get("ov").unwrap();
        assert_eq!(n2.wg_ip.as_deref(), Some("10.250.0.1"));
        assert_eq!(n2.peers, peers);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ip_is_deterministic_and_avoids_reserved() {
        // prefixo fixo (a default é auto-detectada/persistida em runtime). O IP
        // DERIVADO (hash puro) é estável e evita reservados — é só o ponto de
        // partida; a unicidade real vem do lease + sondagem (ver `ipam`).
        let ip = derive_ip_in("10.88", "0000000a00000000");
        assert!(ip.starts_with("10.88."));
        // ids que partilham os 8 primeiros hex DERIVAM o mesmo IP — era a raiz da
        // colisão. O `ipam::allocate` é que os separa (testado em `ipam::tests`).
        assert_eq!(derive_ip_in("10.88", "deadbeef1234"), derive_ip_in("10.88", "deadbeef9999"));
        // o último octeto nunca é 0/1/255
        for id in ["00000000", "00000001", "000000ff"] {
            let last: u8 =
                derive_ip_in("10.88", id).rsplit('.').next().unwrap().parse().unwrap();
            assert!(last >= 2 && last != 255, "id {id} -> {last}");
        }
    }

    #[test]
    fn valid_ip_in_subnet_aceita_e_rejeita() {
        // dentro da subnet, unicast utilizável
        assert!(valid_ip_in_subnet("10.88", "10.88.0.77"));
        assert!(valid_ip_in_subnet("10.88", "10.88.255.254"));
        assert!(valid_ip_in_subnet("10.204", "10.204.19.189"));
        // fora da subnet (prefixo errado)
        assert!(!valid_ip_in_subnet("10.88", "10.9.0.5"));
        assert!(!valid_ip_in_subnet("10.88", "192.168.0.5"));
        // reservados: rede, gateway, broadcast
        assert!(!valid_ip_in_subnet("10.88", "10.88.0.0"));
        assert!(!valid_ip_in_subnet("10.88", "10.88.0.1"));
        assert!(!valid_ip_in_subnet("10.88", "10.88.255.255"));
        // malformados
        assert!(!valid_ip_in_subnet("10.88", "10.88.0"));
        assert!(!valid_ip_in_subnet("10.88", "10.88.0.300"));
        assert!(!valid_ip_in_subnet("10.88", "10.88.0.x"));
    }

    #[test]
    fn default_base_evita_docker_e_podman() {
        // a base por omissão nunca pode ser 88 (Podman) nem cair em 172/16 (Docker
        // não é 10.x). pick_free_base escolhe sempre fora dos octetos usados.
        let used = used_10_octets();
        assert!(used.contains(&88), "Podman (10.88) tem de estar marcado como usado");
        assert!(used.contains(&90), "VIPs (10.90) reservado");
        let base = pick_free_base();
        assert!(!used.contains(&base), "a base escolhida ({base}) colide com algo já usado");
        assert!(base != 88 && base != 90);
    }

    #[test]
    fn user_network_is_isolated_subnet() {
        let base = Network::base_for("frontend");
        assert!((100..=239).contains(&base), "base {base} fora do intervalo");
        let n = Network::user_with_base("frontend", base);
        assert_eq!(n.subnet, format!("10.{base}.0.0/16"));
        assert_eq!(n.gateway, format!("10.{base}.0.1"));
        assert!(n.bridge.starts_with("dlxn") && n.bridge.len() <= 15);
        // fora da subnet por omissão (88) e dos VIPs (90).
        assert_ne!(base, 88);
        assert_ne!(base, 90);
        // IP de container cai na subnet da rede.
        assert!(alloc_ip_in(&n.prefix, "deadbeef").starts_with(&format!("10.{base}.")));
    }

    #[test]
    fn net_rate_spec_parsing() {
        // caudais: sufixos decimais (k/m/g), com ou sem `bit`/`bps`.
        assert_eq!(parse_rate_bits("1000000").unwrap(), 1_000_000);
        assert_eq!(parse_rate_bits("10mbit").unwrap(), 10_000_000);
        assert_eq!(parse_rate_bits("512k").unwrap(), 512_000);
        assert_eq!(parse_rate_bits("1G").unwrap(), 1_000_000_000);
        assert_eq!(parse_rate_bits("100mbps").unwrap(), 100_000_000);
        // inválidos / não-positivos.
        assert!(parse_rate_bits("").is_err());
        assert!(parse_rate_bits("abc").is_err());
        assert!(parse_rate_bits("0").is_err());
        assert!(parse_rate_bits("-5m").is_err());

        // burst: sufixos binários (k=1024), `b` final opcional.
        assert_eq!(parse_size_bytes("4096"), Some(4096));
        assert_eq!(parse_size_bytes("256k"), Some(256 * 1024));
        assert_eq!(parse_size_bytes("1mb"), Some(1024 * 1024));
        assert_eq!(parse_size_bytes("xyz"), None);

        // burst por omissão = ~100 ms de caudal, com piso de 16 KiB.
        let r = parse_net_rate("10mbit", None).unwrap();
        assert_eq!(r.rate_bit, 10_000_000);
        assert_eq!(r.burst_bytes, 10_000_000 / 8 / 10); // 125_000 bytes
        let small = parse_net_rate("100k", None).unwrap();
        assert_eq!(small.burst_bytes, 16 * 1024); // piso aplicado

        // burst explícito é respeitado; o formato `tc` é o esperado.
        let r = parse_net_rate("1mbit", Some("32k")).unwrap();
        assert_eq!(r, NetRate { rate_bit: 1_000_000, burst_bytes: 32 * 1024 });
        assert_eq!(r.tc_rate(), "1000000bit");
        assert_eq!(r.tc_burst(), "32768");
        assert!(parse_net_rate("1mbit", Some("0")).is_err());
        assert!(parse_net_rate("1mbit", Some("bad")).is_err());
    }

    #[test]
    fn network_store_create_get_list_remove() {
        let tmp = std::env::temp_dir().join(format!("dlxnet-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let s = NetworkStore::open(&tmp).unwrap();
        assert!(s.get("bridge").unwrap().name == DEFAULT_NET);
        assert!(s.get("nope").is_err());
        let a = s.create("alpha").unwrap();
        let b = s.create("beta").unwrap();
        assert_ne!(a.subnet, b.subnet, "redes distintas têm subnets distintas");
        assert_eq!(s.list().unwrap().len(), 2);
        assert!(s.create("alpha").is_err(), "duplicado deve falhar");
        assert!(s.create("bridge").is_err(), "nome reservado deve falhar");
        s.remove("alpha").unwrap();
        assert_eq!(s.list().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

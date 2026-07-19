//! `delonix network` — ls/create/rm/inspect.
//!
//! **Nota (dois stores em paralelo, deliberado, não um bug):** `NetworkStore`
//! (`delonix_net::NetworkStore`) é o registo declarativo "rico" (drivers
//! bridge/macvlan/ipvlan/overlay, VNI, peers WireGuard), persistido em
//! `<root>/networks/<nome>`. `infra::{network_create_with,network_remove}`
//! (`delonix_net::infra`) é o plano FÍSICO ligado ao holder netns rootless
//! (bridge real + prefixo), persistido separadamente em
//! `<ingress_dir>/networks/<nome>.json` — é o que `container run --net <nome>`
//! e `vm create --network <nome>` realmente usam para atachar. Para o driver
//! `bridge` (o único que os containers atacham hoje via `infra::
//! attach_container`), `network create` orquestra os dois EM CONJUNTO, com o
//! `NetworkStore` como fonte da verdade do prefixo (`infra::network_create_with`
//! existe precisamente para alinhar os dois — ver o comentário lá). O driver
//! `overlay` TAMBÉM orquestra os dois: além do registo, sobe o plano físico no
//! holder (bridge + uplink VXLAN + WireGuard se cifrado — ver `realize_overlay`),
//! porque é realizável sem privilégio de host. Já `macvlan`/`ipvlan` só ficam no
//! `NetworkStore`: o plano físico deles precisa de CAP_NET_ADMIN na init-netns do
//! host, que o modelo rootless não tem — o `create` regista mas AVISA alto que a
//! rede não foi realizada (Realized=False), em vez de fingir sucesso.

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_net::{infra, Network, NetworkStore};
use delonix_runtime_core::Result;
use serde::Deserialize;
use std::path::PathBuf;

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::state_root;

/// `spec` de `kind: Network` — espelha os campos de `NetworkCmd::Create`.
#[derive(Debug, Deserialize)]
struct NetworkSpec {
    #[serde(default = "default_driver")]
    driver: String,
    parent: Option<String>,
    subnet: Option<String>,
    #[serde(default)]
    gateway: String,
    vni: Option<u32>,
    #[serde(default)]
    peers: Vec<String>,
    /// Canónico `wgIp` (camelCase, uniforme com o resto do schema); `wg_ip`
    /// continua aceite (retrocompat).
    #[serde(rename = "wgIp", alias = "wg_ip")]
    wg_ip: Option<String>,
}

fn default_driver() -> String {
    "bridge".to_string()
}

/// Nomes aceites no `spec` de `kind: Network` (canónicos + aliases), para o
/// aviso de campos desconhecidos.
pub(crate) const NETWORK_SPEC_FIELDS: &[&str] = &[
    "driver", "parent", "subnet", "gateway", "vni", "peers", "wgIp", "wg_ip",
];

#[derive(Subcommand)]
pub enum NetworkCmd {
    /// Dashboard (KPIs + tabela) das redes — TUI interactivo, ou `--once` snapshot.
    Dash {
        #[arg(long)]
        once: bool,
    },
    /// Lista as redes.
    Ls,
    /// Identidade WireGuard DESTE nó, para o overlay VXLAN cifrado entre nós
    /// (`network create --driver overlay`). A chave privada fica 0600 em
    /// `<root>/wg/node.key`; a pública é o que se distribui aos peers.
    Node {
        #[command(subcommand)]
        action: NodeCmd,
    },
    /// Cria uma rede.
    Create {
        name: String,
        /// `bridge` (default, filtrada pelo firewall) | `macvlan` | `ipvlan` (NÃO
        /// filtrados, ver aviso) | `overlay` (VXLAN inter-nó).
        #[arg(long, default_value = "bridge")]
        driver: String,
        /// NIC-pai do host (obrigatório p/ macvlan/ipvlan).
        #[arg(long)]
        parent: Option<String>,
        /// Subnet (obrigatório p/ macvlan/ipvlan, ex.: `192.168.1.0/24`).
        #[arg(long)]
        subnet: Option<String>,
        /// Gateway (macvlan/ipvlan).
        #[arg(long, default_value = "")]
        gateway: String,
        /// VXLAN Network Identifier (obrigatório p/ overlay).
        #[arg(long)]
        vni: Option<u32>,
        /// Nó-par (`<ip>` ou `<ip>=<wg_pubkey>=<wg_ip>`), repetível (overlay).
        #[arg(long = "peer")]
        peers: Vec<String>,
        /// IP de túnel WireGuard deste nó (overlay cifrado).
        #[arg(long)]
        wg_ip: Option<String>,
    },
    /// Detalhe de uma rede.
    Inspect {
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        name: String,
    },
    /// Detalhe legível de uma ou mais redes, ao estilo `kubectl describe`
    /// (para humanos; use `inspect` para a vista compacta de sempre).
    Describe {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Remove uma rede.
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        name: String,
    },
    /// Aplica os documentos `kind: Network` de um manifesto (idempotente por nome).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

pub fn run(action: NetworkCmd) -> Result<()> {
    let store = NetworkStore::open(state_root())?;
    match action {
        NetworkCmd::Dash { once } => super::dash::run(super::dash::DashScope::Networks, once),
        NetworkCmd::Ls => cmd_ls(&store),
        NetworkCmd::Node { action } => cmd_node(action),
        NetworkCmd::Create {
            name,
            driver,
            parent,
            subnet,
            gateway,
            vni,
            peers,
            wg_ip,
        } => {
            let net = create_network(
                &store, &name, &driver, parent, subnet, &gateway, vni, peers, wg_ip,
            )?;
            println!("{}", net.name);
            Ok(())
        }
        NetworkCmd::Inspect { name } => cmd_inspect(&store, &name),
        NetworkCmd::Describe { names } => cmd_describe(&store, &names),
        NetworkCmd::Rm { name } => cmd_rm(&store, &name),
        NetworkCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
    }
}

/// Aplica os documentos `kind: Network` (chamado por `network apply` e por
/// `stack apply`, que já tem os documentos carregados de antemão).
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let store = NetworkStore::open(state_root())?;
    for doc in manifest::of_kind(docs, "Network") {
        let name = &doc.metadata.name;
        // Avisa de gralhas ANTES do early-continue (ver container::apply): um
        // re-apply contra uma rede já existente também deve ver o aviso.
        manifest::warn_unknown_fields(doc, NETWORK_SPEC_FIELDS);
        if store.get(name).is_ok() {
            println!("network/{name}: já existe, nada a fazer");
            continue;
        }
        let spec: NetworkSpec = manifest::spec_of(doc)?;
        create_network(
            &store,
            name,
            &spec.driver,
            spec.parent,
            spec.subnet,
            &spec.gateway,
            spec.vni,
            spec.peers,
            spec.wg_ip,
        )?;
        println!("network/{name}: criada");
    }
    Ok(())
}

fn cmd_ls(store: &NetworkStore) -> Result<()> {
    let mut t = output::Table::new(&["NAME", "DRIVER", "BRIDGE", "SUBNET"]);
    for n in store.list()? {
        t.row(vec![n.name, n.driver, n.bridge, n.subnet]);
    }
    t.print();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// Cria uma rede nos DOIS stores coordenados (registo declarativo + plano
/// físico do holder, com o MESMO prefixo). É `pub(crate)` para o modo kind
/// poder criar a rede do cluster — usar só `infra::network_create` deixaria o
/// `NetworkStore` sem registo e o `run --net <x>` recusaria com
/// "no such container: network <x>".
pub(crate) fn create_network(
    store: &NetworkStore,
    name: &str,
    driver: &str,
    parent: Option<String>,
    subnet: Option<String>,
    gateway: &str,
    vni: Option<u32>,
    peers: Vec<String>,
    wg_ip: Option<String>,
) -> Result<Network> {
    match driver {
        "bridge" => {
            let net = store.create(name)?;
            // Realiza fisicamente (bridge real do holder rootless) — alinhada
            // ao MESMO prefixo que o NetworkStore acabou de decidir.
            infra::network_create_with(name, &net.prefix)?;
            Ok(net)
        }
        "macvlan" | "ipvlan" => {
            let parent = parent.ok_or_else(|| {
                delonix_runtime_core::Error::Invalid(format!(
                    "--parent é obrigatório para driver {driver}"
                ))
            })?;
            let subnet = subnet.ok_or_else(|| {
                delonix_runtime_core::Error::Invalid(format!(
                    "--subnet é obrigatório para driver {driver}"
                ))
            })?;
            let net = store.create_lan(name, driver, &parent, &subnet, gateway)?;
            // HONESTIDADE (não um no-op silencioso): macvlan/ipvlan põem o container
            // DIRECTAMENTE na LAN física do `parent` — isso exige criar a
            // sub-interface na init-netns do host com CAP_NET_ADMIN, privilégio que
            // uma sessão rootless (o modelo por omissão deste motor) não tem. O
            // registo declarativo fica gravado (intenção preservada p/ um host
            // privilegiado), mas o plano físico NÃO é realizado — dizê-lo alto.
            eprintln!(
                "aviso: rede '{name}' (driver {driver}) registada mas NÃO realizada — \
                 condition Realized=False reason=DriverNotImplemented. macvlan/ipvlan \
                 precisam de privilégio na init-netns do host (CAP_NET_ADMIN), que o \
                 modelo rootless não tem; containers NÃO conseguirão atachar-se a ela. \
                 Para rede multi-nó rootless use driver 'overlay'."
            );
            Ok(net)
        }
        "overlay" => {
            let vni = vni.ok_or_else(|| {
                delonix_runtime_core::Error::Invalid(
                    "--vni é obrigatório para driver overlay".into(),
                )
            })?;
            let net = store.create_overlay(name, vni, &peers, wg_ip.as_deref())?;
            // Plano físico rootless (holder netns): bridge + uplink VXLAN + WG (se
            // cifrado). Ao contrário de macvlan/ipvlan, o overlay É realizável sem
            // privilégio de host — vive todo no netns do holder.
            if let Err(e) = realize_overlay(&net) {
                eprintln!(
                    "aviso: rede overlay '{name}' registada mas o uplink físico não \
                     subiu ({e}) — condition Realized=False. Reconcilia no próximo \
                     'network create' quando o holder/pares estiverem disponíveis."
                );
            }
            Ok(net)
        }
        other => Err(delonix_runtime_core::Error::Invalid(format!(
            "driver desconhecido: '{other}' (use bridge|macvlan|ipvlan|overlay)"
        ))),
    }
}

/// **Realiza o plano físico de uma rede overlay** no holder netns rootless:
/// (1) bridge do holder alinhada ao prefixo que o `NetworkStore` decidiu;
/// (2) uplink VXLAN (`dlxvx<vni>`) a masterizar essa bridge + FDB dos pares;
/// (3) WireGuard, SE o overlay é cifrado (`wg_ip` presente) — cifra o transporte
///     VXLAN entre nós (o FDB passa a apontar para os `wg_ip` em vez dos `node_ip`).
///
/// Espelha `delonix_net::Net::ensure_vxlan`/`ensure_overlay_wg` (o caminho antigo
/// root/host-netns), mas conduzido pelo control-socket do holder — o único com
/// CAP_NET_ADMIN no netns de infra. Idempotente. Requer o holder de pé
/// (`ensure_up`). Só faz sentido chamar quando `net.driver == "overlay"`.
fn realize_overlay(net: &Network) -> Result<()> {
    const WG_PORT: u16 = 51820;
    let Some(vni) = net.vni else { return Ok(()) };
    let Some(dev) = net.vxlan_dev() else {
        return Ok(());
    };
    // Overlay CIFRADO (wg_ip deste nó presente) EXIGE o `wg` no host. Falha ANTES
    // de subir o VXLAN: senão o FDB apontaria para os wg_ip dos pares (só
    // alcançáveis pelo túnel) sem túnel nenhum a subir → uplink silenciosamente
    // blackholed. Erro acionável em vez de um overlay que finge estar de pé.
    let encrypted = net.wg_ip.is_some();
    if encrypted && !delonix_net::wg::available() {
        return Err(delonix_runtime_core::Error::Invalid(
            "overlay cifrado (wg_ip) mas 'wg' indisponível no host — instala \
             wireguard-tools + o módulo do kernel, ou remove wg_ip para transporte \
             VXLAN plano (não cifrado)"
                .into(),
        ));
    }
    // Parse dos peers UMA vez (reusado no FDB e no loop WG).
    let parsed: Vec<(String, Option<(String, String)>)> = net
        .peers
        .iter()
        .map(|p| delonix_net::parse_overlay_peer(p))
        .collect();
    // Holder de pé (sem incrementar o ref-count — o uplink é infra persistente,
    // não uma carga; morre com o `network rm` → `netdel`, não com um release).
    infra::ensure_up()?;
    // A bridge/gateway vêm do plano físico alinhado ao prefixo do NetworkStore.
    infra::network_create_with(&net.name, &net.prefix)?;
    let (bridge, _prefix, gateway) = infra::resolve_net(&net.name)?;
    // FDB: `wg_ip` de cada par se cifrado, senão o `node_ip` plano.
    let dsts: Vec<String> = parsed
        .iter()
        .map(|(node_ip, wg)| {
            wg.as_ref()
                .map(|(_pubkey, wgip)| wgip.clone())
                .unwrap_or_else(|| node_ip.clone())
        })
        .collect();
    infra::set_vxlan(&dev, vni, &bridge, &gateway, &dsts)?;
    // WireGuard só no overlay CIFRADO (a disponibilidade já foi garantida acima).
    if let Some(my_wg_ip) = net.wg_ip.as_deref() {
        let key = delonix_net::wg::ensure_node_key()?;
        let iface = format!("wgo{vni:06x}"); // <= 15 chars
        infra::set_wg_iface(&iface, &key.private, WG_PORT, &format!("{my_wg_ip}/24"))?;
        for (node_ip, wg) in &parsed {
            if let Some((pubkey, wgip)) = wg {
                infra::set_wg_peer(
                    &iface,
                    pubkey,
                    &format!("{node_ip}:{WG_PORT}"),
                    &[format!("{wgip}/32")],
                )?;
            }
        }
    }
    Ok(())
}

fn cmd_inspect(store: &NetworkStore, name: &str) -> Result<()> {
    let n = store.get(name)?;
    println!("nome:     {}", n.name);
    println!("driver:   {}", n.driver);
    println!("bridge:   {}", n.bridge);
    println!("subnet:   {}", n.subnet);
    println!("gateway:  {}", n.gateway);
    if let Some(p) = &n.parent {
        println!("parent:   {p}");
    }
    if let Some(vni) = n.vni {
        println!("vni:      {vni}");
    }
    if !n.peers.is_empty() {
        println!("peers:    {}", n.peers.join(", "));
    }
    Ok(())
}

/// `network describe` — detalhe legível ao estilo `kubectl describe`.
/// Complementa o `inspect` (vista compacta de sempre, estável para scripts).
fn cmd_describe(store: &NetworkStore, names: &[String]) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        let n = store.get(name)?;
        if i > 0 {
            println!();
        }
        describe_one(&n);
    }
    Ok(())
}

/// Containers ligados a esta rede, lidos do `Store` — `network` (a rede
/// primária do `run --net`) ou `extra_networks` (as ligadas depois).
///
/// Best-effort de propósito: um erro a abrir/ler o store dá `None`, e o
/// `describe` omite a secção em vez de afirmar "<none>". A distinção importa —
/// "não há containers ligados" e "não consegui saber" não são a mesma coisa
/// numa vista que se usa para decidir se uma rede pode ser removida.
fn attached_containers(net: &str) -> Option<Vec<String>> {
    let store = delonix_runtime_core::Store::open(state_root().join("containers")).ok()?;
    let cs = store.list().ok()?;
    Some(
        cs.iter()
            .filter(|c| {
                c.network.as_deref() == Some(net)
                    || c.extra_networks.iter().any(|e| e.network == net)
            })
            .map(|c| {
                // O IP da rede em causa, seja ela a primária ou uma extra.
                let ip = if c.network.as_deref() == Some(net) {
                    c.ip.clone()
                } else {
                    c.extra_networks
                        .iter()
                        .find(|e| e.network == net)
                        .map(|e| e.ip.clone())
                };
                format!(
                    "{} ({}) {}",
                    c.name,
                    super::container::short_id(&c.id),
                    ip.unwrap_or_else(|| "<no ip>".into())
                )
            })
            .collect(),
    )
}

fn describe_one(n: &Network) {
    let mut d = output::Describe::new();
    d.field("Name", &n.name);
    d.field("Driver", &n.driver);
    d.field(
        "Bridge",
        if n.bridge.is_empty() {
            "<none>"
        } else {
            &n.bridge
        },
    );
    d.field("Subnet", &n.subnet);
    d.field(
        "Gateway",
        if n.gateway.is_empty() {
            "<none>"
        } else {
            &n.gateway
        },
    );
    d.field("Prefix", &n.prefix);
    // Só nos drivers de LAN física (macvlan/ipvlan).
    d.field_opt("Parent", n.parent.as_deref());
    // Só no driver overlay.
    d.field_opt("VNI", n.vni.map(|v| v.to_string()));
    d.field_opt("WireGuard IP", n.wg_ip.as_deref());
    if !n.peers.is_empty() {
        d.list("Peers", &n.peers);
    }
    match attached_containers(&n.name) {
        Some(cs) => {
            d.list("Containers", &cs);
        }
        None => {
            d.field(
                "Containers",
                "<unknown> (não consegui ler o store de containers)",
            );
        }
    }
    d.print();
}

fn cmd_rm(store: &NetworkStore, name: &str) -> Result<()> {
    store.remove(name)?;
    infra::network_remove(name);
    println!("{name}");
    Ok(())
}

/// Subcomandos de `network node` — a identidade WireGuard do nó local.
#[derive(clap::Subcommand)]
pub enum NodeCmd {
    /// Gera a chave do nó (se ainda não existir) e imprime a pública com o
    /// contexto de o que fazer com ela. Idempotente.
    Init,
    /// Imprime só a chave pública (para compor em scripts).
    Key,
}

/// `network node` — `ensure_node_key` é idempotente: gera na primeira vez,
/// depois lê a que já existe.
fn cmd_node(action: NodeCmd) -> Result<()> {
    let key = delonix_net::wg::ensure_node_key()?;
    match action {
        NodeCmd::Init => {
            println!("nó inicializado — chave pública (distribui aos peers do overlay):");
            println!("  {}", key.public);
            println!("privada protegida 0600 em <root>/wg/node.key");
        }
        // Só a chave, sem ruído: isto costuma ir para dentro doutro comando.
        NodeCmd::Key => println!("{}", key.public),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::NetworkSpec;

    #[test]
    fn networkspec_aceita_wg_ip_legado_e_wgip_canonico() {
        let legado: NetworkSpec =
            serde_yaml::from_str("driver: overlay\nwg_ip: 10.9.0.1\n").unwrap();
        assert_eq!(legado.wg_ip.as_deref(), Some("10.9.0.1"));
        let canon: NetworkSpec = serde_yaml::from_str("driver: overlay\nwgIp: 10.9.0.1\n").unwrap();
        assert_eq!(canon.wg_ip.as_deref(), Some("10.9.0.1"));
    }
}

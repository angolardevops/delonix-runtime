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
//! existe precisamente para alinhar os dois — ver o comentário lá). Os drivers
//! `macvlan`/`ipvlan`/`overlay` só ficam no `NetworkStore` (não passam pela
//! bridge do holder da mesma forma) — limitação conhecida, não bloqueante.

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
    wg_ip: Option<String>,
}

fn default_driver() -> String {
    "bridge".to_string()
}

#[derive(Subcommand)]
pub enum NetworkCmd {
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
        NetworkCmd::Ls => cmd_ls(&store),
        NetworkCmd::Node { action } => cmd_node(action),
        NetworkCmd::Create { name, driver, parent, subnet, gateway, vni, peers, wg_ip } => {
            let net = create_network(&store, &name, &driver, parent, subnet, &gateway, vni, peers, wg_ip)?;
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
        if store.get(name).is_ok() {
            println!("network/{name}: já existe, nada a fazer");
            continue;
        }
        let spec: NetworkSpec = manifest::spec_of(doc)?;
        create_network(&store, name, &spec.driver, spec.parent, spec.subnet, &spec.gateway, spec.vni, spec.peers, spec.wg_ip)?;
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
                delonix_runtime_core::Error::Invalid(format!("--parent é obrigatório para driver {driver}"))
            })?;
            let subnet = subnet.ok_or_else(|| {
                delonix_runtime_core::Error::Invalid(format!("--subnet é obrigatório para driver {driver}"))
            })?;
            store.create_lan(name, driver, &parent, &subnet, gateway)
        }
        "overlay" => {
            let vni = vni.ok_or_else(|| delonix_runtime_core::Error::Invalid("--vni é obrigatório para driver overlay".into()))?;
            store.create_overlay(name, vni, &peers, wg_ip.as_deref())
        }
        other => Err(delonix_runtime_core::Error::Invalid(format!(
            "driver desconhecido: '{other}' (use bridge|macvlan|ipvlan|overlay)"
        ))),
    }
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
            .filter(|c| c.network.as_deref() == Some(net) || c.extra_networks.iter().any(|e| e.network == net))
            .map(|c| {
                // O IP da rede em causa, seja ela a primária ou uma extra.
                let ip = if c.network.as_deref() == Some(net) {
                    c.ip.clone()
                } else {
                    c.extra_networks.iter().find(|e| e.network == net).map(|e| e.ip.clone())
                };
                format!("{} ({}) {}", c.name, super::container::short_id(&c.id), ip.unwrap_or_else(|| "<no ip>".into()))
            })
            .collect(),
    )
}

fn describe_one(n: &Network) {
    let mut d = output::Describe::new();
    d.field("Name", &n.name);
    d.field("Driver", &n.driver);
    d.field("Bridge", if n.bridge.is_empty() { "<none>" } else { &n.bridge });
    d.field("Subnet", &n.subnet);
    d.field("Gateway", if n.gateway.is_empty() { "<none>" } else { &n.gateway });
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
            d.field("Containers", "<unknown> (não consegui ler o store de containers)");
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

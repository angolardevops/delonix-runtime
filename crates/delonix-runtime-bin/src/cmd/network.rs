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
use delonix_net::{infra, Network, NetworkStore};
use delonix_runtime_core::Result;
use serde::Deserialize;
use std::path::PathBuf;

use super::manifest::{self, ManifestDoc};
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
    Inspect { name: String },
    /// Remove uma rede.
    Rm { name: String },
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
        NetworkCmd::Create { name, driver, parent, subnet, gateway, vni, peers, wg_ip } => {
            let net = create_network(&store, &name, &driver, parent, subnet, &gateway, vni, peers, wg_ip)?;
            println!("{}", net.name);
            Ok(())
        }
        NetworkCmd::Inspect { name } => cmd_inspect(&store, &name),
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
    println!("{:<20}  {:<8}  {:<10}  SUBNET", "NOME", "DRIVER", "BRIDGE");
    for n in store.list()? {
        println!("{:<20}  {:<8}  {:<10}  {}", n.name, n.driver, n.bridge, n.subnet);
    }
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

fn cmd_rm(store: &NetworkStore, name: &str) -> Result<()> {
    store.remove(name)?;
    infra::network_remove(name);
    println!("{name}");
    Ok(())
}

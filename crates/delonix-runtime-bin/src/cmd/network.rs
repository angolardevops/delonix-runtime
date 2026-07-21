//! `delonix network` — ls/create/rm/inspect.
//!
//! **Note (two stores in parallel, deliberate, not a bug):** `NetworkStore`
//! (`delonix_net::NetworkStore`) is the "rich" declarative registry (drivers
//! bridge/macvlan/ipvlan/overlay, VNI, WireGuard peers), persisted in
//! `<root>/networks/<name>`. `infra::{network_create_with,network_remove}`
//! (`delonix_net::infra`) is the PHYSICAL plane tied to the rootless holder netns
//! (real bridge + prefix), persisted separately in
//! `<ingress_dir>/networks/<name>.json` — it is what `container run --net <name>`
//! and `vm create --network <name>` actually use to attach. For the `bridge`
//! driver (the only one containers attach to today via `infra::
//! attach_container`), `network create` orchestrates both TOGETHER, with the
//! `NetworkStore` as the source of truth for the prefix (`infra::network_create_with`
//! exists precisely to align the two — see the comment there). The `overlay`
//! driver ALSO orchestrates both: besides the registry, it brings up the physical
//! plane in the holder (bridge + VXLAN uplink + WireGuard if encrypted — see
//! `realize_overlay`), because it is realizable without host privilege. Whereas
//! `macvlan`/`ipvlan` only stay in the `NetworkStore`: their physical plane needs
//! CAP_NET_ADMIN in the host init-netns, which the rootless model does not have —
//! `create` registers but WARNS loudly that the network was not realized
//! (Realized=False), instead of faking success.

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_net::{infra, Network, NetworkStore};
use delonix_runtime_core::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::state_root;

/// `spec` for `kind: Network` — mirrors the fields of `NetworkCmd::Create`.
#[derive(Debug, Deserialize, Serialize)]
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
    /// Canonical `wgIp` (camelCase, uniform with the rest of the schema); `wg_ip`
    /// is still accepted (backward compat).
    #[serde(rename = "wgIp", alias = "wg_ip")]
    wg_ip: Option<String>,
}

fn default_driver() -> String {
    "bridge".to_string()
}

/// Names accepted in the `spec` of `kind: Network` (canonical + aliases), for the
/// unknown-fields warning.
pub(crate) const NETWORK_SPEC_FIELDS: &[&str] = &[
    "driver", "parent", "subnet", "gateway", "vni", "peers", "wgIp", "wg_ip",
];

#[derive(Subcommand)]
pub enum NetworkCmd {
    /// Dashboard (KPIs + table) of the networks — interactive TUI, or `--once` snapshot.
    Dash {
        #[arg(long)]
        once: bool,
    },
    /// List the networks.
    Ls,
    /// WireGuard identity of THIS node, for the encrypted VXLAN overlay between nodes
    /// (`network create --driver overlay`). The private key stays 0600 in
    /// `<root>/wg/node.key`; the public one is what you hand out to the peers.
    Node {
        #[command(subcommand)]
        action: NodeCmd,
    },
    /// Create a network.
    Create {
        name: String,
        /// `bridge` (default, filtered by the firewall) | `macvlan` | `ipvlan` (NOT
        /// filtered, see warning) | `overlay` (inter-node VXLAN).
        #[arg(long, default_value = "bridge")]
        driver: String,
        /// Host parent NIC (required for macvlan/ipvlan).
        #[arg(long)]
        parent: Option<String>,
        /// Subnet (required for macvlan/ipvlan, e.g.: `192.168.1.0/24`).
        #[arg(long)]
        subnet: Option<String>,
        /// Gateway (macvlan/ipvlan).
        #[arg(long, default_value = "")]
        gateway: String,
        /// VXLAN Network Identifier (required for overlay).
        #[arg(long)]
        vni: Option<u32>,
        /// Peer node (`<ip>` or `<ip>=<wg_pubkey>=<wg_ip>`), repeatable (overlay).
        #[arg(long = "peer")]
        peers: Vec<String>,
        /// WireGuard tunnel IP of this node (encrypted overlay).
        #[arg(long)]
        wg_ip: Option<String>,
    },
    /// Detail of a network.
    Inspect {
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        name: String,
    },
    /// Readable detail of one or more networks, `kubectl describe` style
    /// (for humans; use `inspect` for the usual compact view).
    Describe {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Remove a network.
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        name: String,
    },
    /// Apply the `kind: Network` documents of a manifest (idempotent by name).
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

/// Apply the `kind: Network` documents (called by `network apply` and by
/// `stack apply`, which already has the documents loaded beforehand).
/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: NetworkSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec)
        .map_err(|e| delonix_runtime_core::Error::Invalid(format!("dry-run: {e}")))
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let store = NetworkStore::open(state_root())?;
    for doc in manifest::of_kind(docs, "Network") {
        let name = &doc.metadata.name;
        // Warn about typos BEFORE the early-continue (see container::apply): a
        // re-apply against an already existing network must also see the warning.
        manifest::warn_unknown_fields(doc, NETWORK_SPEC_FIELDS);
        if store.get(name).is_ok() {
            println!(
                "network/{name}: {}",
                super::po::t("already exists, nothing to do")
            );
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
/// Create a network in BOTH coordinated stores (declarative registry + the
/// holder's physical plane, with the SAME prefix). It is `pub(crate)` so the kind
/// mode can create the cluster network — using only `infra::network_create` would
/// leave the `NetworkStore` without a record and `run --net <x>` would refuse with
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
            // Realize it physically (real bridge of the rootless holder) — aligned
            // to the SAME prefix the NetworkStore just decided.
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
            // HONESTY (not a silent no-op): macvlan/ipvlan put the container
            // DIRECTLY on the physical LAN of `parent` — that requires creating the
            // sub-interface in the host init-netns with CAP_NET_ADMIN, a privilege
            // that a rootless session (this engine's default model) does not have.
            // The declarative record is saved (intent preserved for a privileged
            // host), but the physical plane is NOT realized — say it loudly.
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
            // Rootless physical plane (holder netns): bridge + VXLAN uplink + WG (if
            // encrypted). Unlike macvlan/ipvlan, the overlay IS realizable without
            // host privilege — it lives entirely in the holder netns.
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

/// **Realizes the physical plane of an overlay network** in the rootless holder
/// netns:
/// (1) holder bridge aligned to the prefix the `NetworkStore` decided;
/// (2) VXLAN uplink (`dlxvx<vni>`) mastering that bridge + FDB of the peers;
/// (3) WireGuard, IF the overlay is encrypted (`wg_ip` present) — encrypts the
///     VXLAN transport between nodes (the FDB then points to the `wg_ip` instead of
///     the `node_ip`).
///
/// Mirrors `delonix_net::Net::ensure_vxlan`/`ensure_overlay_wg` (the old
/// root/host-netns path), but driven through the holder's control socket — the only
/// one with CAP_NET_ADMIN in the infra netns. Idempotent. Requires the holder up
/// (`ensure_up`). It only makes sense to call when `net.driver == "overlay"`.
fn realize_overlay(net: &Network) -> Result<()> {
    const WG_PORT: u16 = 51820;
    let Some(vni) = net.vni else { return Ok(()) };
    let Some(dev) = net.vxlan_dev() else {
        return Ok(());
    };
    // ENCRYPTED overlay (this node's wg_ip present) REQUIRES `wg` on the host. Fail
    // BEFORE bringing up the VXLAN: otherwise the FDB would point to the peers'
    // wg_ip (only reachable through the tunnel) with no tunnel coming up → uplink
    // silently blackholed. An actionable error instead of an overlay that pretends
    // to be up.
    let encrypted = net.wg_ip.is_some();
    if encrypted && !delonix_net::wg::available() {
        return Err(delonix_runtime_core::Error::Invalid(
            "overlay cifrado (wg_ip) mas 'wg' indisponível no host — instala \
             wireguard-tools + o módulo do kernel, ou remove wg_ip para transporte \
             VXLAN plano (não cifrado)"
                .into(),
        ));
    }
    // Parse the peers ONCE (reused in the FDB and in the WG loop).
    let parsed: Vec<(String, Option<(String, String)>)> = net
        .peers
        .iter()
        .map(|p| delonix_net::parse_overlay_peer(p))
        .collect();
    // Holder up (without incrementing the ref-count — the uplink is persistent
    // infra, not a workload; it dies with `network rm` → `netdel`, not with a
    // release).
    infra::ensure_up()?;
    // The bridge/gateway come from the physical plane aligned to the NetworkStore
    // prefix.
    infra::network_create_with(&net.name, &net.prefix)?;
    let (bridge, _prefix, gateway) = infra::resolve_net(&net.name)?;
    // FDB: `wg_ip` of each peer if encrypted, otherwise the plain `node_ip`.
    let dsts: Vec<String> = parsed
        .iter()
        .map(|(node_ip, wg)| {
            wg.as_ref()
                .map(|(_pubkey, wgip)| wgip.clone())
                .unwrap_or_else(|| node_ip.clone())
        })
        .collect();
    infra::set_vxlan(&dev, vni, &bridge, &gateway, &dsts)?;
    // WireGuard only in the ENCRYPTED overlay (availability was already ensured
    // above).
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

/// `network describe` — readable detail in `kubectl describe` style.
/// Complements `inspect` (the usual compact view, stable for scripts).
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

/// Containers attached to this network, read from the `Store` — `network` (the
/// primary network of `run --net`) or `extra_networks` (those attached later).
///
/// Best-effort on purpose: an error opening/reading the store yields `None`, and
/// `describe` omits the section instead of asserting "<none>". The distinction
/// matters — "there are no attached containers" and "I couldn't tell" are not the
/// same thing in a view used to decide whether a network can be removed.
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
                // The IP on the network in question, be it the primary or an extra.
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
    // Only on the physical-LAN drivers (macvlan/ipvlan).
    d.field_opt("Parent", n.parent.as_deref());
    // Only on the overlay driver.
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

/// Subcommands of `network node` — the WireGuard identity of the local node.
#[derive(clap::Subcommand)]
pub enum NodeCmd {
    /// Generate the node key (if it does not exist yet) and print the public one
    /// with the context of what to do with it. Idempotent.
    Init,
    /// Print only the public key (for composing in scripts).
    Key,
}

/// `network node` — `ensure_node_key` is idempotent: generates on the first time,
/// then reads the one that already exists.
fn cmd_node(action: NodeCmd) -> Result<()> {
    let key = delonix_net::wg::ensure_node_key()?;
    match action {
        NodeCmd::Init => {
            println!(
                "{}",
                super::po::t("node initialized — public key (hand it to the overlay peers):")
            );
            println!("  {}", key.public);
            println!("privada protegida 0600 em <root>/wg/node.key");
        }
        // Just the key, no noise: this usually goes into another command.
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

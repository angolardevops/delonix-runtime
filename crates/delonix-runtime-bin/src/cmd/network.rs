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
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct NetworkSpec {
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

/// Fields the reconciler compares for a `kind: Network`. Only `peers` converges
/// hot (`NetworkStore::add_overlay_peer`); everything else defines the L2/L3
/// plane that containers are already attached to, so it is a replace — and a
/// replace of a network detaches every container on it, which is precisely why
/// it is refused without `--replace`.
///
/// `gateway` is deliberately absent: for a bridge network it is DERIVED from the
/// base octet and the manifest's field is empty by default, so comparing it
/// would report a difference on every plan for every network.
pub(crate) const RECONCILED_NETWORK_FIELDS: &[&str] =
    &["driver", "parent", "subnet", "vni", "peers"];

fn desired_network_fields(spec: &NetworkSpec) -> std::collections::BTreeMap<String, String> {
    let mut f = std::collections::BTreeMap::new();
    f.insert("driver".into(), spec.driver.clone());
    if let Some(p) = &spec.parent {
        f.insert("parent".into(), p.clone());
    }
    if let Some(s) = &spec.subnet {
        f.insert("subnet".into(), s.clone());
    }
    if let Some(v) = spec.vni {
        f.insert("vni".into(), v.to_string());
    }
    if !spec.peers.is_empty() {
        let mut peers = spec.peers.clone();
        peers.sort();
        f.insert("peers".into(), peers.join(","));
    }
    f
}

fn actual_network_fields(n: &Network) -> std::collections::BTreeMap<String, String> {
    let mut f = std::collections::BTreeMap::new();
    f.insert("driver".into(), n.driver.clone());
    if let Some(p) = &n.parent {
        f.insert("parent".into(), p.clone());
    }
    f.insert("subnet".into(), n.subnet.clone());
    if let Some(v) = n.vni {
        f.insert("vni".into(), v.to_string());
    }
    if !n.peers.is_empty() {
        let mut peers = n.peers.clone();
        peers.sort();
        f.insert("peers".into(), peers.join(","));
    }
    f
}

/// Destroys a network so the normal creation path can rebuild it. Every
/// container attached to it loses that attachment — hence the explicit
/// `--replace`.
pub(crate) fn remove_for_replace(name: &str) -> Result<()> {
    let store = NetworkStore::open(state_root())?;
    cmd_rm(&store, name)
}

/// The FDB destination of an overlay peer: its `wg_ip` when the overlay is
/// encrypted, otherwise the plain `node_ip`.
///
/// **Extracted because two places need the SAME answer.** The rule used to live
/// only inside `realize_overlay`; the removal path needs it too, and if the two
/// ever disagreed the engine would delete the wrong FDB entry — leaving the
/// removed peer receiving traffic while cutting off one that should stay.
fn peer_fdb_dst(parsed: &(String, Option<(String, String)>)) -> String {
    let (node_ip, wg) = parsed;
    wg.as_ref()
        .map(|(_pubkey, wgip)| wgip.clone())
        .unwrap_or_else(|| node_ip.clone())
}

/// The encrypted overlay's WireGuard interface, derived from the VNI (<= 15 chars).
/// Same reason as above: realize and remove must name the same device.
fn wg_iface_name(vni: u32) -> String {
    format!("wgo{vni:06x}")
}

/// Applies the hot part of a plan. Only an overlay's peer list converges
/// without recreating the network; everything else defines the plane containers
/// are already attached to.
pub(crate) fn converge(name: &str, diffs: &[super::reconcile::FieldDiff]) -> Result<()> {
    let store = NetworkStore::open(state_root())?;
    for d in diffs {
        match d.field.as_str() {
            "peers" => {
                let (removed, added) =
                    super::reconcile::list_delta(d.from.as_deref(), d.to.as_deref());
                for p in &added {
                    store.add_overlay_peer(name, p)?;
                }
                for p in &removed {
                    remove_peer_everywhere(&store, name, p)?;
                }
                // **O lado ADD nunca tocou no dataplane**, e só se viu ao ligar o
                // lado remove: `add_overlay_peer` escreve o registo e mais nada —
                // quem semeia o FDB é o `realize_overlay`, que corre no `create`.
                // Medido: acrescentar peers por manifesto deixava-os no registo e
                // FORA do FDB, ou seja o overlay não os alcançava enquanto o
                // `network inspect` jurava que sim.
                //
                // Re-semeia com a lista FINAL em vez de só com os `added`: o
                // `do_vxlan` acrescenta apenas o que falta (comparação por token
                // exacto), logo é idempotente, e uma lista completa também repõe
                // o que um respawn do holder tenha levado.
                if !added.is_empty() {
                    reseed_overlay_fdb(&store, name)?;
                }
            }
            other => {
                return Err(delonix_runtime_core::Error::Invalid(format!(
                    "network/{name}: '{other}' does not converge hot — bug in \
                     `reconcile::hot_fields`"
                )))
            }
        }
    }
    Ok(())
}

/// Re-seeds the VXLAN uplink's FDB from the registry's CURRENT peer list.
///
/// Uses the same `set_vxlan` the creation path uses, so there is one way to seed
/// an FDB and not two. The WireGuard side is deliberately NOT redone here: a new
/// peer's tunnel needs `ensure_node_key` and the interface, which is
/// `realize_overlay`'s job — and a converge that quietly re-ran all of it would
/// be recreating the network under the name of a field update.
fn reseed_overlay_fdb(store: &NetworkStore, name: &str) -> Result<()> {
    let net = store.get(name)?;
    let (Some(dev), Some(vni)) = (net.vxlan_dev(), net.vni) else {
        return Ok(());
    };
    let dsts: Vec<String> = net
        .peers
        .iter()
        .map(|p| peer_fdb_dst(&delonix_net::parse_overlay_peer(p)))
        .collect();
    let (bridge, _prefix, gateway) = infra::resolve_net(name)?;
    match infra::set_vxlan(&dev, vni, &bridge, &gateway, &dsts) {
        Err(e) if holder_is_down(&e) => {
            // Nothing to seed into: the uplink lives in the holder's netns and
            // died with it. The registry is the whole truth until it comes back.
            Ok(())
        }
        r => r,
    }
}

/// Retires an overlay peer from ALL THREE places it lives.
///
/// The gap this closes was triple, and only the first was documented: the
/// registry's `peers=` line, the VXLAN **FDB** entry that makes this node flood
/// to it, and — on an encrypted overlay — the **WireGuard peer**, which left a
/// node no longer in the mesh with a working crypto channel. That last one is
/// the security-relevant leftover.
///
/// **Dataplane before registry, deliberately.** If the FDB removal fails, the
/// registry still lists the peer and the next plan proposes the removal again.
/// The other order loses the only record of what still had to be undone.
///
/// A holder that is DOWN is the one failure treated as success, and it is not a
/// shortcut: the uplink and its FDB live in the holder's ephemeral netns, so if
/// the holder is gone the entry is gone with it — removing it from the registry
/// is then the whole truth. Said out loud rather than silently, because "peer
/// removed" and "peer removed from a network that is not running" are different
/// facts.
fn remove_peer_everywhere(store: &NetworkStore, name: &str, peer: &str) -> Result<()> {
    let net = store.get(name)?;
    let parsed = delonix_net::parse_overlay_peer(peer);
    // `vxlan_dev()` and not a second `format!`: the name is hex-encoded
    // (`dlxvx0042`, not `dlxvx66`), and a private copy of the formula would send
    // the deletion to a device that does not exist — reporting success while the
    // peer keeps receiving traffic. The same «one formula, one owner» rule the
    // bridge name already carries.
    if let (Some(dev), Some(vni)) = (net.vxlan_dev(), net.vni) {
        let dst = peer_fdb_dst(&parsed);
        if let Err(e) = infra::del_vxlan_peer(&dev, &dst) {
            if !holder_is_down(&e) {
                return Err(e);
            }
            println!(
                "{}",
                super::po::tf(
                    "network/{name}: peer {peer} removed from the registry (the overlay is not \
                     running, so its FDB entry is already gone)",
                    &[("name", name), ("peer", peer)],
                )
            );
            return store.remove_overlay_peer(name, peer).map(|_| ());
        }
        // The tunnel only exists on an encrypted overlay, and only for a peer
        // that carried a key.
        if net.wg_ip.is_some() {
            if let Some((pubkey, _)) = &parsed.1 {
                if let Err(e) = infra::del_wg_peer(&wg_iface_name(vni), pubkey) {
                    if !holder_is_down(&e) {
                        return Err(e);
                    }
                }
            }
        }
    }
    store.remove_overlay_peer(name, peer)?;
    Ok(())
}

/// Is this the holder being unreachable, rather than the operation failing?
///
/// Matched on the error the control socket produces, because the two mean
/// opposite things here: one says the work is already done, the other says it
/// was not done and nobody noticed.
fn holder_is_down(e: &delonix_runtime_core::Error) -> bool {
    let s = e.to_string();
    s.contains("holder is down") || s.contains("control socket")
}

/// Records that this stack owns the network, and what it last applied.
pub(crate) fn stamp(
    name: &str,
    stack: &str,
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let store = NetworkStore::open(state_root())?;
    store.set_metadata(
        name,
        &[
            (
                super::reconcile::STACK_LABEL.to_string(),
                Some(stack.to_string()),
            ),
            (
                super::reconcile::MANAGED_BY.to_string(),
                Some("delonix".to_string()),
            ),
        ],
        &[(
            super::reconcile::LAST_APPLIED.to_string(),
            Some(super::reconcile::encode_last_applied(fields)),
        )],
    )?;
    Ok(())
}

pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: NetworkSpec = manifest::spec_of(doc)?;
    Ok(super::reconcile::Desired {
        kind: "Network".into(),
        name: doc.metadata.name.clone(),
        fields: desired_network_fields(&spec),
        converges: true,
        ownable: true,
    })
}

pub(crate) fn actual() -> Result<Vec<super::reconcile::Actual>> {
    let store = NetworkStore::open(state_root())?;
    Ok(store
        .list()?
        .into_iter()
        .map(|n| super::reconcile::Actual {
            kind: "Network".into(),
            name: n.name.clone(),
            fields: actual_network_fields(&n),
            owner: n.labels.get(super::reconcile::STACK_LABEL).cloned(),
            last_applied: n
                .annotations
                .get(super::reconcile::LAST_APPLIED)
                .and_then(|raw| super::reconcile::decode_last_applied(raw)),
        })
        .collect())
}

#[derive(Subcommand)]
pub enum NetworkCmd {
    /// 802.1Q VLAN on a physical NIC — **the one command here that needs root**.
    ///
    /// Dry-run by default: it prints the plan and changes nothing until
    /// `--apply`. Everything else in this engine is rootless; a VLAN interface
    /// on a host NIC needs CAP_NET_ADMIN in the host's netns, which no
    /// unprivileged user has (see ADR-0013 tier C).
    Vlan {
        /// Parent NIC on the host (e.g. `eth0`).
        parent: String,
        /// VLAN id, 1-4094.
        id: u16,
        /// Remove the VLAN interface instead of creating it.
        #[arg(long)]
        rm: bool,
        /// Actually run it (as root). Without this it is a dry-run.
        #[arg(long)]
        apply: bool,
    },
    /// Open a DIRECTED path from one network to another (ADR-0013 tier B).
    ///
    /// Networks are isolated from each other by default. A route says a packet
    /// MAY cross; it does not say it is allowed — the per-workload firewall
    /// still decides, and a namespace boundary still needs its own policy.
    Route {
        /// Source network (the side that may initiate).
        from: String,
        /// Destination network.
        to: String,
        /// Close the path instead of opening it.
        #[arg(long)]
        rm: bool,
    },
    /// Dashboard (KPIs + table) of the networks — interactive TUI, or `--once` snapshot.
    Dash {
        #[arg(long)]
        once: bool,
        #[arg(long)]
        json: bool,
    },
    /// List the networks.
    Ls {
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: output::OutputFormat,
    },
    /// WireGuard identity of THIS node, for the encrypted overlay between nodes.
    ///
    /// The VXLAN overlay of `network create --driver overlay`. The private key
    /// stays 0600 in `<root>/wg/node.key`; the public one is what you hand out
    /// to the peers.
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
        /// Subnet. For `bridge`, `10.<200-254>.0.0/16` (only /16); required for
        /// macvlan/ipvlan, e.g. `192.168.1.0/24`. Omit it and a free one is picked.
        //
        // The old text said only "required for macvlan/ipvlan" and stopped
        // there — from v0.47.0 `bridge` honours it too (`base_from_subnet`),
        // which is the version that fixed the bug where the flag was accepted
        // and silently thrown away. Documenting it as macvlan-only left the
        // most-used driver's newest behaviour invisible.
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
        /// Output format: `table` (default, the historical text) or `json` (ADR-0005)
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: super::output::OutputFormat,
    },
    /// Readable detail of one or more networks, `kubectl describe` style.
    ///
    /// For humans; use `inspect` for the usual compact view.
    Describe {
        #[arg(required = true, add = ArgValueCandidates::new(super::complete::networks))]
        names: Vec<String>,
    },
    /// Remove a network.
    Rm {
        #[arg(add = ArgValueCandidates::new(super::complete::networks))]
        name: String,
    },
    /// Apply the `kind: Network` documents of a manifest (idempotent by name).
    Apply {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

pub fn run(action: NetworkCmd) -> Result<()> {
    let store = NetworkStore::open(state_root())?;
    match action {
        NetworkCmd::Dash { once, json } => {
            super::dash::run(super::dash::DashScope::Networks, once, json)
        }
        NetworkCmd::Vlan {
            parent,
            id,
            rm,
            apply,
        } => super::vlan::run(&parent, id, rm, apply),
        NetworkCmd::Route { from, to, rm } => {
            delonix_net::infra::network_route(&from, &to, !rm)?;
            println!(
                "{}",
                super::po::tf(
                    if rm {
                        "route closed: {from} -> {to}"
                    } else {
                        "route open: {from} -> {to}"
                    },
                    &[("from", &from), ("to", &to)],
                )
            );
            Ok(())
        }
        NetworkCmd::Ls { output } => cmd_ls(&store, output),
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
        NetworkCmd::Inspect { name, output } => cmd_inspect(&store, &name, output),
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

/// Refuses the AWS-VPC vocabulary (`terraform-aws-modules/vpc` names) that this
/// engine does not implement, naming what it DOES have.
///
/// A plain "unknown field" warning is right for a typo and wrong for these:
/// somebody writing `singleNatGateway` has a working mental model, it just
/// does not map onto a single-node rootless SDN — there are no availability
/// zones to spread NAT gateways across, and one network is one flat bridge,
/// not a set of routed subnets. Failing closed keeps the promise this repo
/// makes everywhere else: an option that is accepted is an option that works.
fn reject_vpc_vocabulary(doc: &ManifestDoc) -> Result<()> {
    const NOT_HERE: &[(&str, &str)] = &[
        (
            "vpcCidr",
            "use `subnet: 10.<200-254>.0.0/16` — a network IS the address space here",
        ),
        (
            "cidr",
            "use `subnet: 10.<200-254>.0.0/16` — a network IS the address space here",
        ),
        (
            "publicSubnets",
            "not implemented: a network is one flat bridge, and its egress already \
             goes out through the node's NAT",
        ),
        (
            "privateSubnets",
            "not implemented: there is no per-subnet routing policy yet — use a \
             separate network plus `kind: FirewallPolicy` to control who reaches whom",
        ),
        (
            "singleNatGateway",
            "meaningless on a single node: there are no availability zones to place \
             NAT gateways in, and egress already shares one NAT",
        ),
    ];
    let serde_yaml::Value::Mapping(map) = &doc.spec else {
        return Ok(());
    };
    for (field, why) in NOT_HERE {
        if map.contains_key(serde_yaml::Value::String((*field).to_string())) {
            return Err(delonix_runtime_core::Error::Invalid(super::po::tf(
                "Network '{name}': `{field}` — {why}",
                &[("name", &doc.metadata.name), ("field", field), ("why", why)],
            )));
        }
    }
    Ok(())
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    let store = NetworkStore::open(state_root())?;
    for doc in manifest::of_kind(docs, "Network") {
        let name = &doc.metadata.name;
        // Warn about typos BEFORE the early-continue (see container::apply): a
        // re-apply against an already existing network must also see the warning.
        // Before the generic warning: for these fields "unknown field, check
        // the spelling" is misleading — the spelling is right, the concept is
        // the one that does not exist here.
        reject_vpc_vocabulary(doc)?;
        let spec: NetworkSpec = manifest::spec_of(doc)?;
        if let Ok(existing) = store.get(name) {
            // "Ensure present" still means we do not renumber a live network —
            // but now that `subnet` is honoured, staying silent about a spec
            // that asks for a different one would be the same lie this change
            // exists to remove.
            if let Some(want) = spec.subnet.as_deref() {
                if want != existing.subnet {
                    eprintln!(
                        "{}",
                        super::po::tf(
                            "WARNING: network '{name}': the manifest asks for {want} but it \
                             exists as {have} — NOT renumbered (workloads are addressed on it). \
                             Remove and recreate it to change the subnet.",
                            &[("name", name), ("want", want), ("have", &existing.subnet)],
                        )
                    );
                }
            }
            println!(
                "network/{name}: {}",
                super::po::t("already exists, nothing to do")
            );
            continue;
        }
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
        println!("network/{name}: {}", super::po::t("created"));
    }
    Ok(())
}

/// `network ls -o json` row (ADR-0005): stable keys mirroring the table columns.
#[derive(serde::Serialize)]
struct NetworkLsRow {
    name: String,
    driver: String,
    bridge: String,
    subnet: String,
}

fn cmd_ls(store: &NetworkStore, format: output::OutputFormat) -> Result<()> {
    let nets = store.list()?;
    if format == output::OutputFormat::Json {
        let rows: Vec<NetworkLsRow> = nets
            .into_iter()
            .map(|n| NetworkLsRow {
                name: n.name,
                driver: n.driver,
                bridge: n.bridge,
                subnet: n.subnet,
            })
            .collect();
        return output::print_json(&rows);
    }
    let mut t = output::Table::new(&["NAME", "DRIVER", "BRIDGE", "SUBNET"]);
    for n in nets {
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
            // An explicit subnet is HONOURED (or refused, naming what is
            // supported). Until this was wired, both `--subnet` and the
            // manifest's `spec.subnet` were accepted and dropped on the floor
            // for the only driver rootless actually realizes — the caller got
            // an octet derived from the network's name hash and was told
            // nothing. `create_with_base` had existed for this the whole time,
            // with a doc-comment saying so and zero callers.
            let net = match subnet.as_deref() {
                Some(s) => {
                    // Prefixo ARBITRÁRIO (ADR-0013, camada A). Só depois de o
                    // dataplane saber o comprimento: o holder deriva-o do
                    // registo em vez do `/16` fixo, e a chave do registo de
                    // leases passa a vir da rede que CONTÉM o endereço. Ligar
                    // isto antes fazia o `create` passar e o primeiro container
                    // falhar no attach.
                    let cidr = NetworkStore::validate_subnet(s)?;
                    let net = store.create_with_cidr(name, cidr)?;
                    // Um gateway DECLARADO é aceite desde a camada A — mas
                    // validado contra o prefixo, e nunca em silêncio. Ele não
                    // muda quem é dono da bridge (o holder continua a
                    // encaminhar e a mascarar); muda a ROTA DEFAULT que os
                    // containers desta rede recebem, que passa a apontar para
                    // um appliance de fronteira ali dentro.
                    if !gateway.is_empty() && gateway != net.gateway {
                        if let Err(e) = NetworkStore::validate_gateway(&cidr, gateway) {
                            let _ = store.remove(name);
                            return Err(e);
                        }
                        super::output::warn(&super::po::tf(
                            "network '{name}': the default route of its workloads will point at \
                             {gw}, not at the engine ({have}). Nothing answers there until a \
                             workload on this network does — until then they have no way out.",
                            &[("name", name), ("gw", gateway), ("have", &net.gateway)],
                        ));
                    }
                    net
                }
                None => store.create(name)?,
            };
            let declared_gw =
                (!gateway.is_empty() && gateway != net.gateway).then(|| gateway.to_string());
            // Realize it physically (real bridge of the rootless holder) — aligned
            // to the SAME prefix the NetworkStore just decided. If this fails, the
            // declarative record just created above would otherwise be ORPHANED —
            // `network ls` would show it, nothing could attach (NotFound), and a
            // retry would fail with "already exists" until a manual `network rm`.
            // Roll it back so a failed `create` leaves nothing behind to clean up.
            if let Err(e) =
                infra::network_create_with_gateway(name, &net.prefix, declared_gw.as_deref())
            {
                let _ = store.remove(name);
                return Err(e);
            }
            Ok(net)
        }
        "macvlan" | "ipvlan" => {
            let parent = parent.ok_or_else(|| {
                delonix_runtime_core::Error::Invalid(super::po::tf(
                    "--parent is required for driver {driver}",
                    &[("driver", driver)],
                ))
            })?;
            let subnet = subnet.ok_or_else(|| {
                delonix_runtime_core::Error::Invalid(super::po::tf(
                    "--subnet is required for driver {driver}",
                    &[("driver", driver)],
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
                "{}",
                super::po::tf(
                    "warning: network '{name}' (driver {driver}) registered but NOT realized — \
                     condition Realized=False reason=DriverNotImplemented. macvlan/ipvlan \
                     need privilege in the host's init-netns (CAP_NET_ADMIN), which the \
                     rootless model does not have; containers will NOT be able to attach to it. \
                     For rootless multi-node networking use driver 'overlay'.",
                    &[("name", name), ("driver", driver)],
                )
            );
            Ok(net)
        }
        "overlay" => {
            let vni = vni.ok_or_else(|| {
                delonix_runtime_core::Error::Invalid(
                    super::po::t("--vni is required for driver overlay").into(),
                )
            })?;
            let net = store.create_overlay(name, vni, &peers, wg_ip.as_deref())?;
            // Rootless physical plane (holder netns): bridge + VXLAN uplink + WG (if
            // encrypted). Unlike macvlan/ipvlan, the overlay IS realizable without
            // host privilege — it lives entirely in the holder netns.
            // Same treatment as `bridge` above, and for the same reason — NOT the
            // macvlan/ipvlan one. Those are warned-about because rootless genuinely
            // cannot realize them, so the record preserves intent for a privileged
            // host. The overlay CAN be realized here, so a record without an uplink
            // is the orphan the bridge arm exists to prevent: `network ls` shows it,
            // nothing attaches, and the retry hits `already exists` (exit 5).
            //
            // The message this replaces promised "reconciles on the next 'network
            // create'" and that promise was false — `create_overlay` is not
            // idempotent, so the second create conflicts instead of reconciling.
            // Measured: `--wg-ip` with no `wg` on the host exited 0 and printed the
            // network name, leaving a Realized=False record no command could fix.
            if let Err(e) = realize_overlay(&net) {
                let _ = store.remove(name);
                return Err(e);
            }
            Ok(net)
        }
        other => Err(delonix_runtime_core::Error::Invalid(super::po::tf(
            "unknown driver: '{other}' (use bridge|macvlan|ipvlan|overlay)",
            &[("other", other)],
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
            super::po::t(
                "encrypted overlay (wg_ip) but 'wg' is unavailable on the host — install \
                 wireguard-tools + the kernel module, or remove wg_ip for plain (unencrypted) \
                 VXLAN transport",
            )
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
    let dsts: Vec<String> = parsed.iter().map(peer_fdb_dst).collect();
    infra::set_vxlan(&dev, vni, &bridge, &gateway, &dsts)?;
    // WireGuard only in the ENCRYPTED overlay (availability was already ensured
    // above).
    if let Some(my_wg_ip) = net.wg_ip.as_deref() {
        let key = delonix_net::wg::ensure_node_key()?;
        let iface = wg_iface_name(vni);
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

/// The machine view of a network (`inspect -o json`).
///
/// `Network` is not `Serialize` — its record is `key=value` lines with several
/// writers — so this is a dedicated view struct, which is just as well: it makes
/// the published contract explicit instead of leaking whatever the on-disk shape
/// happens to be today. `cli-stability.md` already promised this output as
/// stable; only `container inspect` was actually emitting it.
#[derive(serde::Serialize)]
struct NetworkInspect<'a> {
    name: &'a str,
    driver: &'a str,
    bridge: &'a str,
    subnet: &'a str,
    gateway: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vni: Option<u32>,
    peers: &'a [String],
}

fn cmd_inspect(
    store: &NetworkStore,
    name: &str,
    format: super::output::OutputFormat,
) -> Result<()> {
    let n = store.get(name)?;
    if format == super::output::OutputFormat::Json {
        return super::output::print_json(&[NetworkInspect {
            name: &n.name,
            driver: &n.driver,
            bridge: &n.bridge,
            subnet: &n.subnet,
            gateway: &n.gateway,
            parent: n.parent.as_deref(),
            vni: n.vni,
            peers: &n.peers,
        }]);
    }
    println!("{}:     {}", super::po::t("name"), n.name);
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
                super::po::t("<unknown> (could not read the container store)"),
            );
        }
    }
    d.print();
}

pub(crate) fn cmd_rm(store: &NetworkStore, name: &str) -> Result<()> {
    // Read the VXLAN device name BEFORE the record goes: it is derived from the
    // `vni`, which only the store record carries. Removing the uplink first
    // also avoids the state that leaked before — a device mastered on a bridge
    // that has just been deleted.
    let uplink = store.get(name).ok().and_then(|n| n.vxlan_dev());
    store.remove(name)?;
    if let Some(dev) = uplink {
        infra::vxlan_remove(&dev);
    }
    infra::network_remove(name);
    println!("{name}");
    Ok(())
}

/// Subcommands of `network node` — the WireGuard identity of the local node.
#[derive(clap::Subcommand)]
pub enum NodeCmd {
    /// Generate the node key and print the public one. Idempotent.
    ///
    /// Generated only if it does not exist yet, and printed with the context
    /// of what to do with it.
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
            println!(
                "{}",
                super::po::t("private key protected 0600 at <root>/wg/node.key")
            );
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

    /// Same guard as the sibling Kinds: the documented list is the constant's
    /// real consumer, so a field cannot start being compared in silence.
    #[test]
    fn os_campos_comparados_sao_os_documentados() {
        let spec: super::NetworkSpec = serde_yaml::from_str(
            "driver: overlay\nsubnet: 10.42.0.0/16\nvni: 42\npeers: [10.0.0.7]\n",
        )
        .unwrap();
        let f = super::desired_network_fields(&spec);
        for k in f.keys() {
            assert!(
                super::RECONCILED_NETWORK_FIELDS.contains(&k.as_str()),
                "{k} is compared but undocumented"
            );
        }
        // `gateway` is derived from the base octet and empty in the manifest by
        // default — comparing it would report a difference on every plan.
        assert!(!f.contains_key("gateway"));
    }

    /// **O destino do FDB tem de ser o MESMO no realize e na remoção.**
    ///
    /// A regra («`wg_ip` se o overlay é cifrado, senão o `node_ip`») vivia só
    /// dentro do `realize_overlay`. Duplicá-la no caminho de remoção é como as
    /// duas passam a discordar — e o sintoma seria o pior possível: apaga-se a
    /// entrada ERRADA, o peer removido continua a receber tráfego e um que devia
    /// ficar deixa de o receber. Por isso é uma função, e este teste exerce-a com
    /// as duas formas que um peer pode ter.
    #[test]
    fn o_dst_do_fdb_e_o_mesmo_para_as_duas_formas_de_peer() {
        // Cifrado: manda o `wg_ip`, que é o endereço DENTRO do túnel.
        let cifrado = (
            "10.0.0.7".to_string(),
            Some(("chave".to_string(), "10.9.0.7".to_string())),
        );
        assert_eq!(super::peer_fdb_dst(&cifrado), "10.9.0.7");
        // Em claro: manda o `node_ip`, o endereço real do nó.
        let claro = ("10.0.0.8".to_string(), None);
        assert_eq!(super::peer_fdb_dst(&claro), "10.0.0.8");
        // E o que a função devolve é o que o `parse_overlay_peer` produz a partir
        // da string do registo — o elo que fecha o ciclo add→remove.
        let do_registo = delonix_net::parse_overlay_peer("10.0.0.7=chave=10.9.0.7");
        assert_eq!(super::peer_fdb_dst(&do_registo), "10.9.0.7");
    }

    /// O nome do device VXLAN tem UMA fórmula, e é hex.
    ///
    /// Escrevi `format!("dlxvx{vni}")` (decimal) na primeira versão da remoção, e
    /// o `Network::vxlan_dev()` produz `dlxvx002a` para o VNI 42. A deleção teria
    /// ido para um device inexistente e reportado sucesso — o peer a receber
    /// tráfego com o registo a dizer que saiu.
    #[test]
    fn o_nome_do_device_vxlan_e_o_do_motor() {
        let tmp = std::env::temp_dir().join(format!("dlx-vxdev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = delonix_net::NetworkStore::open(&tmp).unwrap();
        let net = store.create_overlay("m", 42, &[], None).unwrap();
        assert_eq!(net.vxlan_dev().as_deref(), Some("dlxvx002a"));
        assert_ne!(net.vxlan_dev().as_deref(), Some("dlxvx42"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

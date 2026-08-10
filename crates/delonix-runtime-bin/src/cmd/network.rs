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
                if !removed.is_empty() {
                    // Say it rather than drop it: `add_overlay_peer` has no
                    // inverse today, so a peer removed from the manifest stays
                    // in the FDB. Silently reporting success here would be the
                    // exact dishonesty this feature exists to remove.
                    eprintln!(
                        "{}",
                        super::po::tf(
                            "WARNING: network/{name}: peer(s) {peers} were removed from the \
                             manifest but removing an overlay peer is not implemented — \
                             recreate the network to drop them",
                            &[("name", name), ("peers", &removed.join(", "))],
                        )
                    );
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
        NetworkCmd::Dash { once, json } => {
            super::dash::run(super::dash::DashScope::Networks, once, json)
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
            let net = store.create(name)?;
            // Realize it physically (real bridge of the rootless holder) — aligned
            // to the SAME prefix the NetworkStore just decided. If this fails, the
            // declarative record just created above would otherwise be ORPHANED —
            // `network ls` would show it, nothing could attach (NotFound), and a
            // retry would fail with "already exists" until a manual `network rm`.
            // Roll it back so a failed `create` leaves nothing behind to clean up.
            if let Err(e) = infra::network_create_with(name, &net.prefix) {
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
            if let Err(e) = realize_overlay(&net) {
                eprintln!(
                    "{}",
                    super::po::tf(
                        "warning: overlay network '{name}' registered but the physical uplink did not \
                         come up ({e}) — condition Realized=False. Reconciles on the next \
                         'network create' once the holder/peers are available.",
                        &[("name", name), ("e", &e.to_string())],
                    )
                );
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
}

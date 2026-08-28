//! `delonix system namespace` — the isolation boundary, made visible. Moved
//! here from the top level (B2 of the CLI restructuring): a namespace is a
//! property of the ENGINE's isolation model, not a peer of `container`/
//! `image`/`vm`.
//!
//! A namespace has **no record of its own**: it exists while something is in
//! it. So this group derives, exactly like `cluster ls` (from labels) and
//! `pod ls` (from labels) — there is no store to add, and adding one would be a
//! new resource, not a listing.
//!
//! # Why there is no `create`/`rm`
//!
//! An empty namespace would need a registry to exist in, which makes it a
//! resource with a lifecycle, quotas and a teardown — a decision for an ADR,
//! not for a listing command. The group's `--help` says so, because leaving the
//! question unanswered is how an operator concludes the command is missing.
//!
//! # The host property that decides whether any of this is real
//!
//! The boundary is nftables chains on the `forward` hook, and traffic between
//! two containers on the SAME bridge only reaches them through `br_netfilter`.
//! Without it every rule installs, every command reports success, and the
//! namespaces do not isolate. That is the most expensive silent failure this
//! engine can have, so a listing of namespaces is exactly where it has to be
//! said — see [`isolation_state`].

use clap::Subcommand;
use delonix_net::infra;
use delonix_runtime_core::Result;
use serde::Serialize;

use super::output;
use super::util::state_root;

#[derive(Subcommand)]
pub enum NamespaceCmd {
    /// List the isolation namespaces IN USE, with what is in each.
    Ls {
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: output::OutputFormat,
    },
    /// What is inside ONE namespace, by Kind, `kubectl describe` style.
    Describe {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::namespaces))]
        name: String,
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: output::OutputFormat,
    },
}

/// What is in one namespace. Names, not just counts: `describe` prints them and
/// `ls` counts them, so the two can never disagree about what a namespace holds.
#[derive(Default, Serialize)]
struct Contents {
    containers: Vec<String>,
    pods: Vec<String>,
    vms: Vec<String>,
    volumes: Vec<String>,
}

impl Contents {
    fn total(&self) -> usize {
        self.containers.len() + self.pods.len() + self.vms.len() + self.volumes.len()
    }
}

/// Whether the boundary this group describes is actually enforced ON THIS HOST.
///
/// Three states, and the third is the one that matters: a query that FAILS is
/// not «it is off». The holder may be down or too old, and answering `inert`
/// to that would send an operator to fix a host that is fine — the same
/// discipline as `infra::network_routes_live`.
enum Isolation {
    Active,
    Inert,
    Unknown,
}

fn isolation_state() -> Isolation {
    match infra::br_netfilter_active() {
        Ok(true) => Isolation::Active,
        Ok(false) => Isolation::Inert,
        Err(_) => Isolation::Unknown,
    }
}

/// Every namespace in use, and what it holds.
///
/// The SOURCES are the ones `complete::namespaces` declares in
/// `NAMESPACE_SOURCES` — containers, VMs and volumes carry the namespace, and
/// pods/workloads/stacks stamp it onto what they lower to. Counting has to read
/// the records rather than just the names, which is why this does not call that
/// function: it answers a different question about the same table.
fn collect() -> std::collections::BTreeMap<String, Contents> {
    collect_in(&state_root())
}

/// The root is a PARAMETER, and not resolved inside, for the reason the rest of
/// this repo already learned: a test that calls a function which resolves
/// `state_root()` writes to the machine's REAL state. See the note on
/// `apply_share` in the manifest work.
fn collect_in(root: &std::path::Path) -> std::collections::BTreeMap<String, Contents> {
    let mut out: std::collections::BTreeMap<String, Contents> = Default::default();
    // SEEDED from the completer's own list, not from a second walk of the same
    // stores. `default` comes with it — it is where everything lands, and a node
    // with nothing running would otherwise print an empty table that reads as
    // «this engine has no namespaces» rather than «nothing is running».
    //
    // Measured before this line existed: a tenant whose volume sub-tree was
    // there but held no record yet was OFFERED by TAB and then answered
    // `no such namespace` by `describe`. Two derivations of one question is how
    // they come to disagree, and the disagreement lands on the operator as a
    // namespace that exists and does not.
    for ns in super::complete::namespace_names_in(root) {
        out.entry(ns).or_default();
    }

    if let Ok(store) = delonix_runtime_core::Store::open(root.join("containers")) {
        let all = store.list().unwrap_or_default();
        for c in &all {
            let ns = if c.namespace.is_empty() {
                "default"
            } else {
                &c.namespace
            };
            out.entry(ns.to_string())
                .or_default()
                .containers
                .push(c.name.clone());
        }
        // A pod is its labelled members; the namespace is the members' own, and
        // `pod_member_run_opts` stamps the pod's onto every one of them.
        let mut seen: std::collections::BTreeSet<(String, String)> = Default::default();
        for c in &all {
            if let Some(p) = c.labels.get(super::pod::POD_LABEL) {
                let ns = if c.namespace.is_empty() {
                    "default"
                } else {
                    &c.namespace
                };
                seen.insert((ns.to_string(), p.clone()));
            }
        }
        for (ns, pod) in seen {
            out.entry(ns).or_default().pods.push(pod);
        }
    }

    for v in delonix_vm::list(root).unwrap_or_default() {
        let ns = if v.namespace.is_empty() {
            "default".to_string()
        } else {
            v.namespace.clone()
        };
        out.entry(ns).or_default().vms.push(v.name);
    }

    if let Ok(store) = delonix_volume::VolumeStore::open(root) {
        // `None` is the UNSCOPED root and not the `default` namespace — an
        // ordinary volume belongs to no tenant, and folding it into `default`
        // would invent an owner for every volume on the node.
        for ov in store.list_all().unwrap_or_default() {
            if let Some(ns) = ov.namespace {
                out.entry(ns).or_default().volumes.push(ov.volume.name);
            }
        }
    }
    for c in out.values_mut() {
        c.containers.sort();
        c.pods.sort();
        c.vms.sort();
        c.volumes.sort();
    }
    out
}

/// The JSON of `describe`. Carries the name and the enforcement state as well
/// as the contents — a bare `Contents` would answer «what is inside» without
/// saying WHICH namespace, nor whether the boundary is real on this host.
#[derive(Serialize)]
struct NsDetail<'a> {
    namespace: &'a str,
    isolation_set: String,
    /// `null` when this host could not be asked — never `false`, which would
    /// read as «the boundary is off».
    enforced: Option<bool>,
    #[serde(flatten)]
    contents: &'a Contents,
}

#[derive(Serialize)]
struct NsRow {
    namespace: String,
    containers: usize,
    pods: usize,
    vms: usize,
    volumes: usize,
}

/// Says it out loud when the boundary is not enforced.
///
/// Only when there is more than one namespace: on a node where everything is in
/// `default` there is no boundary to be inert, and warning there would be noise
/// that teaches operators to ignore the line that matters.
fn warn_if_inert(ns_count: usize) {
    if ns_count < 2 {
        return;
    }
    if let Isolation::Inert = isolation_state() {
        output::warn(super::po::t(
            "these namespaces do NOT isolate on this host: bridge traffic is not filtered (br_netfilter not loaded, or net.bridge.bridge-nf-call-iptables=0), so every rule installs and reports success while traffic crosses anyway. Fix: modprobe br_netfilter && sysctl -w net.bridge.bridge-nf-call-iptables=1 net.bridge.bridge-nf-call-ip6tables=1 (persist via /etc/modules-load.d and /etc/sysctl.d, or `install.sh --tune`)",
        ));
    }
}

pub fn run(cmd: NamespaceCmd) -> Result<()> {
    match cmd {
        NamespaceCmd::Ls { output: fmt } => {
            let all = collect();
            if fmt == output::OutputFormat::Json {
                let rows: Vec<NsRow> = all
                    .iter()
                    .map(|(ns, c)| NsRow {
                        namespace: ns.clone(),
                        containers: c.containers.len(),
                        pods: c.pods.len(),
                        vms: c.vms.len(),
                        volumes: c.volumes.len(),
                    })
                    .collect();
                return output::print_json(&rows);
            }
            let mut t = output::Table::new(&["NAMESPACE", "CONTAINERS", "PODS", "VMS", "VOLUMES"]);
            for (ns, c) in &all {
                t.row(vec![
                    ns.clone(),
                    c.containers.len().to_string(),
                    c.pods.len().to_string(),
                    c.vms.len().to_string(),
                    c.volumes.len().to_string(),
                ]);
            }
            t.print();
            warn_if_inert(all.len());
            Ok(())
        }
        NamespaceCmd::Describe { name, output: fmt } => {
            let all = collect();
            let c = all.get(&name).ok_or_else(|| {
                delonix_runtime_core::Error::NotFound(super::po::tf(
                    "namespace {name} (nothing is in it; see `delonix system namespace ls`)",
                    &[("name", &name)],
                ))
            })?;
            if fmt == output::OutputFormat::Json {
                return output::print_json(&[NsDetail {
                    namespace: &name,
                    isolation_set: infra::dlxns_set(&name),
                    enforced: match isolation_state() {
                        Isolation::Active => Some(true),
                        Isolation::Inert => Some(false),
                        Isolation::Unknown => None,
                    },
                    contents: c,
                }]);
            }
            let mut d = output::Describe::new();
            d.field("Name", &name);
            // The nft set is what an operator greps for in the holder; naming it
            // saves deriving the hash by hand. Printed even when the holder is
            // down: the NAME is a pure function of the namespace.
            d.field("Isolation set", infra::dlxns_set(&name));
            d.field(
                "Enforced",
                match isolation_state() {
                    Isolation::Active => "yes",
                    Isolation::Inert => "NO — bridge traffic is not filtered on this host",
                    Isolation::Unknown => "unknown (could not ask this host)",
                },
            );
            for (label, items) in [
                ("Containers", &c.containers),
                ("Pods", &c.pods),
                ("VMs", &c.vms),
                ("Volumes", &c.volumes),
            ] {
                if !items.is_empty() {
                    d.list(label, items);
                }
            }
            if c.total() == 0 {
                d.field("Contents", "(empty)");
            }
            d.print();
            warn_if_inert(all.len());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::collect_in;

    fn tmp(line: u32) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "delonix-namespace-ls-test-{}-{}",
            std::process::id(),
            line
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// The drift this command was built with, and then had to close.
    ///
    /// A tenant whose volume sub-tree exists but holds no RECORD yet was
    /// offered by TAB and answered `no such namespace` by `describe` — a
    /// namespace that exists and does not, in the same breath. It happened
    /// because the listing walked the stores a second time instead of asking
    /// the one derivation `complete::namespace_names` owns.
    ///
    /// `list_all()` alone still cannot see it — that is the whole point, and
    /// this test says so out loud so nobody «simplifies» the seeding back.
    #[test]
    fn a_namespace_with_no_records_yet_is_still_listed() {
        let root = tmp(line!());
        std::fs::create_dir_all(root.join("volumes/.ns/inquilino-b")).unwrap();

        let store = delonix_volume::VolumeStore::open(&root).unwrap();
        assert!(
            store.list_all().unwrap_or_default().is_empty(),
            "precondition: no volume RECORD exists yet"
        );
        assert!(
            store.namespaces().contains(&"inquilino-b".to_string()),
            "the owning module sees the sub-tree even with no record in it"
        );

        let listed = collect_in(&root);
        assert!(
            listed.contains_key("inquilino-b"),
            "a namespace TAB offers has to be in the listing; got {:?}",
            listed.keys().collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `default` is always there: a node with nothing running printing an empty
    /// table reads as «this engine has no namespaces», not «nothing is running».
    #[test]
    fn default_is_always_listed() {
        let root = tmp(line!());
        assert!(collect_in(&root).contains_key("default"));
        let _ = std::fs::remove_dir_all(&root);
    }
}

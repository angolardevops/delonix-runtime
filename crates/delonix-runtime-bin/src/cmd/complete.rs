//! Autocompletion candidates for RESOURCE NAMES — containers, pods, images,
//! VM images, volumes, share volumes, network storages, networks, VMs,
//! workloads, clusters, secrets, tunnels, namespaces and registries.
//!
//! The dynamic `clap_complete` already completes commands/subcommands/flags from
//! the `Cli` definition. What was missing was TAB over the arguments: `delonix
//! container stop <TAB>` suggested nothing, and the user had to go to a
//! `container ls` to copy the name by hand — exactly what docker/podman
//! spare you.
//!
//! # Why it is cheap to do this here (and would not be in a remote client)
//!
//! Each candidate comes from a LOCAL on-disk Store (`$DELONIX_ROOT/…`), read
//! directly. There is no daemon to contact nor network in between, so a TAB
//! costs one directory read. (A PaaS HTTP client could not do the same
//! without a network call per TAB — which is why `delonixctl`
//! deliberately does not complete names.)
//!
//! # Rule: fail SILENTLY
//!
//! A completer must NEVER write to the terminal nor panic — it is running
//! in the middle of the user's command line, on every TAB. If the store
//! does not open (nonexistent root, permissions, state mid-write), the
//! right answer is "I have no suggestions", not an error in the middle of the
//! prompt. Hence the `unwrap_or_default()` everywhere.
//!
//! # Rule: DERIVED state comes from whoever owns it
//!
//! Several resources here have no registry of their own — a pod is the
//! `delonix.io/pod` label on its members, a share volume is a record split
//! across one directory per namespace. Those layouts are NOT re-implemented
//! here: the owning module exposes a `completion_names()` and this file calls
//! it. A second copy of "where the records live" is exactly how a TAB starts
//! suggesting names that no command can resolve.

use super::kinds as k;
use clap_complete::engine::CompletionCandidate;

use super::util::state_root;

fn cands<I: IntoIterator<Item = String>>(nomes: I) -> Vec<CompletionCandidate> {
    nomes.into_iter().map(CompletionCandidate::new).collect()
}

/// Containers running **and** stopped: `start`/`rm` want the stopped ones, the
/// `exec`/`logs` the live ones. Filtering by state here would give a TAB that "hides"
/// the container the user is actually trying to type.
pub fn containers() -> Vec<CompletionCandidate> {
    let Ok(store) = delonix_runtime_core::Store::open(state_root().join("containers")) else {
        return Vec::new();
    };
    cands(store.list().unwrap_or_default().into_iter().map(|c| c.name))
}

/// Local images, by their readable reference (without the `@sha256:…` when there
/// is a tag — see `output::display_ref`; a 71-char digest is not completed with
/// TAB, you type it).
pub fn images() -> Vec<CompletionCandidate> {
    let Ok(store) = delonix_image::ImageStore::open(state_root()) else {
        return Vec::new();
    };
    cands(
        store
            .list()
            .unwrap_or_default()
            .into_iter()
            .flat_map(|i| i.repo_tags)
            .map(|t| super::output::display_ref(&t)),
    )
}

pub fn volumes() -> Vec<CompletionCandidate> {
    let Ok(store) = delonix_volume::VolumeStore::open(state_root()) else {
        return Vec::new();
    };
    cands(store.list().unwrap_or_default().into_iter().map(|v| v.name))
}

pub fn networks() -> Vec<CompletionCandidate> {
    let Ok(store) = delonix_net::NetworkStore::open(state_root()) else {
        return Vec::new();
    };
    cands(store.list().unwrap_or_default().into_iter().map(|n| n.name))
}

pub fn vms() -> Vec<CompletionCandidate> {
    cands(
        delonix_vm::list(&state_root())
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.name),
    )
}

/// Kind-mode clusters — derived from the nodes' label, which is the source of truth
/// (there is no separate "cluster" record; see `cmd::kindmode::list`).
pub fn clusters() -> Vec<CompletionCandidate> {
    let Ok(store) = delonix_runtime_core::Store::open(state_root().join("containers")) else {
        return Vec::new();
    };
    let mut nomes: Vec<String> = store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| c.labels.get("io.x-k8s.kind.cluster").cloned())
        .collect();
    nomes.sort();
    nomes.dedup();
    cands(nomes)
}

/// Names of the vault secrets.
pub fn secrets() -> Vec<CompletionCandidate> {
    let Ok(store) = delonix_runtime_core::SecretStore::open(state_root()) else {
        return Vec::new();
    };
    cands(store.list().into_iter().map(|s| s.name))
}

/// Pods — derived from the `delonix.io/pod` label on their members, which is
/// where pod membership lives (there is no pod registry; see `cmd::pod`).
/// Deduped, because a pod of N containers carries the label N times.
pub fn pods() -> Vec<CompletionCandidate> {
    let Ok(store) = delonix_runtime_core::Store::open(state_root().join("containers")) else {
        return Vec::new();
    };
    let mut nomes: Vec<String> = store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| c.labels.get(super::pod::POD_LABEL).cloned())
        .collect();
    nomes.sort();
    nomes.dedup();
    cands(nomes)
}

/// Golden/appliance VM images (`<root>/vm-images/`) — a store of its own,
/// separate from the container images above. The `.json` FILE NAME is not the
/// image name (`VmImageStore::sanitize` maps `:` to `_`), so the name is read
/// from the record, never guessed from the path.
pub fn vm_images() -> Vec<CompletionCandidate> {
    let Ok(store) = super::vmimage::VmImageStore::open(state_root()) else {
        return Vec::new();
    };
    cands(store.list().unwrap_or_default().into_iter().map(|i| i.name))
}

/// Share volumes — the records live in one directory per namespace plus the
/// pre-scoping flat ones, so the layout is read by the module that owns it.
pub fn sharevolumes() -> Vec<CompletionCandidate> {
    cands(super::sharevolume::completion_names())
}

/// Public tunnels (`<root>/tunnels/`), live or not: `rm` wants the dead ones
/// too — the same reason `containers` does not filter by state.
pub fn tunnels() -> Vec<CompletionCandidate> {
    cands(super::tunnel::completion_names())
}

/// Workloads = containers AND VMs, the union `workload ls` prints. A name
/// owned by both is offered ONCE: `workload describe` refuses the ambiguity
/// out loud, and a TAB that printed it twice would read as a bug in the list,
/// not as the collision it is.
pub fn workloads() -> Vec<CompletionCandidate> {
    let mut nomes: Vec<String> = Vec::new();
    if let Ok(store) = delonix_runtime_core::Store::open(state_root().join("containers")) {
        nomes.extend(store.list().unwrap_or_default().into_iter().map(|c| c.name));
    }
    nomes.extend(
        delonix_vm::list(&state_root())
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.name),
    );
    nomes.sort();
    nomes.dedup();
    cands(nomes)
}

/// How a namespaced Kind's namespaces are found. Three Kinds keep a record this
/// module can read; the rest stamp the namespace onto whatever they lower to,
/// and are covered by scanning THAT.
///
/// The distinction is written down instead of being left to whoever reads
/// [`namespaces`] next, because "covered transitively" and "forgotten" look
/// exactly alike in a function that just scans two stores.
enum NsSource {
    /// This module reads the Kind's own store — and the collector IS the table
    /// entry, so [`namespaces`] cannot drift from what the table claims.
    Store(fn(&std::path::Path) -> Vec<String>),
    /// The Kind carries no store of its own here — the namespace travels to the
    /// resource named, and that one IS scanned. The string says which, and why.
    ///
    /// At runtime only the VARIANT matters (it is skipped); the reason is read
    /// by `every_via_points_at_a_scanned_kind`, which is the point of it being
    /// data and not a comment — a comment claiming "Pod is covered via
    /// Container" cannot be checked when someone removes the Container source.
    #[cfg_attr(not(test), allow(dead_code))]
    Via(&'static str),
}

/// Every Kind that answers something other than [`kinds::Namespaced::Never`],
/// and where its namespaces come from.
///
/// A Kind missing from here is a namespace the completion cannot offer, which
/// is how a whole tenant becomes untypable. `every_namespaced_kind_declares_a_source`
/// makes leaving one out a test failure rather than a silent gap — the same
/// reason `cmd::kinds` exists at all.
///
/// The table GOVERNS: [`namespaces`] runs the `Store` collectors from here and
/// keeps no second list. A classifier nothing consults is the seventh list this
/// codebase already paid for once.
const NAMESPACE_SOURCES: &[(&str, NsSource)] = &[
    (k::CONTAINER, NsSource::Store(ns_from_containers)),
    (k::VM, NsSource::Store(ns_from_vms)),
    // `PerDocument`: a plain volume is global, one with a `share:` block is
    // scoped, and `list_all` is the call that returns the owner alongside the
    // record (`VolumeStore::list` deliberately does NOT see the scoped ones).
    (k::VOLUME, NsSource::Store(ns_from_volumes)),
    (
        k::POD,
        NsSource::Via(
            "Container — `pod_member_run_opts` stamps the pod's namespace onto every member",
        ),
    ),
    (
        k::WORKLOAD,
        NsSource::Via("Container/VirtualMachine — it lowers to one of them and the namespace goes with it"),
    ),
    (
        k::STACK,
        NsSource::Via(
            "Container/VirtualMachine — `manifest::load` propagates the namespace onto every child it expands",
        ),
    ),
];

fn ns_from_containers(root: &std::path::Path) -> Vec<String> {
    let Ok(store) = delonix_runtime_core::Store::open(root.join("containers")) else {
        return Vec::new();
    };
    store
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.namespace)
        .collect()
}

fn ns_from_vms(root: &std::path::Path) -> Vec<String> {
    delonix_vm::list(root)
        .unwrap_or_default()
        .into_iter()
        .map(|v| v.namespace)
        .collect()
}

/// A tenant whose only resource is a share volume was invisible until this
/// existed: nothing of theirs is running, so neither store above knows the name.
///
/// Asks `VolumeStore::namespaces()` — the owning module's own answer, which
/// exists so a caller can walk every namespace WITHOUT knowing the on-disk
/// layout (the `.ns` sub-tree name is private precisely so it can change).
/// `list_all()` would reach the same names by reading every volume record and
/// dropping the `None` owners, which is a second copy of "where the records
/// live" — the thing the rule at the top of this file forbids.
fn ns_from_volumes(root: &std::path::Path) -> Vec<String> {
    let Ok(store) = delonix_volume::VolumeStore::open(root) else {
        return Vec::new();
    };
    store.namespaces()
}

/// Isolation namespaces IN USE — a namespace has no record of its own; it
/// exists while something is in it. The sources are declared in
/// [`NAMESPACE_SOURCES`] and this runs them; the `Via` Kinds come with them.
///
/// `default` is always offered: it is where everything lands, and a node with
/// nothing running would otherwise complete nothing at all.
pub fn namespaces() -> Vec<CompletionCandidate> {
    cands(namespace_names())
}

/// The names themselves, so a caller that is not a completer can ask the same
/// question and get the same answer.
///
/// `cmd::namespace::ls` is that caller, and a test pins the two together — a
/// namespace offered by TAB and absent from the listing is a namespace that
/// exists and does not, which is exactly what happened before this existed.
pub(crate) fn namespace_names() -> Vec<String> {
    namespace_names_in(&state_root())
}

/// The root is a PARAMETER so a caller can ask about a root that is not this
/// machine's. Mixing an injected root with an ambient one is how a listing
/// seeds itself from one place and counts from another — measured, and the
/// reason this signature changed.
pub(crate) fn namespace_names_in(root: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = vec!["default".to_string()];
    for (_, src) in NAMESPACE_SOURCES {
        if let NsSource::Store(collect_from) = src {
            names.extend(collect_from(root));
        }
    }
    names.sort();
    names.dedup();
    names
}

/// What `cluster kube generate` accepts: a container name OR a pod name (it
/// resolves either, emitting one k8s Pod manifest). Two lists, one namespace of
/// names — offering only half of them is what sent users to `pod ls` by hand.
pub fn containers_or_pods() -> Vec<CompletionCandidate> {
    let mut all = containers();
    all.extend(pods());
    all
}

/// Named SDN netns the holder serves — what `net netns attach/exec/…` take.
///
/// Derived, because the names live in the holder's `/run/netns`, inside ITS
/// mount namespace: unreachable from here without a re-exec, which is far too
/// much for one TAB. The two producers are the source instead — a container on
/// a custom network gets a netns named by its ID (`infra::attach_container`
/// takes `c.id`), and a pod gets `pod-<name>`.
///
/// `--net host`/`none` containers are filtered out: they have no netns at all,
/// and offering them would be a TAB suggesting a name no command can resolve —
/// the exact failure the rule at the top of this file forbids.
pub fn netns() -> Vec<CompletionCandidate> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(store) = delonix_runtime_core::Store::open(state_root().join("containers")) {
        let all = store.list().unwrap_or_default();
        names.extend(
            all.iter()
                // `network` is what the container ENDED UP on; `None` is `--net
                // host`/`none`, which has no netns at all. The field that records
                // what `--net` ASKED FOR is a different one, and using it here
                // would offer `host` as a netns name.
                .filter(|c| c.network.is_some())
                .map(|c| c.id.clone()),
        );
        // The pod's netns is named by the POD, not by any member — the format
        // has one owner (`pod::pod_netns_name`) and this asks it, rather than
        // writing `pod-{name}` a second time.
        let mut pods: Vec<String> = all
            .iter()
            .filter_map(|c| c.labels.get(super::pod::POD_LABEL).cloned())
            .collect();
        pods.sort();
        pods.dedup();
        names.extend(pods.iter().map(|p| super::pod::pod_netns_name(p)));
    }
    names.sort();
    names.dedup();
    cands(names)
}

/// Top-level commands, for `delonix man <TAB>`.
///
/// Reads the SAME `manual::ENTRIES` that `man` renders, so a TAB can never
/// offer a page that does not exist. Only the first token: the argument is a
/// `Vec<String>` and this completer cannot see which position it is filling, so
/// completing `container run` would need position awareness it does not have.
/// The first word is the one worth having — it is where the tree branches.
pub fn man_commands() -> Vec<CompletionCandidate> {
    let mut names: Vec<String> = super::manual::ENTRIES
        .iter()
        .filter_map(|e| e.path.split_whitespace().next().map(str::to_string))
        .collect();
    names.sort();
    names.dedup();
    cands(names)
}

/// Registries this node is logged in to (`<root>/auth.json`) — the only ones
/// `image logout` has anything to remove.
pub fn registries() -> Vec<CompletionCandidate> {
    cands(delonix_image::auth::hosts(&state_root()))
}

#[cfg(test)]
mod tests {
    use super::{NsSource, NAMESPACE_SOURCES};
    use crate::cmd::kinds::{self, Namespaced};

    /// The gate this module exists to hold: a Kind that `cmd::kinds` says
    /// carries a namespace, and that nothing here knows how to reach, is a
    /// tenant whose name TAB can never offer.
    ///
    /// It failed when written — `Volume` answers `PerDocument` and the
    /// derivation only read the container and VM stores, so a tenant whose
    /// single resource was a share volume did not exist as far as completion
    /// was concerned.
    #[test]
    fn every_namespaced_kind_declares_a_source() {
        let missing: Vec<&str> = kinds::all()
            .filter(|f| f.namespaced != Namespaced::Never)
            .map(|f| f.kind)
            .filter(|k| !NAMESPACE_SOURCES.iter().any(|(kind, _)| kind == k))
            .collect();
        assert!(
            missing.is_empty(),
            "namespaced Kind(s) with no source declared in NAMESPACE_SOURCES: {}. \
             Add the row: `Store` if this module reads its registry, \
             `Via(...)` if the namespace travels to another Kind that IS already scanned.",
            missing.join(", ")
        );
    }

    /// The other direction, and it is not symmetry for its own sake: an entry
    /// for a Kind that is `Never` claims to cover a namespace that does not
    /// exist, and an entry for a Kind that was REMOVED reads as coverage while
    /// covering nothing. Both make the table above lie in the reassuring
    /// direction.
    #[test]
    fn no_declared_source_is_left_over() {
        let leftover: Vec<&str> = NAMESPACE_SOURCES
            .iter()
            .map(|(kind, _)| *kind)
            .filter(|k| !kinds::all().any(|f| f.kind == *k && f.namespaced != Namespaced::Never))
            .collect();
        assert!(
            leftover.is_empty(),
            "source declared for a Kind that does not exist or is not namespaced: {}",
            leftover.join(", ")
        );
    }

    /// A `Via` has to name Kinds that are THEMSELVES scanned, or the chain ends
    /// in nothing and the entry reads as coverage while covering zero. Reads the
    /// prefix up to the em dash, which is where the table writes the target
    /// before the reason.
    ///
    /// Requires EVERY named target to be backed by a `Store`, not just one: a
    /// `Workload` lowers to a Container *or* a Vm, and covering one of the two
    /// leaves the other invisible.
    #[test]
    fn every_via_points_at_a_scanned_kind() {
        for (kind, src) in NAMESPACE_SOURCES {
            let NsSource::Via(texto) = src else { continue };
            let targets = texto.split('—').next().unwrap_or("").trim();
            assert!(
                !targets.is_empty(),
                "{kind}: a `Via` source has to name the Kind before the em dash"
            );
            for target in targets.split('/') {
                let target = target.trim();
                let scanned = NAMESPACE_SOURCES
                    .iter()
                    .any(|(o, s)| *o == target && matches!(s, NsSource::Store(_)));
                assert!(
                    scanned,
                    "{kind} says the namespace travels to {target:?}, \
                     which no `Store` source scans"
                );
            }
        }
    }
}

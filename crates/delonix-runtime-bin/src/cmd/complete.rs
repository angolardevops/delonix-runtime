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

/// Network storages (NFS/CIFS/WebDAV). They ARE volumes with a network driver
/// — the same filter `storage ls` applies, so `storage rm <TAB>` never offers
/// a local volume that `storage rm` would refuse.
pub fn storages() -> Vec<CompletionCandidate> {
    let Ok(store) = delonix_volume::VolumeStore::open(state_root()) else {
        return Vec::new();
    };
    cands(
        store
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|v| delonix_volume::is_network_driver(&v.driver))
            .map(|v| v.name),
    )
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

/// Isolation namespaces IN USE — derived from the containers and VMs that
/// carry them (a namespace has no record of its own; it exists while something
/// is in it). `default` is always offered: it is where everything lands, and a
/// node with nothing running would otherwise complete nothing at all.
pub fn namespaces() -> Vec<CompletionCandidate> {
    let mut nomes: Vec<String> = vec!["default".to_string()];
    if let Ok(store) = delonix_runtime_core::Store::open(state_root().join("containers")) {
        nomes.extend(
            store
                .list()
                .unwrap_or_default()
                .into_iter()
                .map(|c| c.namespace),
        );
    }
    nomes.extend(
        delonix_vm::list(&state_root())
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.namespace),
    );
    nomes.sort();
    nomes.dedup();
    cands(nomes)
}

/// What `cluster kube generate` accepts: a container name OR a pod name (it
/// resolves either, emitting one k8s Pod manifest). Two lists, one namespace of
/// names — offering only half of them is what sent users to `pod ls` by hand.
pub fn containers_or_pods() -> Vec<CompletionCandidate> {
    let mut all = containers();
    all.extend(pods());
    all
}

/// Registries this node is logged in to (`<root>/auth.json`) — the only ones
/// `image logout` has anything to remove.
pub fn registries() -> Vec<CompletionCandidate> {
    cands(delonix_image::auth::hosts(&state_root()))
}

//! Autocompletion candidates for RESOURCE NAMES — containers, images,
//! volumes, networks, VMs, clusters.
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

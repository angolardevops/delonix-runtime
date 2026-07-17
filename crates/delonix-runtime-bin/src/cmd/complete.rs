//! Candidatos de autocompletion para NOMES DE RECURSOS — containers, imagens,
//! volumes, redes, VMs, clusters.
//!
//! O `clap_complete` dinâmico já completa comandos/subcomandos/flags a partir da
//! definição do `Cli`. O que faltava era o TAB sobre os argumentos: `delonix
//! container stop <TAB>` não sugeria nada, e o utilizador tinha de ir a um
//! `container ls` copiar o nome à mão — precisamente o que o docker/podman
//! poupam.
//!
//! # Porque é barato fazer isto aqui (e não seria num cliente remoto)
//!
//! Cada candidato sai de um Store LOCAL em disco (`$DELONIX_ROOT/…`), lido
//! directamente. Não há daemon a contactar nem rede pelo meio, por isso um TAB
//! custa uma leitura de directório. (Um cliente HTTP do PaaS não podia fazer o
//! mesmo sem uma chamada de rede por cada TAB — é por isso que o `delonixctl`
//! deliberadamente não completa nomes.)
//!
//! # Regra: falhar em SILÊNCIO
//!
//! Um completer NUNCA pode escrever no terminal nem entrar em pânico — está a
//! correr no meio da linha de comandos do utilizador, a cada TAB. Se o store
//! não abre (raiz inexistente, permissões, estado a meio de uma escrita), a
//! resposta certa é "não tenho sugestões", não um erro no meio do prompt. Daí o
//! `unwrap_or_default()` em todo o lado.

use clap_complete::engine::CompletionCandidate;

use super::util::state_root;

fn cands<I: IntoIterator<Item = String>>(nomes: I) -> Vec<CompletionCandidate> {
    nomes.into_iter().map(CompletionCandidate::new).collect()
}

/// Containers a correr **e** parados: o `start`/`rm` querem os parados, o
/// `exec`/`logs` os vivos. Filtrar por estado aqui daria um TAB que "esconde" o
/// container que o utilizador está mesmo a tentar escrever.
pub fn containers() -> Vec<CompletionCandidate> {
    let Ok(store) = delonix_runtime_core::Store::open(state_root().join("containers")) else {
        return Vec::new();
    };
    cands(store.list().unwrap_or_default().into_iter().map(|c| c.name))
}

/// Imagens locais, pela sua referência legível (sem o `@sha256:…` quando há
/// tag — ver `output::display_ref`; um digest de 71 chars não se completa com
/// TAB, escreve-se).
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
    cands(delonix_vm::list(&state_root()).unwrap_or_default().into_iter().map(|v| v.name))
}

/// Clusters do modo kind — derivados da label dos nós, que é a fonte de verdade
/// (não há registo de "cluster" à parte; ver `cmd::kindmode::list`).
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

/// Nomes dos segredos do cofre.
pub fn secrets() -> Vec<CompletionCandidate> {
    let Ok(store) = delonix_runtime_core::SecretStore::open(state_root()) else {
        return Vec::new();
    };
    cands(store.list().into_iter().map(|s| s.name))
}

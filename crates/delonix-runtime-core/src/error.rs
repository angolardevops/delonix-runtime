//! O tipo de erro partilhado por todo o Delonix Engine.

use thiserror::Error;

/// Erros do Delonix Engine.
#[derive(Debug, Error)]
pub enum Error {
    /// Falha de I/O (ler/escrever estado, cgroups, `/proc`).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Falha a serialisar/desserialisar estado (JSON).
    #[error("state serialisation error: {0}")]
    Json(#[from] serde_json::Error),

    /// Uma chamada ao sistema (`clone`, `mount`, `setns`, ...) falhou.
    #[error("system call `{context}` failed: {message}")]
    Runtime {
        /// O nome da operação que falhou.
        context: &'static str,
        /// A mensagem do `errno` subjacente.
        message: String,
    },

    /// Não existe nenhum container com o id/nome dado.
    #[error("no such container: {0}")]
    NotFound(String),

    /// Não existe nenhuma VM com o nome dado. Variante própria porque o
    /// [`Error::NotFound`] partilhado diz "no such container" — num
    /// `vm stop`/`vm rm` isso confundia (o utilizador nem mexeu em containers).
    #[error("no such VM: {0} (see `delonix vm ls`)")]
    VmNotFound(String),

    /// O container existe mas não está em execução.
    #[error("container is not running: {0}")]
    NotRunning(String),

    /// Argumento inválido.
    #[error("invalid argument: {0}")]
    Invalid(String),

    /// Falha ao falar com um registo de imagens OCI (Docker Hub, ghcr.io, ...).
    #[error("registry error: {0}")]
    Registry(String),

    /// O estado desejado entra em conflito com o estado actual (ex.: já existe um
    /// recurso com o mesmo nome mas de outro `kind`).
    #[error("conflict: {0}")]
    Conflict(String),
}

/// Alias de conveniência.
pub type Result<T> = std::result::Result<T, Error>;

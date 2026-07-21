//! The error type shared by the whole Delonix Engine.

use thiserror::Error;

/// Delonix Engine errors.
#[derive(Debug, Error)]
pub enum Error {
    /// I/O failure (read/write state, cgroups, `/proc`).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Failure to serialise/deserialise state (JSON).
    #[error("state serialisation error: {0}")]
    Json(#[from] serde_json::Error),

    /// A system call (`clone`, `mount`, `setns`, ...) failed.
    #[error("system call `{context}` failed: {message}")]
    Runtime {
        /// The name of the operation that failed.
        context: &'static str,
        /// The message of the underlying `errno`.
        message: String,
    },

    /// There is no container with the given id/name.
    #[error("no such container: {0}")]
    NotFound(String),

    /// There is no VM with the given name. Its own variant because the
    /// shared [`Error::NotFound`] says "no such container" — in a
    /// `vm stop`/`vm rm` that was confusing (the user didn't even touch containers).
    #[error("no such VM: {0} (see `delonix vm ls`)")]
    VmNotFound(String),

    /// The container exists but is not running.
    #[error("container is not running: {0}")]
    NotRunning(String),

    /// Invalid argument.
    #[error("invalid argument: {0}")]
    Invalid(String),

    /// Failure to talk to an OCI image registry (Docker Hub, ghcr.io, ...).
    #[error("registry error: {0}")]
    Registry(String),

    /// The desired state conflicts with the current state (e.g.: a resource with
    /// the same name but of a different `kind` already exists).
    #[error("conflict: {0}")]
    Conflict(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

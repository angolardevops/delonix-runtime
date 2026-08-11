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

    /// A resource does not exist. BUG FOUND (code review, live testing):
    /// this Display hardcoded "no such container: {0}" regardless of what
    /// actually failed — `delonix secret rm <missing>` really did print "no
    /// such container: secret X" (confirmed live). `Error::VmNotFound`
    /// already existed as a workaround for this exact confusion, but only
    /// for VMs; the SAME shared `NotFound` is also thrown by
    /// `SecretStore`/`VolumeStore`/`NetworkStore`/`ImageStore`/etc. Every
    /// one of THOSE call sites already embeds its own resource-type prefix
    /// into the string (`"secret {name}"`, `"network {name}"`, `"volume
    /// {name}"`, ...) — only `Store<Container>`'s two call sites relied on
    /// this Display's hardcoded wording instead. Fixed at the root: the
    /// Display is now generic, and `Store<Container>` supplies its own
    /// "container " prefix like everyone else already did.
    #[error("no such {0}")]
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
    ///
    /// Until exit codes were classified this variant had **zero producers** —
    /// `delonix-mgmt` matched on it (409) and nothing ever built one, while the
    /// real "already exists" refusals said `Error::Invalid` and came back as a
    /// 400 and a generic exit 1. Publishing an exit code for a variant nobody
    /// constructs would have been a number that can never be observed, so the
    /// refusals were moved to where they belonged instead (`NetworkStore`'s
    /// three `create*`, `volumes snapshot create`). Anything new that refuses
    /// because the name is TAKEN belongs here, not in `Invalid`: the caller's
    /// next move is different (adopt/skip vs. fix the argument).
    #[error("conflict: {0}")]
    Conflict(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

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

    /// A capability this host does not have: a tool that is not installed, a
    /// backend that is not available, a kernel feature that is off.
    ///
    /// Its own variant because the caller's next move is different from every
    /// other failure — nothing about the ARGUMENTS is wrong, and retrying
    /// changes nothing. Somebody has to install something. Before this existed
    /// these refusals said [`Error::Invalid`] and came back as a generic exit
    /// 1, indistinguishable from a typo in a flag: `wg` missing,
    /// `virt-customize` missing, `ngrok`/`cloudflared` not in `PATH`.
    ///
    /// **The message must name the tool or feature and how to get it.** The raw
    /// `ENOENT` of a spawn is NOT a missing file — "No such file or directory"
    /// sends the reader looking for a path that was never the problem.
    #[error("unavailable: {0}")]
    Unavailable(String),

    /// The operation was still not done when its deadline passed.
    ///
    /// Distinct from a failure: nothing said no, and the work may well be
    /// finishing right now. A reconciler waits longer or comes back; it must
    /// not read this as «it broke» and recreate the resource on top of one that
    /// is still coming up.
    #[error("timed out: {0}")]
    Timeout(String),
}

impl Error {
    /// The stable, machine-readable identity of a failure.
    ///
    /// The pair of the exit code, for callers that read text rather than `$?`:
    /// an HTTP client, a `-o json` consumer, a log pipeline. It exists for the
    /// same reason the numbers do — the MESSAGE is translated, so a caller that
    /// greps it works on the machine it was written on and silently stops
    /// classifying on a node with another locale.
    ///
    /// **The granularity deliberately matches the exit-code classification, not
    /// the variant list.** `NotFound` and `VmNotFound` share a code because they
    /// are one question for a caller — «it is not there» — exactly as they
    /// share exit code 4. A finer split here would be a distinction the numbers
    /// do not make, and the two classifications would drift apart.
    ///
    /// **These strings are a contract.** A code may be ADDED; an existing one
    /// never changes spelling and never changes meaning. Renaming one is the
    /// same breakage as renaming an exit code, with none of the visibility.
    ///
    /// The `match` is exhaustive on purpose, like `cmd::exitcode::for_error`'s:
    /// a variant added tomorrow stops the build here instead of being filed
    /// under a catch-all nobody ever revisits.
    pub fn code(&self) -> &'static str {
        match self {
            Error::NotFound(_) | Error::VmNotFound(_) => "DX_NOT_FOUND",
            Error::NotRunning(_) => "DX_NOT_RUNNING",
            Error::Conflict(_) => "DX_CONFLICT",
            Error::Unavailable(_) => "DX_UNAVAILABLE",
            Error::Timeout(_) => "DX_TIMEOUT",
            Error::Invalid(_) => "DX_INVALID_ARGUMENT",
            Error::Registry(_) => "DX_REGISTRY",
            Error::Runtime { .. } => "DX_SYSCALL_FAILED",
            Error::Json(_) => "DX_INVALID_STATE",
            Error::Io(_) => "DX_IO",
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

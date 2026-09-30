//! The error type shared by the whole Delonix Engine, and the `DX_*` code of each
//! class (ADR-0040 D2.1: the foundation owns the codes every context maps to).
//!
//! It lives in the pure model, the bottom of the dependency graph, so every layer
//! names the same type.

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

    /// A provider refused the credential or the privilege it was sent: an
    /// API that answers 401, or 302 to a login page (OPNsense does both,
    /// ADR-0051 phase 0), or 403 for a key without the right. Distinct from
    /// [`Error::Io`] with `PermissionDenied`, which is the local
    /// filesystem's EACCES: printing «I/O error» for a refused API key names
    /// a cause that was never the problem (ADR-0059 D5).
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// A failure with its own entry in the code dictionary (ADR-0043 D4).
    ///
    /// A crate's own error converts into the class it belongs to and wraps it
    /// here with its specific number, so the number travels to whoever prints or
    /// reports it. It shows, classifies and exits exactly as `inner` does; only
    /// [`Error::number`] is finer. Built with [`Error::coded`], which refuses a
    /// number whose class digit is not `inner`'s class.
    #[error("{inner}")]
    Coded {
        /// The dictionary number, `CDNN`.
        number: u16,
        /// The class this failure belongs to, with its message.
        inner: Box<Error>,
        /// Where it happened, when the raiser knew (ADR-0059 D5). `None` for
        /// every failure that nobody gave a context to.
        context: Option<Box<ErrorContext>>,
    },
}

/// The context of a failure, as ADR-0059 D5's envelope carries it next to the
/// code: which provider, in which role, at which step of the operation, and
/// the provider's own message. Every field is optional; an absent one is
/// omitted from the envelope, never filled with a guess.
///
/// `cause` is the provider's message **redacted by the provider** that
/// produced it — only the client that holds a credential knows which strings
/// are secret, so nothing downstream can redact it later.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorContext {
    /// The provider id (`opnsense`, `proxmox`).
    pub provider: Option<String>,
    /// The network role the provider was serving (`segment`, `gateway`).
    pub role: Option<String>,
    /// The catalog capability that was asked for (`net.gateway.alias`).
    pub capability: Option<String>,
    /// The operation step that failed (`ensure_alias`, `commit`).
    pub step: Option<String>,
    /// The digest of the plan that was being applied (ADR-0059 F4).
    pub plan_digest: Option<String>,
    /// The provider's message, redacted.
    pub cause: Option<String>,
}

impl ErrorContext {
    /// True when no field is set.
    pub fn is_empty(&self) -> bool {
        *self == ErrorContext::default()
    }

    /// Fills every field of `self` that is empty from `other`: a context set
    /// closer to the failure wins over one added further up.
    pub(crate) fn fill_from(&mut self, other: ErrorContext) {
        let fill = |mine: &mut Option<String>, theirs: Option<String>| {
            if mine.is_none() {
                *mine = theirs;
            }
        };
        fill(&mut self.provider, other.provider);
        fill(&mut self.role, other.role);
        fill(&mut self.capability, other.capability);
        fill(&mut self.step, other.step);
        fill(&mut self.plan_digest, other.plan_digest);
        fill(&mut self.cause, other.cause);
    }
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
            Error::Coded { inner, .. } => inner.code(),
            Error::NotFound(_) | Error::VmNotFound(_) => "DX_NOT_FOUND",
            Error::NotRunning(_) => "DX_NOT_RUNNING",
            Error::Conflict(_) => "DX_CONFLICT",
            Error::Unavailable(_) => "DX_UNAVAILABLE",
            Error::Timeout(_) => "DX_TIMEOUT",
            Error::PermissionDenied(_) => "DX_PERMISSION_DENIED",
            Error::Invalid(_) => "DX_INVALID_ARGUMENT",
            Error::Registry(_) => "DX_REGISTRY",
            Error::Runtime { .. } => "DX_SYSCALL_FAILED",
            Error::Json(_) => "DX_INVALID_STATE",
            // The KIND decides, as it does for the exit code: one DX_ spanning
            // two numbers would make `$?` and the text contradict each other for
            // the SAME failure, which is what the `exitcode` invariant forbids.
            Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                "DX_PERMISSION_DENIED"
            }
            Error::Io(_) => "DX_IO",
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// `text` with every one of `secrets` replaced by `<redacted>`.
///
/// For a client that holds a credential and puts a remote answer into an
/// error: only it knows which strings are secret, so it redacts before the
/// text leaves (ADR-0059 D5, the `cause`; ADR-0049's rule that no rendered
/// error carries the secret). A secret shorter than 4 bytes is skipped:
/// replacing every `a` of a message makes it unreadable and hides nothing.
pub fn redact_known(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if s.len() >= 4 && out.contains(s) {
            out = out.replace(s, "<redacted>");
        }
    }
    out
}

impl Error {
    /// Maps a failed read of a record file WITHOUT lying about why.
    ///
    /// `std::fs::read(...).map_err(|_| Error::NotFound(...))` is right for
    /// `ENOENT` and wrong for everything else: a permission denied, an I/O
    /// error or a corrupt mount all came out as "no such X". The operator then
    /// goes and `ls` a file that is sitting right there — measured shape across
    /// nine call sites, and in one of them the code had already PROVED the file
    /// existed one line above.
    ///
    /// `describe` is lazy so the common path builds no string.
    pub fn not_found_or_io(e: std::io::Error, describe: impl FnOnce() -> String) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound(describe())
        } else {
            Error::Io(e)
        }
    }
}

#[cfg(test)]
mod not_found_or_io_tests {
    use super::Error;
    use std::io::{Error as IoError, ErrorKind};

    #[test]
    fn a_missing_file_keeps_the_callers_wording() {
        let e = Error::not_found_or_io(IoError::from(ErrorKind::NotFound), || {
            "network blue".to_string()
        });
        assert!(matches!(e, Error::NotFound(ref s) if s == "network blue"));
        assert_eq!(e.to_string(), "no such network blue");
    }

    /// The whole point: this used to say "no such network blue" for a file that
    /// is sitting right there and cannot be read, sending the operator to look
    /// for something they would find.
    #[test]
    fn a_permission_error_is_not_reported_as_absence() {
        let e = Error::not_found_or_io(IoError::from(ErrorKind::PermissionDenied), || {
            "network blue".to_string()
        });
        assert!(matches!(e, Error::Io(_)), "got {e:?}");
        assert!(!e.to_string().contains("no such"), "{e}");
        assert!(e.to_string().contains("permission"), "{e}");
    }

    #[test]
    fn the_description_is_not_built_when_the_file_is_merely_unreadable() {
        let mut built = false;
        let _ = Error::not_found_or_io(IoError::from(ErrorKind::PermissionDenied), || {
            built = true;
            String::new()
        });
        assert!(
            !built,
            "the common path must not pay for a string it discards"
        );
    }
}

//! The cloud-init seed generator's own failures, and the dictionary number of
//! each group (ADR-0043).
//!
//! **Scoped to this module, not the whole crate.** `delonix-vm`'s two VM
//! backends (Cloud Hypervisor, libvirt) still build the shared error directly
//! — this is the first slice, not the whole crate; the rest is a larger,
//! separate change. The messages are a contract with what was there before:
//! each variant carries the text the call site used to build by hand and
//! converts into the same shared class, wrapped with its number.

use thiserror::Error;

/// A failure of [`super::generate_seed_iso`].
#[derive(Debug, Error)]
pub enum Error {
    /// A VM name that fails the same validation `create()` enforces — checked
    /// again here because this function writes to disk before `create()` ever
    /// runs. Shares [`crate::Error::InvalidName`]'s number (DX-1507): it is
    /// the same failure, just caught from a different call site.
    #[error("{0}")]
    InvalidName(String),

    /// A `--user-data` override that could not be copied into place.
    #[error("{0}")]
    UserDataCopyFailed(String),

    /// `cloud-localds` is not on `PATH`.
    #[error("{0}")]
    ToolMissing(String),

    /// `cloud-localds` could not even be spawned (not an ENOENT — that is
    /// [`Error::ToolMissing`]).
    #[error("{0}")]
    ToolSpawnFailed(String),

    /// `cloud-localds` ran and exited non-zero.
    #[error("{0}")]
    ToolExitFailed(String),

    /// A failure of the layers underneath (filesystem, JSON, state), with its own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Engine(e.into())
    }
}

/// The shared class each failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043). A failure of the
    /// layers underneath keeps its own.
    pub fn number(&self) -> u16 {
        match self {
            Error::InvalidName(_) => 1507,
            Error::UserDataCopyFailed(_) => 1516,
            Error::ToolMissing(_) => 6504,
            Error::ToolSpawnFailed(_) => 9508,
            Error::ToolExitFailed(_) => 9509,
            Error::Engine(e) => e.number(),
        }
    }
}

impl From<Error> for Dx {
    fn from(e: Error) -> Self {
        let number = e.number();
        let class = match e {
            Error::ToolMissing(text) => Dx::Unavailable(text),
            Error::ToolSpawnFailed(text) | Error::ToolExitFailed(text) => Dx::Runtime {
                context: "cloud-localds",
                message: text,
            },
            Error::Engine(e) => return e,
            e => Dx::Invalid(e.to_string()),
        };
        Dx::coded(number, class)
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    fn every_variant() -> Vec<Error> {
        vec![
            Error::InvalidName("invalid VM name: \"../x\"".into()),
            Error::UserDataCopyFailed("could not copy user-data '/x': not found".into()),
            Error::ToolMissing("cloud-localds not found — install it".into()),
            Error::ToolSpawnFailed("running cloud-localds: Permission denied".into()),
            Error::ToolExitFailed("cloud-localds failed (exit Some(1))".into()),
            Error::Engine(delonix_model::Error::Conflict("x".into())),
        ]
    }

    #[test]
    fn every_failure_keeps_its_number_through_the_conversion() {
        for e in every_variant() {
            let number = e.number();
            let shown = e.to_string();
            let converted = delonix_model::Error::from(e);
            assert_eq!(converted.number(), number, "{shown}");
            assert!(
                delonix_model::codes::lookup(number).is_some(),
                "DX-{number:04} ({shown}) has no dictionary entry"
            );
        }
    }

    /// The text the CLI printed before this type existed: the shared class
    /// wraps the local variant's message verbatim.
    #[test]
    fn the_converted_message_is_the_one_printed_before() {
        let cases = [
            (
                Error::InvalidName("invalid VM name: \"../x\"".into()),
                "invalid argument: invalid VM name: \"../x\"",
            ),
            (
                Error::ToolMissing("cloud-localds not found — install it".into()),
                "unavailable: cloud-localds not found — install it",
            ),
            (
                Error::ToolSpawnFailed("running cloud-localds: Permission denied".into()),
                "system call `cloud-localds` failed: running cloud-localds: Permission denied",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(delonix_model::Error::from(e).to_string(), want);
        }
    }
}

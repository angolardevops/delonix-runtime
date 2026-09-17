//! The state layer's own failures, and the dictionary number of each (ADR-0043).
//!
//! **The messages are a contract with what was there before.** Each variant
//! converts into the [`delonix_model::Error`] class the call site used to build by
//! hand, with the same text, wrapped with its number — so the CLI prints byte for
//! byte what it printed and exits with the same code.

use std::path::PathBuf;
use thiserror::Error;

/// A failure of the state layer.
#[derive(Debug, Error)]
pub enum Error {
    /// No container record answers to that id, prefix or name.
    #[error("container: {0}")]
    NoSuchContainer(String),

    /// A container name that exists in more than one namespace.
    #[error(
        "container name '{name}' exists in several namespaces ({options}) — qualify it as \
         <namespace>/<name>"
    )]
    AmbiguousContainer {
        /// The name as asked.
        name: String,
        /// The qualified candidates, comma-separated.
        options: String,
    },

    /// No record under that key in a JSON store.
    #[error("{0}")]
    NoSuchRecord(String),

    /// No secret of that name.
    #[error("secret {0}")]
    NoSuchSecret(String),

    /// A secret name outside `[a-z0-9._-]`.
    #[error("invalid secret name: {0:?}")]
    InvalidSecretName(String),

    /// A secret key that is not a valid environment variable name.
    #[error("invalid env key: {0:?}")]
    InvalidEnvKey(String),

    /// A credential name outside `[a-z0-9._:-]`.
    #[error("invalid credential name: {0}")]
    InvalidCredentialName(String),

    /// The host's master key has the wrong length.
    #[error("corrupted master key at {}", .0.display())]
    CorruptMasterKey(PathBuf),

    /// The vault could not encrypt or decrypt; the text says which and what.
    #[error("{0}")]
    Vault(String),

    /// The per-store lock could not be taken, so the read-modify-write is refused.
    #[error("{0}")]
    Lock(String),

    /// The kernel gave no entropy.
    #[error("{0}")]
    Entropy(String),

    /// A failure of the layers underneath (the filesystem, JSON), with its own class.
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

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Engine(e.into())
    }
}

/// The shared class each state failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043). A failure of the layers
    /// underneath keeps its own.
    pub fn number(&self) -> u16 {
        match self {
            Error::NoSuchContainer(_) => 4101,
            Error::AmbiguousContainer { .. } => 1101,
            Error::NoSuchRecord(_) => 4000,
            Error::NoSuchSecret(_) => 4801,
            Error::InvalidSecretName(_) => 1801,
            Error::InvalidEnvKey(_) => 1802,
            Error::InvalidCredentialName(_) => 1803,
            Error::CorruptMasterKey(_) => 1804,
            Error::Vault(_) => 1805,
            Error::Lock(_) => 9003,
            Error::Entropy(_) => 9004,
            Error::Engine(e) => e.number(),
        }
    }

    /// «No such resource» — asked of the class, like the shared error.
    pub fn is_not_found(&self) -> bool {
        self.number() / 1000 == 4
    }

    /// The shared class this failure converts into, for a caller that needs a
    /// variant's payload (`match e.into_root() { Error::NotFound(n) => … }`).
    pub fn into_root(self) -> delonix_model::Error {
        delonix_model::Error::from(self).into_root()
    }

    /// «An argument is wrong».
    pub fn is_invalid_argument(&self) -> bool {
        self.number() / 1000 == 1
    }
}

impl From<Error> for Dx {
    fn from(e: Error) -> Self {
        let number = e.number();
        let class = match e {
            Error::NoSuchContainer(_) | Error::NoSuchRecord(_) | Error::NoSuchSecret(_) => {
                Dx::NotFound(e.to_string())
            }
            Error::Lock(message) => Dx::Runtime {
                context: "state lock",
                message,
            },
            Error::Entropy(message) => Dx::Runtime {
                context: "getrandom",
                message,
            },
            Error::Engine(e) => return e,
            e => Dx::Invalid(e.to_string()),
        };
        // `NoSuchRecord` is the store not knowing what the record is: the class
        // entry, not a coded one.
        if number.is_multiple_of(1000) {
            return class;
        }
        Dx::coded(number, class)
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    fn every_variant() -> Vec<Error> {
        vec![
            Error::NoSuchContainer("web".into()),
            Error::AmbiguousContainer {
                name: "db".into(),
                options: "a/db, b/db".into(),
            },
            Error::NoSuchRecord("dev".into()),
            Error::NoSuchSecret("tok".into()),
            Error::InvalidSecretName("../x".into()),
            Error::InvalidEnvKey("1A".into()),
            Error::InvalidCredentialName("../x".into()),
            Error::CorruptMasterKey("/k".into()),
            Error::Vault("failed to encrypt".into()),
            Error::Lock("flock failed".into()),
            Error::Entropy("no entropy".into()),
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

    /// The text the CLI printed before this type existed.
    #[test]
    fn the_converted_message_is_the_one_printed_before() {
        let cases = [
            (
                Error::NoSuchContainer("web".into()),
                "no such container: web",
            ),
            (Error::NoSuchSecret("tok".into()), "no such secret tok"),
            (Error::NoSuchRecord("dev".into()), "no such dev"),
            (
                Error::InvalidSecretName("a b".into()),
                "invalid argument: invalid secret name: \"a b\"",
            ),
            (
                Error::CorruptMasterKey("/k".into()),
                "invalid argument: corrupted master key at /k",
            ),
            (
                Error::Lock("flock on /x failed".into()),
                "system call `state lock` failed: flock on /x failed",
            ),
            (
                Error::Entropy("boom".into()),
                "system call `getrandom` failed: boom",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(delonix_model::Error::from(e).to_string(), want);
        }
    }
}

//! The volume store's own failures, and the `DX_*` class each one belongs to
//! (ADR-0040 P3: errors per crate, mapped to the codes the foundation owns).
//!
//! **The messages are a contract with what was there before.** Each variant
//! converts into the [`delonix_model::Error`] class the call site used to build by
//! hand, with the same text, so the CLI prints byte for byte what it printed and
//! exits with the same code — now with its own dictionary number (`DX-12NN`,
//! `DX-42NN`, `DX-9201`), which the carrier checks against the class.

use thiserror::Error;

/// A failure of the volume store.
#[derive(Debug, Error)]
pub enum Error {
    /// A name outside the characters a volume may carry.
    #[error("invalid volume name: {0:?}")]
    InvalidName(String),

    /// A network driver asked for with no device to mount.
    #[error("{0} volume requires a device (the mount target)")]
    MissingDevice(String),

    /// A registered network volume whose record lost its device.
    #[error("{driver} volume '{name}' has no device")]
    NoDevice {
        /// The volume's driver.
        driver: String,
        /// The volume's name.
        name: String,
    },

    /// No volume of that name in this scope.
    #[error("no such volume {0}")]
    NoSuchVolume(String),

    /// No snapshot of that name on that volume.
    #[error("no such snapshot {snap} of volume {volume}")]
    NoSuchSnapshot {
        /// The snapshot asked for.
        snap: String,
        /// The volume it was asked of.
        volume: String,
    },

    /// A snapshot name that could escape the snapshots directory.
    #[error("invalid snapshot name: '{0}' (use [a-zA-Z0-9._-], no '/' or '..')")]
    InvalidSnapshotName(String),

    /// A hard quota asked of a volume that already holds data.
    #[error(
        "hard quota (loopback) only on an empty volume; create with --quota or empty it first"
    )]
    QuotaOnNonEmpty,

    /// A quota below what the volume already uses.
    #[error("the new quota is smaller than the current usage — free up space first")]
    QuotaBelowUsage,

    /// A shrink that needs the volume unmounted while something holds it.
    #[error("volume in use — stop the containers to shrink the quota")]
    InUse,

    /// The same name is both a namespaced share and a global volume.
    #[error(
        "volume {src:?} is ambiguous in namespace {namespace:?}: a namespaced volume and a \
         global one of the same name both exist. Rename one, or finish the migration with \
         `delonix sharevolume migrate`"
    )]
    Ambiguous {
        /// The name as written in the spec.
        src: String,
        /// The workload's namespace.
        namespace: String,
    },

    /// A `-v` spec that is not `source:/target[:options]`.
    #[error("invalid volume spec: {0:?} (use source:/target[:ro])")]
    InvalidSpec(String),

    /// A bind option this engine does not implement.
    #[error(
        "unsupported bind option ':{0}' — supported: ':ro'/':rw', ':rprivate'/':rslave'/':rshared' \
         (and 'ro,<propagation>'); SELinux ':z'/':Z' and ':U' are not implemented"
    )]
    UnsupportedBindOption(String),

    /// A mount target that is not absolute.
    #[error("target must be absolute: {0:?}")]
    RelativeTarget(String),

    /// A host tool (`mount`, `mkfs.ext4`, `resize2fs`…) failed or could not run.
    #[error("system call `{context}` failed: {message}")]
    Command {
        /// What was being done.
        context: &'static str,
        /// The tool's own error.
        message: String,
    },

    /// A failure of the layers underneath (the filesystem, the state records),
    /// passed through with its own class.
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

impl From<delonix_state::Error> for Error {
    fn from(e: delonix_state::Error) -> Self {
        Error::Engine(e.into())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Engine(e.into())
    }
}

/// The shared class each volume failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043): `DX-12NN` for a wrong
    /// input, `DX-42NN` for something that is not there, `DX-9201` for a host tool
    /// that failed. A failure of the layers underneath keeps its own.
    pub fn number(&self) -> u16 {
        match self {
            Error::InvalidName(_) => 1201,
            Error::MissingDevice(_) => 1202,
            Error::NoDevice { .. } => 1203,
            Error::InvalidSnapshotName(_) => 1204,
            Error::QuotaOnNonEmpty => 1205,
            Error::QuotaBelowUsage => 1206,
            Error::InUse => 1207,
            Error::Ambiguous { .. } => 1208,
            Error::InvalidSpec(_) => 1209,
            Error::UnsupportedBindOption(_) => 1210,
            Error::RelativeTarget(_) => 1211,
            Error::NoSuchVolume(_) => 4201,
            Error::NoSuchSnapshot { .. } => 4202,
            Error::Command { .. } => 9201,
            Error::Engine(e) => e.number(),
        }
    }
}

impl From<Error> for Dx {
    fn from(e: Error) -> Self {
        let number = e.number();
        let class = match e {
            Error::NoSuchVolume(name) => Dx::NotFound(format!("volume {name}")),
            Error::NoSuchSnapshot { snap, volume } => {
                Dx::NotFound(format!("snapshot {snap} of volume {volume}"))
            }
            Error::Command { context, message } => Dx::Runtime { context, message },
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
            Error::InvalidName("a/b".into()),
            Error::MissingDevice("nfs".into()),
            Error::NoDevice {
                driver: "nfs".into(),
                name: "v".into(),
            },
            Error::NoSuchVolume("v".into()),
            Error::NoSuchSnapshot {
                snap: "s".into(),
                volume: "v".into(),
            },
            Error::InvalidSnapshotName("..".into()),
            Error::QuotaOnNonEmpty,
            Error::QuotaBelowUsage,
            Error::InUse,
            Error::Ambiguous {
                src: "v".into(),
                namespace: "t".into(),
            },
            Error::InvalidSpec("x".into()),
            Error::UnsupportedBindOption("z".into()),
            Error::RelativeTarget("x".into()),
            Error::Command {
                context: "quota",
                message: "boom".into(),
            },
            Error::Engine(delonix_model::Error::Conflict("x".into())),
        ]
    }

    /// Every variant converts into a class its number belongs to (the carrier
    /// panics otherwise), keeps that number, and has an entry in the dictionary.
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
                Error::InvalidName("a/b".into()),
                "invalid argument: invalid volume name: \"a/b\"",
            ),
            (Error::NoSuchVolume("db".into()), "no such volume db"),
            (
                Error::NoSuchSnapshot {
                    snap: "s1".into(),
                    volume: "db".into(),
                },
                "no such snapshot s1 of volume db",
            ),
            (
                Error::NoDevice {
                    driver: "nfs".into(),
                    name: "db".into(),
                },
                "invalid argument: nfs volume 'db' has no device",
            ),
            (
                Error::Command {
                    context: "quota",
                    message: "loop device not found".into(),
                },
                "system call `quota` failed: loop device not found",
            ),
            (
                Error::RelativeTarget("data".into()),
                "invalid argument: target must be absolute: \"data\"",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(delonix_model::Error::from(e).to_string(), want);
        }
    }
}

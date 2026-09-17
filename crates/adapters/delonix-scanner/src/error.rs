//! The scanner's own failures, and the `DX_*` class each one belongs to
//! (ADR-0040 P3: errors per crate, mapped to the codes the foundation owns).
//!
//! **The messages are a contract with what was there before.** Each variant
//! converts into the [`delonix_model::Error`] class these call sites used to build
//! by hand, carrying the same text, so the CLI prints byte for byte what it
//! printed before this type existed — now with its own dictionary number
//! (`DX-14NN`), which the carrier checks against the class.

use thiserror::Error;

/// A failure of the scanner.
#[derive(Debug, Error)]
pub enum Error {
    /// The image has no package database this scanner reads.
    #[error("empty SBOM: no apk/dpkg package database in the image")]
    EmptySbom,

    /// The embedded or synced advisory database is not valid JSON.
    #[error("advisories: {0}")]
    AdvisoryDb(serde_json::Error),

    /// An OSV feed that is not JSON at all.
    #[error("invalid OSV feed: {0}")]
    OsvNotJson(serde_json::Error),

    /// An OSV feed that is JSON of the wrong shape.
    #[error("OSV feed: expected an array or a {{\"vulns\":[…]}} object")]
    OsvShape,

    /// A directory named as an Odoo module has no manifest.
    #[error("'{0}' is not an Odoo module (no __manifest__.py)")]
    NotAModule(String),

    /// A tree with no Odoo module in it.
    #[error("no Odoo module found (directories with __manifest__.py)")]
    NoModule,

    /// Reading a module tree failed.
    #[error("system call `module scan` failed: {0}")]
    ModuleScan(std::io::Error),

    /// A failure of the layers underneath (the image store, the filesystem),
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

/// The shared class each scanner failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043): `DX-14NN` for a wrong
    /// input to the scanner, `DX-9402` for a module tree that could not be read.
    /// A failure of the layers underneath keeps its own.
    pub fn number(&self) -> u16 {
        match self {
            Error::EmptySbom => 1401,
            Error::AdvisoryDb(_) => 1402,
            Error::OsvNotJson(_) => 1403,
            Error::OsvShape => 1404,
            Error::NotAModule(_) => 1405,
            Error::NoModule => 1406,
            Error::ModuleScan(_) => 9402,
            Error::Engine(e) => e.number(),
        }
    }
}

impl From<Error> for Dx {
    fn from(e: Error) -> Self {
        let number = e.number();
        let class = match e {
            Error::ModuleScan(io) => Dx::Runtime {
                context: "module scan",
                message: io.to_string(),
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
        let bad_json = || serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        vec![
            Error::EmptySbom,
            Error::AdvisoryDb(bad_json()),
            Error::OsvNotJson(bad_json()),
            Error::OsvShape,
            Error::NotAModule("m".into()),
            Error::NoModule,
            Error::ModuleScan(std::io::Error::other("x")),
            Error::Engine(delonix_model::Error::NotFound("image x".into())),
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

    /// The text the CLI printed before this type existed, for the variants
    /// that used to be built in place.
    #[test]
    fn the_converted_message_is_the_one_printed_before() {
        let cases = [
            (
                Error::EmptySbom,
                "invalid argument: empty SBOM: no apk/dpkg package database in the image",
            ),
            (
                Error::OsvShape,
                "invalid argument: OSV feed: expected an array or a {\"vulns\":[…]} object",
            ),
            (
                Error::NotAModule("web".into()),
                "invalid argument: 'web' is not an Odoo module (no __manifest__.py)",
            ),
            (
                Error::ModuleScan(std::io::Error::other("boom")),
                "system call `module scan` failed: boom",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(delonix_model::Error::from(e).to_string(), want);
        }
    }
}

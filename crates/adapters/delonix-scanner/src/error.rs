//! The scanner's own failures, and the `DX_*` class each one belongs to
//! (ADR-0040 P3: errors per crate, mapped to the codes the foundation owns).
//!
//! **The messages are a contract with what was there before.** Each variant
//! converts into the [`delonix_model::Error`] class these call sites used to build
//! by hand, carrying the same text, so the CLI prints byte for byte what it
//! printed before this type existed. The conversion is where a class is decided;
//! [`Error::code`] asks the conversion instead of repeating it, so the two cannot
//! drift apart.

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

impl From<Error> for Dx {
    fn from(e: Error) -> Self {
        match e {
            e @ (Error::EmptySbom
            | Error::AdvisoryDb(_)
            | Error::OsvNotJson(_)
            | Error::OsvShape
            | Error::NotAModule(_)
            | Error::NoModule) => Dx::Invalid(e.to_string()),
            Error::ModuleScan(io) => Dx::Runtime {
                context: "module scan",
                message: io.to_string(),
            },
            Error::Engine(e) => e,
        }
    }
}

impl Error {
    /// The `DX_*` code of this failure — the one the CLI exits with and the API
    /// reports. Decided by the conversion above, never a second table.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Engine(e) => e.code(),
            Error::ModuleScan(_) => "DX_SYSCALL_FAILED",
            _ => "DX_INVALID_ARGUMENT",
        }
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

    #[test]
    fn the_code_is_the_code_of_the_class_it_converts_into() {
        for e in every_variant() {
            let code = e.code();
            let shown = e.to_string();
            let class = delonix_model::Error::from(e);
            assert_eq!(code, class.code(), "{shown}");
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

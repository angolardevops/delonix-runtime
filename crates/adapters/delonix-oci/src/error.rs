//! The OCI adapter's own failures, grouped by what went wrong, and the dictionary
//! number of each group (ADR-0043).
//!
//! **The messages are a contract with what was there before.** Each variant carries
//! the text the call site used to build by hand and converts into the same shared
//! class, wrapped with its number — so the CLI prints byte for byte what it printed
//! and exits with the same code.

use thiserror::Error;

/// A failure of the OCI adapter.
#[derive(Debug, Error)]
pub enum Error {
    /// A Dockerfile/Delonixfile that does not parse or asks for something unsupported.
    #[error("{0}")]
    Dockerfile(String),

    /// An image archive (`docker save`/OCI layout) that is incomplete or corrupt.
    #[error("{0}")]
    Archive(String),

    /// A layer or a root filesystem that could not be packed or unpacked.
    #[error("{0}")]
    Layer(String),

    /// An image with no layers to mount.
    #[error("image has no layers")]
    NoLayers,

    /// A state root whose path cannot back an overlay mount.
    #[error("{0}")]
    UnusableStateRoot(String),

    /// A signing or verification key that is malformed or could not be made.
    #[error("{0}")]
    SigningKey(String),

    /// No signature exists for the image.
    #[error("{0}")]
    NotSigned(String),

    /// A signature that does not verify.
    #[error("{0}")]
    Signature(String),

    /// Whether the image is signed could not be determined.
    #[error("{0}")]
    SignatureUnknown(String),

    /// The image is already signed and `--force` was not given.
    #[error("{0}")]
    AlreadySigned(String),

    /// Producing the signature failed.
    #[error("{0}")]
    Signing(String),

    /// No image of that name locally or in the registry.
    #[error("image {0}")]
    NoSuchImage(String),

    /// A repository or image the registry does not show to these credentials.
    #[error("{0}")]
    NotVisible(String),

    /// The registry answered with an error, or could not be reached.
    #[error("{0}")]
    Registry(String),

    /// What the registry served does not hash to what was asked or declared.
    #[error("{0}")]
    DigestMismatch(String),

    /// Mounting the image's overlay failed.
    #[error("{0}")]
    OverlayMount(String),

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

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Engine(e.into())
    }
}

impl From<delonix_state::Error> for Error {
    fn from(e: delonix_state::Error) -> Self {
        Error::Engine(e.into())
    }
}

/// The shared class each OCI failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043). A failure of the layers
    /// underneath keeps its own.
    pub fn number(&self) -> u16 {
        match self {
            Error::Dockerfile(_) => 1407,
            Error::Archive(_) => 1408,
            Error::Layer(_) => 1409,
            Error::NoLayers => 1410,
            Error::UnusableStateRoot(_) => 1411,
            Error::SigningKey(_) => 1412,
            Error::NotSigned(_) => 1413,
            Error::Signature(_) => 1414,
            Error::SignatureUnknown(_) => 1415,
            Error::AlreadySigned(_) => 1416,
            Error::Signing(_) => 1417,
            Error::NoSuchImage(_) => 4401,
            Error::NotVisible(_) => 4402,
            Error::Registry(_) => 9401,
            Error::DigestMismatch(_) => 9403,
            Error::OverlayMount(_) => 9404,
            Error::Engine(e) => e.number(),
        }
    }

    /// «No such resource».
    pub fn is_not_found(&self) -> bool {
        self.number() / 1000 == 4
    }

    /// The shared class this failure converts into, for a caller that needs a
    /// variant's payload.
    pub fn into_root(self) -> Dx {
        Dx::from(self).into_root()
    }
}

impl From<Error> for Dx {
    fn from(e: Error) -> Self {
        let number = e.number();
        let class = match e {
            Error::NoSuchImage(_) => Dx::NotFound(e.to_string()),
            Error::NotVisible(text) => Dx::NotFound(text),
            Error::Registry(text) | Error::DigestMismatch(text) => Dx::Registry(text),
            Error::OverlayMount(message) => Dx::Runtime {
                context: "mount overlay",
                message,
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
            Error::Dockerfile("Dockerfile has no FROM instruction".into()),
            Error::Archive("empty manifest.json".into()),
            Error::Layer("tar: boom".into()),
            Error::NoLayers,
            Error::UnusableStateRoot("state root path contains ':'".into()),
            Error::SigningKey("invalid PEM".into()),
            Error::NotSigned("image not signed".into()),
            Error::Signature("invalid signature".into()),
            Error::SignatureUnknown("could not determine".into()),
            Error::AlreadySigned("already signed".into()),
            Error::Signing("could not sign".into()),
            Error::NoSuchImage("alpine:9".into()),
            Error::NotVisible("repository x — it does not exist".into()),
            Error::Registry("HTTP 500 at u".into()),
            Error::DigestMismatch("config digest mismatch".into()),
            Error::OverlayMount("EPERM".into()),
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
                Error::NoSuchImage("alpine:9".into()),
                "no such image alpine:9",
            ),
            (Error::NoLayers, "invalid argument: image has no layers"),
            (
                Error::Dockerfile("Dockerfile has no FROM instruction".into()),
                "invalid argument: Dockerfile has no FROM instruction",
            ),
            (
                Error::Registry("HTTP 500 at u".into()),
                "registry error: HTTP 500 at u",
            ),
            (
                Error::DigestMismatch("config digest mismatch".into()),
                "registry error: config digest mismatch",
            ),
            (
                Error::OverlayMount("EPERM".into()),
                "system call `mount overlay` failed: EPERM",
            ),
            (
                Error::NotVisible("repository r — hidden".into()),
                "no such repository r — hidden",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(delonix_model::Error::from(e).to_string(), want);
        }
    }
}

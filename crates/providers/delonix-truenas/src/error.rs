//! The TrueNAS provider's own failures, grouped by what went wrong, and the
//! dictionary number of each group (ADR-0043).
//!
//! **The messages are a contract with what was there before.** Each variant
//! carries the text the call site used to build by hand and converts into the
//! same shared class, wrapped with its number — so the CLI prints byte for
//! byte what it printed and exits with the same code.

use thiserror::Error;

/// A failure of the TrueNAS provider.
#[derive(Debug, Error)]
pub enum Error {
    /// The local HTTP/TLS stack could not build a client. Nothing about the
    /// manifest is wrong.
    #[error("{0}")]
    ClientBuild(String),

    /// The appliance's major version is not the one this build speaks.
    #[error("{0}")]
    UnsupportedVersion(String),

    /// The HTTP request to the appliance could not be sent or answered.
    #[error("{0}")]
    Request(String),

    /// The appliance answered with a non-2xx status.
    #[error("{0}")]
    HttpStatus(String),

    /// The appliance's response body did not parse as the JSON expected.
    #[error("{0}")]
    Decode(String),

    /// An asynchronous job on the appliance finished FAILED or ABORTED.
    #[error("{0}")]
    JobFailed(String),

    /// An asynchronous job stopped being reported before a terminal state.
    #[error("{0}")]
    JobVanished(String),

    /// An asynchronous job did not reach a terminal state before the deadline.
    #[error("{0}")]
    JobTimeout(String),

    /// A dataset the appliance reports with no mountpoint.
    #[error("{0}")]
    NoMountpoint(String),

    /// The appliance's own state disagreed with what it had just reported
    /// (missing right after create, still there right after delete).
    #[error("{0}")]
    Inconsistent(String),

    /// A quota under the appliance's own minimum.
    #[error("{0}")]
    QuotaTooSmall(String),

    /// A target url with no scheme, an unsupported one, or no host.
    #[error("{0}")]
    InvalidUrl(String),

    /// A target url carrying userinfo (a credential hidden in the manifest).
    #[error("{0}")]
    CredentialInUrl(String),

    /// A credential that would travel to a plain `http://` target.
    #[error("{0}")]
    InsecureCredential(String),

    /// A dataset name that is not a plain `<pool>/<name>` ZFS path.
    #[error("{0}")]
    InvalidDatasetName(String),

    /// A failure of the layers underneath (filesystem, JSON, state), with its own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// The shared class each TrueNAS failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043). A failure of the
    /// layers underneath keeps its own.
    pub fn number(&self) -> u16 {
        match self {
            Error::ClientBuild(_) => 6201,
            Error::UnsupportedVersion(_) => 6202,
            Error::JobTimeout(_) => 8201,
            Error::Request(_) => 9202,
            Error::HttpStatus(_) => 9203,
            Error::Decode(_) => 9204,
            Error::JobFailed(_) => 9205,
            Error::JobVanished(_) => 9206,
            Error::NoMountpoint(_) => 9207,
            Error::Inconsistent(_) => 9208,
            Error::QuotaTooSmall(_) => 1212,
            Error::InvalidUrl(_) => 1213,
            Error::CredentialInUrl(_) => 1214,
            Error::InsecureCredential(_) => 1215,
            Error::InvalidDatasetName(_) => 1216,
            Error::Engine(e) => e.number(),
        }
    }

    /// «An argument is wrong».
    pub fn is_invalid_argument(&self) -> bool {
        self.number() / 1000 == 1
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
            Error::ClientBuild(text) | Error::UnsupportedVersion(text) => Dx::Unavailable(text),
            Error::JobTimeout(text) => Dx::Timeout(text),
            Error::Request(text)
            | Error::HttpStatus(text)
            | Error::Decode(text)
            | Error::JobFailed(text)
            | Error::JobVanished(text)
            | Error::NoMountpoint(text)
            | Error::Inconsistent(text) => Dx::Registry(text),
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
            Error::ClientBuild("truenas: could not build the HTTP client: x".into()),
            Error::UnsupportedVersion("truenas 24.x at u is not supported".into()),
            Error::Request("truenas: request failed: x".into()),
            Error::HttpStatus("truenas: u returned HTTP 500".into()),
            Error::Decode("truenas: could not read the answer from /x: x".into()),
            Error::JobFailed("truenas: job 1 failed: boom".into()),
            Error::JobVanished("truenas: job 1 vanished before it finished".into()),
            Error::JobTimeout("truenas: job 1 was still 'RUNNING' after 300s".into()),
            Error::NoMountpoint("truenas: dataset 'tank/x' reports no mountpoint".into()),
            Error::Inconsistent("truenas: dataset 'tank/x' is still there after delete".into()),
            Error::QuotaTooSmall("quota of 1 bytes is below the minimum".into()),
            Error::InvalidUrl("invalid TrueNAS url 'x': it needs a scheme".into()),
            Error::CredentialInUrl("invalid TrueNAS url 'x': credentials in the URL".into()),
            Error::InsecureCredential(
                "refusing to send a credential to 'x' over plain http".into(),
            ),
            Error::InvalidDatasetName("invalid dataset 'x': it is empty".into()),
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
                Error::QuotaTooSmall("quota of 1 bytes is below the minimum".into()),
                "invalid argument: quota of 1 bytes is below the minimum",
            ),
            (
                Error::Request("truenas: request failed: x".into()),
                "registry error: truenas: request failed: x",
            ),
            (
                Error::JobTimeout("truenas: job 1 was still 'RUNNING' after 300s".into()),
                "timed out: truenas: job 1 was still 'RUNNING' after 300s",
            ),
            (
                Error::ClientBuild("truenas: could not build the HTTP client: x".into()),
                "unavailable: truenas: could not build the HTTP client: x",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(delonix_model::Error::from(e).to_string(), want);
        }
    }
}

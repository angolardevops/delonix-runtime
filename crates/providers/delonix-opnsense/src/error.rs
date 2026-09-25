//! This provider's own failures, grouped by what went wrong, and the
//! dictionary number of each group (ADR-0043). Shares `Domain::Network`
//! with `delonix-sdn` — the same convention `delonix-proxmox` follows for
//! `Domain::Vm` (a provider continues its port's own domain, never gets
//! one of its own).

use thiserror::Error;

/// A failure of the OPNsense gateway provider client (ADR-0051).
#[derive(Debug, Error)]
pub enum Error {
    /// The local HTTP/TLS stack could not be built from the given
    /// [`crate::Target`] — a malformed CA certificate, most likely.
    #[error("{0}")]
    ClientBuild(String),

    /// The request to the appliance could not be sent or answered at the
    /// transport level (DNS, TCP, TLS) — not a status it returned.
    #[error("{0}")]
    Request(String),

    /// A non-2xx HTTP status this client has no dedicated class for.
    #[error("{0}")]
    HttpStatus(String),

    /// HTTP 401 (a wrong key/secret pair) or HTTP 302 (no `Authorization`
    /// header sent at all — the appliance redirects toward its GUI login).
    /// Both measured live against a real appliance (ADR-0051 Phase 0); a
    /// username/password pair, even a valid GUI account's, answers 401
    /// too — only a generated key/secret pair authenticates.
    #[error("{0}")]
    Unauthorized(String),

    /// HTTP 403: a valid key/secret without the privilege this route
    /// needs.
    #[error("{0}")]
    Forbidden(String),

    /// The route does not exist on this appliance/version — HTTP 404,
    /// `{"errorMessage":"Endpoint not found"}` (ADR-0051 Phase 0, measured
    /// against a mistyped path).
    #[error("{0}")]
    RouteNotFound(String),

    /// The appliance answered `{"result":"failed","validations":{...}}` at
    /// HTTP 200 — a per-field validation error, not a transport failure
    /// (ADR-0051 Phase 0, measured live: sending an invalid protocol and
    /// a non-IP source answered exactly this shape). The message carries
    /// every `<record>.<field>: <reason>` pair the appliance gave.
    #[error("{0}")]
    Validation(String),

    /// A body past the size this client reads into memory (16 MiB).
    #[error("{0}")]
    ResponseTooLarge(String),

    /// The appliance's response body did not parse as the JSON a call
    /// expected.
    #[error("{0}")]
    Decode(String),

    /// A failure of the layers underneath (filesystem, JSON, state), with
    /// its own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// The maximum a response body may be before this client refuses to read
/// the rest of it. Every answer OPNsense's firewall API gives (a rule, an
/// alias, a search page) is a few KB; a body past this is not one of them
/// (ADR-0051 Phase 0's own measurements were all under 5 KB, `alias/get`'s
/// verbose form included).
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

impl Error {
    /// The dictionary number of this failure (ADR-0043). Exhaustive on
    /// purpose, like `delonix_sdn::Error::number`'s: a variant added
    /// tomorrow stops the build here instead of being filed under a
    /// catch-all nobody ever revisits.
    pub fn number(&self) -> u16 {
        match self {
            Error::Validation(_) => 1343,
            Error::RouteNotFound(_) => 4304,
            Error::ClientBuild(_) => 9302,
            Error::Request(_) => 9303,
            Error::HttpStatus(_) => 9304,
            Error::Unauthorized(_) => 9305,
            Error::Forbidden(_) => 9306,
            Error::ResponseTooLarge(_) => 9307,
            Error::Decode(_) => 9308,
            Error::Engine(e) => e.number(),
        }
    }

    /// «An argument is wrong» — a spec the caller can fix and resend.
    pub fn is_invalid_argument(&self) -> bool {
        self.number() / 1000 == 1
    }

    /// «No such resource».
    pub fn is_not_found(&self) -> bool {
        self.number() / 1000 == 4
    }

    /// The shared class this failure converts into, for a caller that
    /// needs a variant's payload.
    pub fn into_root(self) -> delonix_model::Error {
        delonix_model::Error::from(self).into_root()
    }
}

impl From<Error> for delonix_model::Error {
    fn from(e: Error) -> Self {
        let number = e.number();
        let class = match e {
            Error::RouteNotFound(text) => delonix_model::Error::NotFound(text),
            Error::Validation(text) => delonix_model::Error::Invalid(text),
            Error::Engine(e) => return e,
            e => delonix_model::Error::Runtime {
                context: "opnsense api",
                message: e.to_string(),
            },
        };
        delonix_model::Error::coded(number, class)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Engine(e.into())
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    fn every_variant() -> Vec<Error> {
        vec![
            Error::ClientBuild("could not build the https client: x".into()),
            Error::Request("connection refused".into()),
            Error::HttpStatus("HTTP 500: {\"errorMessage\":\"x\"}".into()),
            Error::Unauthorized(
                "authentication failed (401) — check the API key/secret".into(),
            ),
            Error::Forbidden("the API key lacks the privilege this route needs (403)".into()),
            Error::RouteNotFound("no such route: firewall/filter/bogus (404)".into()),
            Error::Validation(
                "rule.protocol: Option [] not in list.; rule.source_net: not-an-ip is not a valid source IP address or alias.".into(),
            ),
            Error::ResponseTooLarge("response exceeds 16 MiB".into()),
            Error::Decode("response body is not valid JSON".into()),
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

    #[test]
    fn is_invalid_argument_and_is_not_found_agree_with_the_number() {
        assert!(Error::Validation("x".into()).is_invalid_argument());
        assert!(!Error::Validation("x".into()).is_not_found());
        assert!(Error::RouteNotFound("x".into()).is_not_found());
        assert!(!Error::RouteNotFound("x".into()).is_invalid_argument());
        assert!(!Error::Unauthorized("x".into()).is_invalid_argument());
        assert!(!Error::Unauthorized("x".into()).is_not_found());
    }
}

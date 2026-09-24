//! The Proxmox provider's own failures, grouped by what went wrong, and the
//! dictionary number of each group (ADR-0043).
//!
//! **The messages are a contract with what was there before.** Each variant
//! carries the text the call site used to build by hand and converts into the
//! same shared class, wrapped with its number — so the CLI prints byte for
//! byte what it printed and exits with the same code.

use thiserror::Error;

/// A failure of the Proxmox provider.
#[derive(Debug, Error)]
pub enum Error {
    /// The local HTTP/TLS stack could not build a client. Nothing about the
    /// manifest is wrong.
    #[error("{0}")]
    ClientBuild(String),

    /// The node named in the target does not exist on the cluster this url
    /// answers for.
    #[error("{0}")]
    NoSuchNode(String),

    /// The HTTP request to the node could not be sent or answered.
    #[error("{0}")]
    Request(String),

    /// The node answered with a non-2xx status none of the typed variants
    /// below claims (Proxmox says most application errors with HTTP 500).
    #[error("{0}")]
    HttpStatus(String),
    /// HTTP 401: the token is wrong or revoked, or the ticket could not be
    /// renewed. Never retried against a token.
    #[error("{0}")]
    Unauthorized(String),
    /// HTTP 403: a valid credential without the privilege the route needs.
    #[error("{0}")]
    Forbidden(String),
    /// HTTP 404, or the node's own "does not exist" (said with HTTP 500).
    #[error("{0}")]
    NodeNotFound(String),
    /// HTTP 409, or the node's own "already exists" (said with HTTP 500).
    #[error("{0}")]
    NodeConflict(String),
    /// HTTP 400/422: a parameter the node does not accept.
    #[error("{0}")]
    BadRequest(String),
    /// HTTP 502/503/504: the API is up, the backend behind it is not.
    #[error("{0}")]
    NodeUnavailable(String),
    /// A body past the size this client reads into memory.
    #[error("{0}")]
    ResponseTooLarge(String),

    /// The node's response body did not parse as the JSON expected.
    #[error("{0}")]
    Decode(String),

    /// The node's answer parsed as JSON but did not have the shape a specific
    /// call needed (a VM id, a task id).
    #[error("{0}")]
    UnexpectedAnswer(String),

    /// A Proxmox task finished with a verdict other than `OK`.
    #[error("{0}")]
    TaskFailed(String),

    /// A Proxmox task did not reach a terminal state before the deadline.
    #[error("{0}")]
    TaskTimeout(String),

    /// The VM's config lock stayed busy on the node past the retry window.
    #[error("{0}")]
    LockTimeout(String),

    /// A snapshot name the VM already has.
    #[error("{0}")]
    SnapshotTaken(String),

    /// A snapshot name the VM does not have.
    #[error("{0}")]
    SnapshotNotFound(String),

    /// A target url with no scheme, a non-https scheme, or no host.
    #[error("{0}")]
    InvalidUrl(String),

    /// A target url carrying userinfo (a credential hidden in the manifest).
    #[error("{0}")]
    CredentialInUrl(String),

    /// A bridge name that cannot go into the `net0` property.
    #[error("{0}")]
    InvalidBridgeName(String),

    /// A node name that cannot go into a URL path.
    #[error("{0}")]
    InvalidNodeName(String),

    /// A `VmConfig.disk` that names neither a template nor a fresh disk.
    #[error("{0}")]
    InvalidDiskSpec(String),

    /// A snapshot name Proxmox's own namespace cannot hold.
    #[error("{0}")]
    InvalidSnapshotName(String),

    /// A cloud-init dump `type` other than `user`, `network` or `meta`.
    #[error("{0}")]
    InvalidCloudInitKind(String),

    /// A rule of the node's own (NOT this engine's SDN) firewall whose
    /// `type`/`action` this client does not accept.
    #[error("{0}")]
    InvalidFirewallRule(String),

    /// A `VmConfig` field this backend cannot honour (local paths, QEMU knobs
    /// the node owns, libvirt-only escape hatches).
    #[error("{0}")]
    UnsupportedField(String),

    /// A VM record with no Proxmox handle — it was not created by this backend.
    #[error("{0}")]
    NoHandle(String),

    /// A failure of the layers underneath (filesystem, JSON, state), with its own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// The shared class each Proxmox failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043). A failure of the
    /// layers underneath keeps its own.
    pub fn number(&self) -> u16 {
        match self {
            Error::NoSuchNode(_) => 1517,
            Error::InvalidUrl(_) => 1518,
            Error::CredentialInUrl(_) => 1519,
            Error::InvalidBridgeName(_) => 1520,
            Error::InvalidNodeName(_) => 1521,
            Error::InvalidDiskSpec(_) => 1522,
            Error::InvalidSnapshotName(_) => 1523,
            Error::UnsupportedField(_) => 1524,
            Error::InvalidFirewallRule(_) => 1528,
            Error::NoHandle(_) => 1525,
            Error::BadRequest(_) => 1526,
            Error::InvalidCloudInitKind(_) => 1529,
            Error::NodeNotFound(_) => 4504,
            Error::NodeConflict(_) => 5504,
            Error::NodeUnavailable(_) => 6506,
            Error::Unauthorized(_) => 9515,
            Error::Forbidden(_) => 9516,
            Error::ResponseTooLarge(_) => 9517,
            Error::SnapshotNotFound(_) => 4503,
            Error::SnapshotTaken(_) => 5503,
            Error::ClientBuild(_) => 6505,
            Error::TaskTimeout(_) => 8501,
            Error::LockTimeout(_) => 8502,
            Error::Request(_) => 9510,
            Error::HttpStatus(_) => 9511,
            Error::UnexpectedAnswer(_) => 9512,
            Error::TaskFailed(_) => 9513,
            Error::Decode(_) => 9514,
            Error::Engine(e) => e.number(),
        }
    }

    /// «An argument is wrong».
    pub fn is_invalid_argument(&self) -> bool {
        self.number() / 1000 == 1
    }

    /// «No such resource».
    pub fn is_not_found(&self) -> bool {
        self.number() / 1000 == 4
    }

    /// «Already exists».
    pub fn is_conflict(&self) -> bool {
        self.number() / 1000 == 5
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
            Error::SnapshotNotFound(text) | Error::NodeNotFound(text) => Dx::NotFound(text),
            Error::SnapshotTaken(text) | Error::NodeConflict(text) => Dx::Conflict(text),
            Error::ClientBuild(text) | Error::NodeUnavailable(text) => Dx::Unavailable(text),
            Error::TaskTimeout(text) | Error::LockTimeout(text) => Dx::Timeout(text),
            Error::BadRequest(text) => Dx::Invalid(text),
            Error::Request(text)
            | Error::HttpStatus(text)
            | Error::Unauthorized(text)
            | Error::Forbidden(text)
            | Error::ResponseTooLarge(text)
            | Error::Decode(text)
            | Error::UnexpectedAnswer(text)
            | Error::TaskFailed(text) => Dx::Registry(text),
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
            Error::ClientBuild("proxmox: could not build the HTTP client: x".into()),
            Error::NoSuchNode("proxmox: no node named 'x' at u (it has: y)".into()),
            Error::Request("proxmox: request failed: x".into()),
            Error::HttpStatus("proxmox: u returned HTTP 500: x".into()),
            Error::Unauthorized("proxmox: u returned HTTP 401: x".into()),
            Error::Forbidden("proxmox: u returned HTTP 403: x".into()),
            Error::NodeNotFound("Proxmox resource at /x: u returned HTTP 404: y".into()),
            Error::NodeConflict("proxmox: u returned HTTP 409: x".into()),
            Error::BadRequest("proxmox: u returned HTTP 400: x".into()),
            Error::NodeUnavailable("proxmox: u returned HTTP 503: x".into()),
            Error::ResponseTooLarge("proxmox: the answer from /x exceeded 16 MiB".into()),
            Error::Decode("proxmox: could not read the answer from x: y".into()),
            Error::UnexpectedAnswer("proxmox: could not read a VM id from /cluster/nextid: x".into()),
            Error::TaskFailed("proxmox: task failed: x".into()),
            Error::TaskTimeout("proxmox: task UPID:x was still 'running' after 600s".into()),
            Error::LockTimeout("proxmox: task failed: can't lock file 'x' - got timeout — the VM's config lock stayed busy for 90s over 5 attempts of 'start'".into()),
            Error::SnapshotTaken("Proxmox VM 100 already has a snapshot named 's1'".into()),
            Error::SnapshotNotFound("snapshot of Proxmox VM 100: s1".into()),
            Error::InvalidUrl("invalid Proxmox url 'x': it needs a scheme".into()),
            Error::CredentialInUrl("invalid Proxmox url 'x': credentials in the URL are not accepted".into()),
            Error::InvalidBridgeName("invalid Proxmox bridge name 'x': expected something like 'vmbr0'".into()),
            Error::InvalidNodeName("invalid Proxmox node name 'x': expected letters, digits, '-' and '.'".into()),
            Error::InvalidDiskSpec("proxmox: 'x' does not name anything on the node".into()),
            Error::InvalidSnapshotName("invalid Proxmox snapshot name 'x': expected letters, digits, '-' and '_'".into()),
            Error::InvalidCloudInitKind("proxmox: cloud-init dump type 'x' is not one of 'user', 'network', 'meta'".into()),
            Error::InvalidFirewallRule("invalid Proxmox firewall rule action 'x': expected ACCEPT, DROP or REJECT (a firewall group's name is not accepted here)".into()),
            Error::UnsupportedField("the 'proxmox' backend cannot honour: kernel".into()),
            Error::NoHandle("VM 'x' has no Proxmox handle in its record".into()),
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
                Error::InvalidNodeName(
                    "invalid Proxmox node name 'x': expected letters, digits, '-' and '.'".into(),
                ),
                "invalid argument: invalid Proxmox node name 'x': expected letters, digits, '-' and '.'",
            ),
            (
                Error::SnapshotNotFound("snapshot of Proxmox VM 100: s1".into()),
                "no such snapshot of Proxmox VM 100: s1",
            ),
            (
                Error::SnapshotTaken("Proxmox VM 100 already has a snapshot named 's1'".into()),
                "conflict: Proxmox VM 100 already has a snapshot named 's1'",
            ),
            (
                Error::ClientBuild("proxmox: could not build the HTTP client: x".into()),
                "unavailable: proxmox: could not build the HTTP client: x",
            ),
            (
                Error::TaskTimeout("proxmox: task UPID:x was still 'running' after 600s".into()),
                "timed out: proxmox: task UPID:x was still 'running' after 600s",
            ),
            (
                Error::Request("proxmox: request failed: x".into()),
                "registry error: proxmox: request failed: x",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(delonix_model::Error::from(e).to_string(), want);
        }
    }
}

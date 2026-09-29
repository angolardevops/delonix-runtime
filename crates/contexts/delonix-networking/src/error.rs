//! The networking context's own failures: the provider registries and the
//! ports that moved here from `delonix-sdn` (ADR-0059 F2a). Each variant
//! keeps the text, the class and the dictionary number (ADR-0043) it had in
//! `delonix-sdn`, so the CLI prints byte for byte what it printed and exits
//! with the same code; the DX-C380 block of ADR-0059 D5 is a later slice.

use thiserror::Error;

/// A failure of a network provider registry or of a remote object's ownership.
#[derive(Debug, Error)]
pub enum Error {
    // ---- invalid argument --------------------------------------------------
    /// A [`crate::gateway::register_gateway_provider`] call with no id, or a
    /// name another provider already has (ADR-0051).
    #[error("{0}")]
    GatewayProviderRegistrationRefused(String),

    /// A [`crate::gateway::GatewayProvider`] operation the provider does not
    /// implement — every default method (ADR-0051 Phase 2) returns this.
    #[error("{0}")]
    UnsupportedByGatewayProvider(String),

    /// A [`crate::network_zone::NetworkZoneProvider`] registration refused —
    /// an empty id, or one whose id/alias already belongs to a different
    /// provider (ADR-0049 addendum, mirrors `GatewayProviderRegistrationRefused`).
    #[error("{0}")]
    NetworkZoneProviderRegistrationRefused(String),

    /// `kind: NetworkZone` was applied but nothing registered a
    /// [`crate::network_zone::NetworkZoneProvider`].
    #[error("{0}")]
    NoNetworkZoneProviderConfigured(String),

    /// More than one [`crate::network_zone::NetworkZoneProvider`] is
    /// registered — `kind: NetworkZone` has no field to disambiguate.
    #[error("{0}")]
    AmbiguousNetworkZoneProvider(String),

    // ---- conflict ----------------------------------------------------------
    /// An object with the identity a remote provider (OPNsense, the SDN of a
    /// Proxmox cluster) was asked to ensure or remove already exists there
    /// WITHOUT this engine's owner mark (`crate::ownership`) — someone else's.
    /// Refused instead of adopted: adopting by name is how a hand-made rule
    /// came to be deleted by a teardown.
    #[error("{0}")]
    RemoteObjectNotOwned(String),

    /// An object this engine owns on a remote provider no longer matches what
    /// was declared (someone edited it there). There is no update in place,
    /// so it is reported instead of being reported as present.
    #[error("{0}")]
    RemoteObjectDrifted(String),

    /// The remote provider carries staged changes that are not this
    /// engine's; its commit applies EVERYTHING staged, so committing would
    /// push them too. Refused before anything is applied.
    #[error("{0}")]
    RemoteForeignPending(String),

    /// A failure of the layers underneath, with its own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// The shared class each failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043). Exhaustive on
    /// purpose: a variant added tomorrow stops the build here.
    pub fn number(&self) -> u16 {
        match self {
            Error::GatewayProviderRegistrationRefused(_) => 1341,
            Error::UnsupportedByGatewayProvider(_) => 1342,
            Error::NetworkZoneProviderRegistrationRefused(_) => 1344,
            Error::NoNetworkZoneProviderConfigured(_) => 1345,
            Error::AmbiguousNetworkZoneProvider(_) => 1346,
            Error::RemoteObjectNotOwned(_) => 5340,
            Error::RemoteObjectDrifted(_) => 5341,
            Error::RemoteForeignPending(_) => 5342,
            Error::Engine(e) => e.number(),
        }
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
            Error::RemoteObjectNotOwned(text)
            | Error::RemoteObjectDrifted(text)
            | Error::RemoteForeignPending(text) => Dx::Conflict(text),
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
            Error::GatewayProviderRegistrationRefused(
                "gateway provider 'x' cannot claim the name 'y': it already belongs to 'z'".into(),
            ),
            Error::UnsupportedByGatewayProvider(
                "ensure_alias is not supported by the 'native' gateway provider".into(),
            ),
            Error::NetworkZoneProviderRegistrationRefused(
                "network zone provider 'x' cannot claim the name 'y': it already belongs to 'z'"
                    .into(),
            ),
            Error::NoNetworkZoneProviderConfigured(
                "kind: NetworkZone has no registered provider".into(),
            ),
            Error::AmbiguousNetworkZoneProvider(
                "kind: NetworkZone has 2 registered providers (a, b)".into(),
            ),
            Error::RemoteObjectNotOwned("alias 'x' on opnsense is not this engine's".into()),
            Error::RemoteObjectDrifted("rule 'x' on opnsense: source differs".into()),
            Error::RemoteForeignPending("opnsense: 1 staged change is not this engine's".into()),
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

    /// The class each moved failure had in `delonix-sdn`: an ownership refusal
    /// is a conflict, a registry refusal an invalid argument.
    #[test]
    fn the_class_is_the_one_delonix_sdn_gave() {
        let c = delonix_model::Error::from(Error::RemoteObjectDrifted("x".into()));
        assert!(c.to_string().starts_with("conflict: "), "{c}");
        let i = delonix_model::Error::from(Error::NoNetworkZoneProviderConfigured("x".into()));
        assert!(i.to_string().starts_with("invalid argument: "), "{i}");
    }
}

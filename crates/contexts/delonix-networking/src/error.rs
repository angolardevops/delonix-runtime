//! The networking context's own failures: the provider registries and the
//! ports that moved here from `delonix-sdn` (ADR-0059 F2a). Each variant
//! keeps the text, the class and the dictionary number (ADR-0043) it had in
//! `delonix-sdn`, so the CLI prints byte for byte what it printed. The number
//! is the reason's, from the network block of ADR-0059 D5 (`DX-C380`…
//! `DX-C399`): several variants share a reason, and the message says which.

use delonix_model::codes::Reason;
use thiserror::Error;

/// A failure of a network provider registry or of a remote object's ownership.
#[derive(Debug, Error)]
pub enum Error {
    // ---- invalid argument --------------------------------------------------
    /// A [`crate::gateway::register_gateway_provider`] call with no id, or a
    /// name another provider already has (ADR-0051).
    #[error("{0}")]
    GatewayProviderRegistrationRefused(String),

    /// A gateway operation a provider does not implement. No provider raises
    /// it since ADR-0059 F2b took the refusing default bodies away; the
    /// variant stays as the typed way to say `unsupported_capability` for a
    /// gateway (D5), which replaced the published `DX-1342`.
    #[error("{0}")]
    UnsupportedByGatewayProvider(String),

    /// A [`crate::segment::SegmentProvider`] registration refused —
    /// an empty id, or one whose id/alias already belongs to a different
    /// provider (ADR-0049 addendum, mirrors `GatewayProviderRegistrationRefused`).
    #[error("{0}")]
    NetworkZoneProviderRegistrationRefused(String),

    /// `kind: NetworkZone` was applied but nothing registered a
    /// [`crate::segment::SegmentProvider`].
    #[error("{0}")]
    NoNetworkZoneProviderConfigured(String),

    /// More than one [`crate::segment::SegmentProvider`] is
    /// registered — `kind: NetworkZone` has no field to disambiguate.
    #[error("{0}")]
    AmbiguousNetworkZoneProvider(String),

    /// A document names a provider that is not registered for its role
    /// (ADR-0059 D3). D1 rule 2: the remedy is another provider, so it is
    /// `unsupported_capability`, not an invalid argument.
    #[error("{0}")]
    ProviderNotRegistered(String),

    /// Nothing names a provider for a role and nothing else decides: a
    /// `providers.yaml` without `networkDefaults.<role>`, or a gateway with
    /// no provider named and zero or several registered (ADR-0059 D3).
    #[error("{0}")]
    NoProviderForRole(String),

    /// A firewall set holds a rule the policy IR cannot represent
    /// (ADR-0059 D6): the whole set is refused, never the rule skipped.
    #[error("{0}")]
    PolicyNotRepresentable(String),

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

    // ---- unavailable (see also UnsupportedByGatewayProvider and
    // ProviderNotRegistered above) ------------------------------------------
    /// `networkDefaults.<role>` names a provider that is not registered for
    /// that role: an error, never a fall-through (ADR-0059 D3).
    #[error("{0}")]
    DefaultProviderNotRegistered(String),

    /// A resource's record names the provider that served it, and that
    /// provider is not registered now. A recorded resource never moves.
    #[error("{0}")]
    RecordedProviderNotRegistered(String),

    /// A failure of the layers underneath, with its own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// The shared class each failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043): the D5 reason's
    /// when it is one ([`Error::reason`], exhaustive on purpose: a variant
    /// added tomorrow stops the build there), its own otherwise.
    pub fn number(&self) -> u16 {
        if let Some(r) = self.reason() {
            return r.number();
        }
        match self {
            Error::GatewayProviderRegistrationRefused(_) => 1341,
            Error::NetworkZoneProviderRegistrationRefused(_) => 1344,
            Error::Engine(e) => e.number(),
            _ => unreachable!("reason() answers every other variant"),
        }
    }

    /// The ADR-0059 D5 reason, for the failures that are one. A registration
    /// refused is a programming error in the process that registered, not a
    /// provider failure, and keeps its own number.
    pub fn reason(&self) -> Option<Reason> {
        Some(match self {
            Error::GatewayProviderRegistrationRefused(_)
            | Error::NetworkZoneProviderRegistrationRefused(_)
            | Error::Engine(_) => return None,
            Error::NoNetworkZoneProviderConfigured(_)
            | Error::AmbiguousNetworkZoneProvider(_)
            | Error::NoProviderForRole(_)
            | Error::PolicyNotRepresentable(_) => Reason::InvalidIntent,
            Error::UnsupportedByGatewayProvider(_)
            | Error::ProviderNotRegistered(_)
            | Error::DefaultProviderNotRegistered(_)
            | Error::RecordedProviderNotRegistered(_) => Reason::UnsupportedCapability,
            Error::RemoteObjectNotOwned(_)
            | Error::RemoteObjectDrifted(_)
            | Error::RemoteForeignPending(_) => Reason::ProviderConflict,
        })
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
            Error::UnsupportedByGatewayProvider(text)
            | Error::ProviderNotRegistered(text)
            | Error::DefaultProviderNotRegistered(text)
            | Error::RecordedProviderNotRegistered(text) => Dx::Unavailable(text),
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
            Error::ProviderNotRegistered("no gateway provider named 'x' is registered".into()),
            Error::NoProviderForRole("kind: NetworkZone names no segment provider".into()),
            Error::PolicyNotRepresentable("firewall rule #1: peer '::1' is IPv6".into()),
            Error::DefaultProviderNotRegistered("networkDefaults.segment names 'x'".into()),
            Error::RecordedProviderNotRegistered("this NetworkZone was created on 'x'".into()),
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

    /// A reason's class is the class the failure converts into, so the number,
    /// the exit code and the reason never tell two stories (ADR-0059 D5).
    #[test]
    fn a_reason_converts_into_its_own_class() {
        for e in every_variant() {
            let Some(r) = e.reason() else { continue };
            let shown = e.to_string();
            let converted = delonix_model::Error::from(e);
            assert_eq!(converted.class(), r.class(), "{shown}");
            assert_eq!(converted.number(), r.number(), "{shown}");
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

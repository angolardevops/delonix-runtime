//! Which registered provider answers a network role (ADR-0059 D3).
//!
//! Highest first: the provider the document names (only `kind:
//! NetworkGateway` has the field; `kind: NetworkZone` keeps none, by the
//! owner's transparency rule) → the provider on the resource's record (an
//! existing resource never moves when a default changes) →
//! `networkDefaults.<role>` in `providers.yaml` → only when there is no
//! `providers.yaml` at all, the single registered provider of the role (the
//! count rule a development node has always had, kept byte for byte) →
//! otherwise refused. A name that resolves to nothing registered is an
//! error at every step, never a fall-through to the next one.
//!
//! Pure: the registered names come in as a list, so every step is testable
//! with fake providers and no process-wide registry.

use crate::error::{Error, Result};

/// A network role this module resolves. One registry per role (D1 rule 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// `SegmentProvider`: `kind: NetworkZone`.
    Segment,
    /// `GatewayProvider`: `kind: NetworkGateway`.
    Gateway,
}

impl Role {
    /// The role's key under `networkDefaults`.
    pub fn word(self) -> &'static str {
        match self {
            Role::Segment => "segment",
            Role::Gateway => "gateway",
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Role::Segment => "NetworkZone",
            Role::Gateway => "NetworkGateway",
        }
    }
}

/// What the caller knows when it asks for a provider.
#[derive(Debug, Clone, Copy)]
pub struct Wanted<'a> {
    pub role: Role,
    /// The provider the document names (`NetworkGateway.spec.provider`).
    pub named: Option<&'a str>,
    /// The provider the resource's record says served it.
    pub recorded: Option<&'a str>,
    /// `networkDefaults.<role>` from `providers.yaml`.
    pub default: Option<&'a str>,
    /// The `providers.yaml` in force, when there is one. Its presence turns
    /// the count rule off.
    pub config: Option<&'a str>,
}

/// One registration as the resolver sees it: the id and the aliases.
pub type Candidate = (&'static str, &'static [&'static str]);

fn find(cands: &[Candidate], name: &str) -> Option<&'static str> {
    let want = name.trim().to_lowercase();
    cands
        .iter()
        .find(|(id, aliases)| *id == want || aliases.contains(&want.as_str()))
        .map(|(id, _)| *id)
}

fn known(cands: &[Candidate]) -> String {
    if cands.is_empty() {
        "none".into()
    } else {
        cands
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn given(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

/// The registered id that answers `want`, or the refusal that says why none
/// does.
pub fn choose(cands: &[Candidate], want: &Wanted) -> Result<&'static str> {
    let role = want.role.word();
    let kind = want.role.kind();
    if let Some(name) = given(want.named) {
        return find(cands, name).ok_or_else(|| {
            Error::ProviderNotRegistered(format!(
                "no {role} provider named '{name}' is registered (known: {})",
                known(cands)
            ))
        });
    }
    if let Some(name) = given(want.recorded) {
        return find(cands, name).ok_or_else(|| {
            Error::RecordedProviderNotRegistered(format!(
                "this {kind} was created on the {role} provider '{name}', which is not registered \
                 now (known: {}) — a recorded resource never moves to another provider; \
                 configure '{name}' again",
                known(cands)
            ))
        });
    }
    if let Some(name) = given(want.default) {
        return find(cands, name).ok_or_else(|| {
            Error::DefaultProviderNotRegistered(format!(
                "networkDefaults.{role} names '{name}', which is not registered for the role \
                 '{role}' (known: {})",
                known(cands)
            ))
        });
    }
    if let Some(path) = want.config {
        return Err(Error::NoProviderForRole(format!(
            "kind: {kind} names no {role} provider and {path} sets no networkDefaults.{role} — \
             set it, or remove the file to fall back to the single registered provider"
        )));
    }
    match (want.role, cands) {
        (_, [(id, _)]) => Ok(id),
        (Role::Segment, []) => Err(Error::NoNetworkZoneProviderConfigured(
            "kind: NetworkZone has no registered provider — configure DELONIX_PROXMOX_URL (the \
             only one this build knows about) and its credential"
                .into(),
        )),
        (Role::Segment, many) => Err(Error::AmbiguousNetworkZoneProvider(format!(
            "kind: NetworkZone has {} registered providers ({}) and nothing picks one — set \
             networkDefaults.segment in a providers.yaml",
            many.len(),
            known(many)
        ))),
        (Role::Gateway, []) => Err(Error::NoProviderForRole(
            "kind: NetworkGateway names no provider and no gateway provider is registered — \
             configure the opnsense entry (DELONIX_OPNSENSE_URL and its credential)"
                .into(),
        )),
        (Role::Gateway, many) => Err(Error::NoProviderForRole(format!(
            "kind: NetworkGateway names no provider and {} gateway providers are registered \
             ({}) — name one in spec.provider, or set networkDefaults.gateway in a providers.yaml",
            many.len(),
            known(many)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two segment providers registered in the process — the case the
    /// battery cannot build, because only Proxmox serves the role and
    /// `providers.yaml` takes one entry per type (ADR-0059 F2c addendum).
    const TWO: &[Candidate] = &[("alpha", &["a"]), ("beta", &[])];

    fn want(role: Role) -> Wanted<'static> {
        Wanted {
            role,
            named: None,
            recorded: None,
            default: None,
            config: None,
        }
    }

    #[test]
    fn the_default_picks_one_of_two() {
        let w = Wanted {
            default: Some("beta"),
            config: Some("/etc/delonix/providers.yaml"),
            ..want(Role::Segment)
        };
        assert_eq!(choose(TWO, &w).unwrap(), "beta");
    }

    #[test]
    fn the_record_wins_over_the_default() {
        let w = Wanted {
            recorded: Some("alpha"),
            default: Some("beta"),
            config: Some("/p.yaml"),
            ..want(Role::Segment)
        };
        assert_eq!(choose(TWO, &w).unwrap(), "alpha");
    }

    #[test]
    fn the_document_wins_over_the_record() {
        let w = Wanted {
            named: Some("A"),
            recorded: Some("beta"),
            ..want(Role::Gateway)
        };
        assert_eq!(
            choose(TWO, &w).unwrap(),
            "alpha",
            "aliases resolve, case-insensitively"
        );
    }

    #[test]
    fn a_default_naming_an_unregistered_provider_is_unavailable_never_a_fall_through() {
        let w = Wanted {
            default: Some("gamma"),
            config: Some("/p.yaml"),
            ..want(Role::Segment)
        };
        let e = choose(TWO, &w).unwrap_err();
        assert!(matches!(e, Error::DefaultProviderNotRegistered(_)), "{e}");
        let root = delonix_model::Error::from(e);
        assert!(root.to_string().starts_with("unavailable: "), "{root}");
        assert_eq!(root.number(), 6303);
    }

    #[test]
    fn a_record_naming_an_unregistered_provider_never_moves() {
        let w = Wanted {
            recorded: Some("gamma"),
            default: Some("alpha"),
            config: Some("/p.yaml"),
            ..want(Role::Segment)
        };
        let e = choose(TWO, &w).unwrap_err();
        assert!(matches!(e, Error::RecordedProviderNotRegistered(_)), "{e}");
        assert_eq!(e.number(), 6304);
    }

    #[test]
    fn a_config_without_the_default_turns_the_count_rule_off() {
        let one: &[Candidate] = &[("alpha", &[])];
        let w = Wanted {
            config: Some("/p.yaml"),
            ..want(Role::Segment)
        };
        let e = choose(one, &w).unwrap_err();
        assert!(matches!(e, Error::NoProviderForRole(_)), "{e}");
        assert!(e.to_string().contains("networkDefaults.segment"), "{e}");
    }

    #[test]
    fn without_a_config_the_count_rule_is_kept() {
        let one: &[Candidate] = &[("alpha", &[])];
        assert_eq!(choose(one, &want(Role::Segment)).unwrap(), "alpha");
        assert!(matches!(
            choose(&[], &want(Role::Segment)).unwrap_err(),
            Error::NoNetworkZoneProviderConfigured(_)
        ));
        assert!(matches!(
            choose(TWO, &want(Role::Segment)).unwrap_err(),
            Error::AmbiguousNetworkZoneProvider(_)
        ));
    }

    #[test]
    fn a_document_naming_an_unregistered_provider_names_what_is_known() {
        let w = Wanted {
            named: Some("native"),
            ..want(Role::Gateway)
        };
        let e = choose(&[], &w).unwrap_err();
        assert!(matches!(e, Error::ProviderNotRegistered(_)), "{e}");
        assert!(e.to_string().contains("known: none"), "{e}");
    }
}

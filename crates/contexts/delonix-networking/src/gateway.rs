//! A pluggable backend for node-egress / perimeter gateway policy — NAT,
//! multi-WAN, perimeter filtering — reached by an appliance like OPNsense
//! over its own API (ADR-0051).
//!
//! **Deliberately not the same thing as `delonix_sdn::infra::apply_firewall`** /
//! `delonix_compute::ports::NetworkProvider::apply_firewall`, which is
//! per-workload nftables rules and is always native: `FirewallPerWorkload`/
//! `FirewallDefaultDeny`/`FirewallSourceFiltering`/`FirewallEgressPolicy`
//! (ADR-0050) already answer that for free, rootless, with no extra VM.
//! What an appliance at the perimeter is good for — NAT, multi-WAN
//! failover, VPN termination — is what this port is for.
//!
//! # The registry mirrors `VmBackend`'s
//!
//! An id, a factory that may fail, registration that does no I/O (ADR-0008:
//! "a map populated at startup, not a plugin system"). The port lives in the
//! networking context since ADR-0059 F2a, in the shape `VmBackend` left
//! `delonix-vm` for the compute context.
//!
//! # No default bodies, and no builtin provider (ADR-0059 D1, F2b)
//!
//! Every operation is required. A default that refuses let a provider
//! compile while claiming, by existing, an operation it does not have; a
//! default that answers `Ok(())` was worse. The one implementation,
//! `crates/providers/delonix-opnsense`, has every method for real, against
//! what ADR-0051 Phase 0 measured on an appliance.
//!
//! The `native` provider Phase 1 registered is gone: it refused every
//! `ensure_*`, so a document naming it could never be applied, and the
//! native masquerade/forward dataplane it stood for is unconditional and
//! never needed a port. The registry starts empty; a gateway is chosen by
//! the name of a provider something registered.

use crate::error::{Error, Result};
use crate::ownership::{OwnerMark, RemoveOutcome};

/// What kind of value [`GatewayAlias::content`] holds. Only the two shapes
/// a v1 client needs (ADR-0051 Phase 0 measured `add_item`'s flat form for
/// `type: "host"`; `network` follows the same shape by the same source —
/// `port`/`url`/`geoip`/… are real OPNsense alias types this v1 does not
/// need and is not claiming to support).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasKind {
    /// One or more IPs/hostnames.
    Host,
    /// One or more CIDRs.
    Network,
}

/// An address alias to ensure exists on a gateway provider, found by NAME —
/// the appliance's own key for one (ADR-0051 Phase 0: `alias/get`/`get_item`
/// key every alias by `name` at the top level). Found is not owned: an alias
/// with that name is this engine's only when its description carries the
/// caller's [`OwnerMark`] (see [`crate::ownership`]).
#[derive(Debug, Clone)]
pub struct GatewayAlias {
    pub name: String,
    pub kind: AliasKind,
    /// One entry per host/network in `content` (OPNsense accepts them
    /// newline-joined; this type keeps them separate so a caller never has
    /// to know that).
    pub content: Vec<String>,
    pub description: String,
}

/// A perimeter filter rule to ensure exists, found by its DESCRIPTION — the
/// appliance has no other stable name for a rule (the OPNsense docs' own
/// worked example finds-or-creates by `description`). The description
/// written to the appliance is this one plus the caller's [`OwnerMark`]; a
/// rule whose description matches WITHOUT the mark is someone else's.
#[derive(Debug, Clone)]
pub struct GatewayRule {
    pub description: String,
    /// An alias name, a bare CIDR, or `"any"` — resolved by the appliance,
    /// not this type (ADR-0051 Phase 0: `search_rule` denormalizes an
    /// alias's current content into `alias_meta_source_net` on read, so a
    /// caller reading state back never needs a second lookup either).
    pub source: String,
    pub destination: String,
    /// `None` = any protocol. `Some("TCP")`/`Some("UDP")`/… otherwise —
    /// measured live as the exact string the docs' worked example sends,
    /// never validated client-side against the full protocol list.
    pub protocol: Option<String>,
}

/// Whether an `ensure_*` call created something or found it already there
/// — already there meaning OWNED by the caller's mark and matching the
/// declaration; an owned object that no longer matches is an error
/// (`RemoteObjectDrifted`), never `AlreadyPresent`. Never "updated": Phase 2 does not implement update-in-place (`set_item`/
/// `set_rule`'s exact request shape — the verbose form `get` returns, or
/// the flat form `add_item` accepts — was not part of the ADR-0051 Phase 0
/// spike, and guessing it risks the same trap the rest of this module
/// exists to avoid). A caller that needs to change an existing alias/rule
/// removes and re-creates it today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    Created,
    AlreadyPresent,
}

/// A backend that can enforce node-egress / perimeter gateway policy.
/// Extends the provider skeleton (ADR-0059 D1 rule 4): `id`, `capabilities()`
/// — the ADR-0050 report, declared for a remote appliance — and `health`.
pub trait GatewayProvider: delonix_compute::vm_provider::Provider {
    /// `true` if this backend can be used right now.
    ///
    /// Never a network round trip — the same contract
    /// `VmBackend::available` documents for the same reason:
    /// a remote backend (OPNsense) answers this from what its registration
    /// already knows, not by connecting to the appliance.
    fn available(&self) -> bool;

    /// Ensures an address alias exists, by name, OWNED by `owner`: one
    /// found under that name without the mark is refused
    /// (`RemoteObjectNotOwned`), one with the mark whose content differs is
    /// refused too (`RemoteObjectDrifted`).
    fn ensure_alias(
        &self,
        alias: &GatewayAlias,
        owner: &OwnerMark,
    ) -> delonix_model::Result<EnsureOutcome>;

    /// Removes an alias by name — only if `owner` owns it; anything else
    /// under that name is left alone and reported
    /// ([`RemoveOutcome::NotOwned`]).
    fn remove_alias(&self, name: &str, owner: &OwnerMark) -> delonix_model::Result<RemoveOutcome>;

    /// Ensures a perimeter filter rule exists, by description, owned by
    /// `owner` — the same refusals as [`ensure_alias`](Self::ensure_alias).
    fn ensure_rule(
        &self,
        rule: &GatewayRule,
        owner: &OwnerMark,
    ) -> delonix_model::Result<EnsureOutcome>;

    /// Removes the rules with this description that `owner` owns; one that
    /// matches without the mark is left alone and reported.
    fn remove_rule(
        &self,
        description: &str,
        owner: &OwnerMark,
    ) -> delonix_model::Result<RemoveOutcome>;

    /// Retires `owner` on the provider once a teardown removed everything it
    /// marked — the label object an OPNsense owner mark lives in.
    fn release_owner(&self, owner: &OwnerMark) -> delonix_model::Result<RemoveOutcome>;

    /// Refuses (`RemoteForeignPending`) when the provider already carries
    /// staged changes nobody applied — called BEFORE the first staged write,
    /// so a refusal leaves nothing of this engine's behind. The
    /// [`commit`](Self::commit) checks again, for what was staged in
    /// between.
    fn check_no_foreign_pending(&self) -> delonix_model::Result<()>;

    /// Activates whatever [`ensure_alias`](Self::ensure_alias)/
    /// [`ensure_rule`](Self::ensure_rule)/removal staged.
    ///
    /// A provider whose commit applies everything staged — OPNsense's does,
    /// the whole `config.xml` — must refuse (`RemoteForeignPending`) when
    /// something staged is not what THIS value staged, before applying.
    ///
    /// OPNsense's own `commit` is synchronous (ADR-0051 Phase 0, measured:
    /// `firewall/filter/apply` returns in under a second with the reload's
    /// own stdout, never a task id to poll) — a provider that DOES have
    /// something to stage is expected to make this call itself, in this
    /// method, not return early and leave the caller polling.
    fn commit(&self) -> delonix_model::Result<()>;
}

/// Builds a [`GatewayProvider`], or reports why it could not.
///
/// `Send + Sync` because the table is process-wide. It constrains the
/// CLOSURE, not the trait — a provider implementation is untouched by this
/// (same reasoning as the VM backend factory).
pub type GatewayProviderFactory = Box<dyn Fn() -> Result<Box<dyn GatewayProvider>> + Send + Sync>;

/// One provider this build knows about: its canonical id, the aliases
/// accepted on input, and how to build one.
pub struct GatewayProviderRegistration {
    /// Canonical id. Must equal what the built provider's
    /// [`Provider::id`](delonix_compute::vm_provider::Provider::id) returns.
    pub id: &'static str,
    /// Extra spellings accepted from a caller; never repeats `id`.
    pub aliases: &'static [&'static str],
    pub new: GatewayProviderFactory,
}

impl std::fmt::Debug for GatewayProviderRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayProviderRegistration")
            .field("id", &self.id)
            .field("aliases", &self.aliases)
            .finish_non_exhaustive()
    }
}

/// Every registered provider. A map populated at startup, not a plugin
/// system (same as the VM backend registry): nothing loads a `.so`, and the
/// only way in is [`register_gateway_provider`], called by a process that
/// already linked the provider's crate.
static GATEWAY_PROVIDERS: std::sync::OnceLock<std::sync::RwLock<Vec<GatewayProviderRegistration>>> =
    std::sync::OnceLock::new();

fn gateway_providers() -> &'static std::sync::RwLock<Vec<GatewayProviderRegistration>> {
    GATEWAY_PROVIDERS.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

fn with_gateway_providers<T>(f: impl FnOnce(&[GatewayProviderRegistration]) -> T) -> T {
    let guard = gateway_providers()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

/// Adds a provider to the registry. Idempotent by id: registering the same
/// id twice REPLACES the entry, so a process that configures a target twice
/// ends up with the last one rather than two that shadow each other.
///
/// **Nothing here does I/O**: the factory is not called, so registering a
/// node that is unreachable costs nothing until someone actually selects it
/// (same contract as the VM backend registry).
///
/// Refused, rather than accepted and left to surprise someone later: an id
/// that is empty, or an id/alias that collides with a DIFFERENT provider
/// already registered — the loser would become unreachable by name,
/// silently.
pub fn register_gateway_provider(reg: GatewayProviderRegistration) -> Result<()> {
    if reg.id.trim().is_empty() {
        return Err(Error::GatewayProviderRegistrationRefused(
            "a gateway provider registration needs an id".into(),
        ));
    }
    let mut guard = gateway_providers()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    for name in std::iter::once(&reg.id).chain(reg.aliases.iter()) {
        let want = name.trim().to_lowercase();
        if let Some(clash) = guard
            .iter()
            .find(|b| b.id != reg.id && (b.id == want || b.aliases.contains(&want.as_str())))
        {
            return Err(Error::GatewayProviderRegistrationRefused(format!(
                "gateway provider '{}' cannot claim the name '{}': it already belongs to '{}'",
                reg.id, name, clash.id
            )));
        }
    }
    guard.retain(|b| b.id != reg.id);
    guard.push(reg);
    Ok(())
}

/// Builds the provider `name` resolves to, or `None` if nothing does.
/// Case- and whitespace-insensitive, matching id or alias.
pub fn gateway_provider_for(name: &str) -> Option<Result<Box<dyn GatewayProvider>>> {
    let want = name.trim().to_lowercase();
    with_gateway_providers(|regs| {
        regs.iter()
            .find(|r| r.id == want || r.aliases.contains(&want.as_str()))
            .map(|r| (r.new)())
    })
}

/// The provider that answers `want` (ADR-0059 D3, [`crate::resolve`]),
/// with the id it resolved to — the caller records it, so the resource
/// never moves when a default changes. Resolving does no I/O; only the
/// chosen provider is built.
pub fn choose_gateway_provider(
    want: &crate::resolve::Wanted,
) -> Result<(&'static str, Box<dyn GatewayProvider>)> {
    with_gateway_providers(|regs| {
        let cands: Vec<crate::resolve::Candidate> =
            regs.iter().map(|r| (r.id, r.aliases)).collect();
        let id = crate::resolve::choose(&cands, want)?;
        let reg = regs
            .iter()
            .find(|r| r.id == id)
            .expect("choose returns a registered id");
        Ok((id, (reg.new)()?))
    })
}

/// The registered ids, in registration order — for an error that names what
/// IS accepted, and for `provider ls`-style listing later.
pub fn gateway_provider_ids() -> Vec<&'static str> {
    with_gateway_providers(|regs| regs.iter().map(|r| r.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(id: &'static str, aliases: &'static [&'static str]) -> GatewayProviderRegistration {
        struct Fake(&'static str);
        impl delonix_compute::vm_provider::Provider for Fake {
            fn id(&self) -> delonix_compute::vm_provider::ProviderId {
                delonix_compute::vm_provider::ProviderId(self.0)
            }
            fn capabilities(&self) -> delonix_compute::capability::ProviderReport {
                use delonix_compute::capability::{
                    CapabilityState, HealthStatus, ProviderHealth, ProviderKind, ProviderReport,
                };
                ProviderReport::build(
                    self.0,
                    ProviderKind::Gateway,
                    true,
                    ProviderHealth {
                        status: HealthStatus::Healthy,
                        reason: "Ok",
                        message: String::new(),
                    },
                    |_| CapabilityState::NotImplemented,
                )
            }
        }
        impl GatewayProvider for Fake {
            fn available(&self) -> bool {
                true
            }
            fn ensure_alias(
                &self,
                _: &GatewayAlias,
                _: &OwnerMark,
            ) -> delonix_model::Result<EnsureOutcome> {
                Ok(EnsureOutcome::Created)
            }
            fn remove_alias(&self, _: &str, _: &OwnerMark) -> delonix_model::Result<RemoveOutcome> {
                Ok(RemoveOutcome::Absent)
            }
            fn ensure_rule(
                &self,
                _: &GatewayRule,
                _: &OwnerMark,
            ) -> delonix_model::Result<EnsureOutcome> {
                Ok(EnsureOutcome::Created)
            }
            fn remove_rule(&self, _: &str, _: &OwnerMark) -> delonix_model::Result<RemoveOutcome> {
                Ok(RemoveOutcome::Absent)
            }
            fn release_owner(&self, _: &OwnerMark) -> delonix_model::Result<RemoveOutcome> {
                Ok(RemoveOutcome::Absent)
            }
            fn check_no_foreign_pending(&self) -> delonix_model::Result<()> {
                Ok(())
            }
            fn commit(&self) -> delonix_model::Result<()> {
                Ok(())
            }
        }
        GatewayProviderRegistration {
            id,
            aliases,
            new: Box::new(move || Ok(Box::new(Fake(id)))),
        }
    }

    #[test]
    fn register_gateway_provider_refuses_an_empty_id() {
        let err = register_gateway_provider(fake("", &[])).unwrap_err();
        assert!(matches!(err, Error::GatewayProviderRegistrationRefused(_)));
        assert_eq!(err.number(), 1341);
    }

    #[test]
    fn register_gateway_provider_refuses_a_name_clash() {
        register_gateway_provider(fake("clash-a", &["shared"])).expect("register a");
        let err = register_gateway_provider(fake("clash-b", &["shared"])).unwrap_err();
        assert!(matches!(err, Error::GatewayProviderRegistrationRefused(_)));
    }

    #[test]
    fn register_gateway_provider_is_idempotent_by_id() {
        register_gateway_provider(fake("reconfigurable", &[])).expect("1st registration");
        register_gateway_provider(fake("reconfigurable", &[])).expect("2nd replaces it");
        let ids: Vec<_> = gateway_provider_ids()
            .into_iter()
            .filter(|&id| id == "reconfigurable")
            .collect();
        assert_eq!(ids.len(), 1, "a reconfigured id does not leave two entries");
    }

    /// ADR-0059 F2b: nothing is registered until something registers it;
    /// the name `native` resolves to nothing.
    #[test]
    fn native_is_no_longer_a_provider() {
        assert!(gateway_provider_for("native").is_none());
        assert!(!gateway_provider_ids().contains(&"native"));
    }

    #[test]
    fn gateway_provider_for_returns_none_for_an_unknown_name() {
        assert!(gateway_provider_for("this-does-not-exist-at-all").is_none());
    }
}

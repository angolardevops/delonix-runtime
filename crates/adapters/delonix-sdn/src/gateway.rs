//! A pluggable backend for node-egress / perimeter gateway policy — NAT,
//! multi-WAN, perimeter filtering — reached by an appliance like OPNsense
//! over its own API (ADR-0051).
//!
//! **Deliberately not the same thing as [`crate::infra::apply_firewall`]** /
//! `delonix_compute::ports::NetworkProvider::apply_firewall`, which is
//! per-workload nftables rules and is always native: `FirewallPerWorkload`/
//! `FirewallDefaultDeny`/`FirewallSourceFiltering`/`FirewallEgressPolicy`
//! (ADR-0050) already answer that for free, rootless, with no extra VM, and
//! nothing about talking to an external appliance's REST API improves on it.
//! What an appliance at the perimeter is actually good for — NAT, multi-WAN
//! failover, VPN termination — has no answerer today; this trait is where
//! one goes.
//!
//! # Why this mirrors `delonix-vm`'s `VmBackend`/`register_backend`
//!
//! Method for method, on purpose: `BackendRegistration`'s shape (an id, a
//! factory that may fail, an `auto_selectable` flag checked at registration
//! time so auto-detection never has to do I/O) is the one mechanism this repo
//! already trusts for "let a process that linked a provider's crate make it
//! selectable without a plugin loader" (ADR-0008's own words: "a map
//! populated at startup, not a plugin system"). Reinventing it here would
//! just be a second copy to keep in sync.
//!
//! It lives in THIS crate and not a new context crate for the same reason
//! `VmBackend` currently lives in `delonix-vm` rather than `delonix-compute`:
//! that move is P4's own unfinished migration (`delonix-proxmox → delonix-vm`
//! is a named, phase-tagged exception in `scripts/arch_fitness.py`, not the
//! target shape). `GatewayProvider` takes the same kind of exception when
//! `delonix-opnsense` is built (Phase 2), and moves wherever `VmBackend`
//! eventually moves, in the same commit shape — see ADR-0051.
//!
//! # Phase 1 → Phase 2
//!
//! Phase 1 shipped the trait and the registry with **zero behavior
//! change** — [`NATIVE_ID`] was the only registered provider, and it did
//! nothing beyond answer [`GatewayProvider::available`]. It deliberately
//! had no `ensure_rule`/`apply` method: designing OPNsense's operational
//! shape before the live spike against a real appliance (ADR-0051, Phase 0)
//! would have risked exactly the trap `delonix-proxmox`'s own module doc
//! names for Proxmox tasks — treating a guess as a measurement.
//!
//! Phase 0 happened (2026-09-24, against a real OPNsense 26.1.2 appliance)
//! and measured the write path, `apply()`'s semantics and the error shapes.
//! This is Phase 2: [`GatewayProvider`] grows the operations Phase 0 makes
//! safe to design — [`ensure_alias`](GatewayProvider::ensure_alias),
//! [`ensure_rule`](GatewayProvider::ensure_rule) and
//! [`commit`](GatewayProvider::commit) — with default implementations that
//! REFUSE (never silently ignore, per this repo's own no-silent-failure
//! rule). [`NativeGatewayProvider`] overrides none of them: it has no alias
//! or rule concept to offer, and its masquerade/forward dataplane is
//! unconditional, so [`GatewayProvider::commit`]'s default (a no-op `Ok`)
//! is honest for it, while the other four default to a clear "not
//! supported by this provider" instead of pretending to do nothing useful.
//! `crates/providers/delonix-opnsense` (also Phase 2) is the first provider
//! that overrides all five for real, against what Phase 0 measured.

use crate::error::{Error, Result};

/// The canonical id of the always-registered native provider.
pub const NATIVE_ID: &str = "native";

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

/// An address alias to ensure exists on a gateway provider, identified by
/// NAME — the appliance's own identity for one (ADR-0051 Phase 0: `alias/
/// get`/`get_item` key every alias by `name` at the top level).
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

/// A perimeter filter rule to ensure exists, identified by its
/// DESCRIPTION — the appliance has no other stable name for a rule, and
/// this is the identity the OPNsense docs' own worked example uses to
/// find-or-create one (`search_rule` by `description`, then `add_rule` if
/// absent).
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

/// Whether an `ensure_*` call created something or found it already there.
/// Never "updated": Phase 2 does not implement update-in-place (`set_item`/
/// `set_rule`'s exact request shape — the verbose form `get` returns, or
/// the flat form `add_item` accepts — was not part of the ADR-0051 Phase 0
/// spike, and guessing it risks the same trap the rest of this module
/// exists to avoid). An `ensure_*` call against a name/description that
/// already exists with DIFFERENT content returns `AlreadyPresent` without
/// touching it — a caller that needs to change an existing alias/rule
/// removes and re-creates it today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    Created,
    AlreadyPresent,
}

/// A backend that can enforce node-egress / perimeter gateway policy.
pub trait GatewayProvider {
    /// Stable identifier, persisted wherever a policy would eventually name
    /// its provider. Must equal the `id` the registration that built this
    /// value used.
    fn id(&self) -> &'static str;

    /// `true` if this backend can be used right now.
    ///
    /// Never a network round trip — the same contract
    /// [`delonix_vm::VmBackend::available`] documents for the same reason:
    /// a remote backend (OPNsense) answers this from what its registration
    /// already knows, not by connecting to the appliance.
    fn available(&self) -> bool;

    /// Ensures an address alias exists, by name. Default: refuses — a
    /// provider with no alias concept (the native one) has nothing honest
    /// to do here.
    fn ensure_alias(&self, alias: &GatewayAlias) -> delonix_model::Result<EnsureOutcome> {
        let _ = alias;
        Err(unsupported(self.id(), "ensure_alias"))
    }

    /// Removes an alias by name. Default: refuses, same reasoning as
    /// [`ensure_alias`](Self::ensure_alias).
    fn remove_alias(&self, name: &str) -> delonix_model::Result<()> {
        let _ = name;
        Err(unsupported(self.id(), "remove_alias"))
    }

    /// Ensures a perimeter filter rule exists, by description. Default:
    /// refuses, same reasoning as [`ensure_alias`](Self::ensure_alias).
    fn ensure_rule(&self, rule: &GatewayRule) -> delonix_model::Result<EnsureOutcome> {
        let _ = rule;
        Err(unsupported(self.id(), "ensure_rule"))
    }

    /// Removes a rule by description. Default: refuses, same reasoning as
    /// [`ensure_alias`](Self::ensure_alias).
    fn remove_rule(&self, description: &str) -> delonix_model::Result<()> {
        let _ = description;
        Err(unsupported(self.id(), "remove_rule"))
    }

    /// Activates whatever [`ensure_alias`](Self::ensure_alias)/
    /// [`ensure_rule`](Self::ensure_rule)/removal staged.
    ///
    /// Default: `Ok(())` — a provider with no alias/rule concept has
    /// nothing staged, ever, so there is nothing dishonest about this one
    /// default being a no-op instead of a refusal (unlike the other four).
    /// OPNsense's own `commit` is synchronous (ADR-0051 Phase 0, measured:
    /// `firewall/filter/apply` returns in under a second with the reload's
    /// own stdout, never a task id to poll) — a provider that DOES have
    /// something to stage is expected to make this call itself, in this
    /// method, not return early and leave the caller polling.
    fn commit(&self) -> delonix_model::Result<()> {
        Ok(())
    }
}

/// Fail-closed error for a provider that does not implement an operation —
/// every default method above returns this. Mirrors
/// `delonix_vm::unsupported_pause`/`unsupported_snapshot` exactly, down to
/// returning the SHARED type directly rather than this crate's own
/// `Result`: the trait's methods live on `delonix_model::Result`, the same
/// reason `VmBackend`'s do.
fn unsupported(provider: &str, op: &str) -> delonix_model::Error {
    Error::UnsupportedByGatewayProvider(format!(
        "{op} is not supported by the '{provider}' gateway provider"
    ))
    .into()
}

/// The native provider: the masquerade/forward dataplane every network
/// already has, unconditionally. Registering nothing else leaves the engine
/// exactly as it behaves today — this type is the reason Phase 1 is a
/// zero-behavior-change scaffold rather than a new default.
struct NativeGatewayProvider;

impl GatewayProvider for NativeGatewayProvider {
    fn id(&self) -> &'static str {
        NATIVE_ID
    }
    fn available(&self) -> bool {
        true
    }
}

/// Builds a [`GatewayProvider`], or reports why it could not.
///
/// `Send + Sync` because the table is process-wide. It constrains the
/// CLOSURE, not the trait — a provider implementation is untouched by this
/// (same reasoning as `delonix_vm::BackendFactory`).
pub type GatewayProviderFactory = Box<dyn Fn() -> Result<Box<dyn GatewayProvider>> + Send + Sync>;

/// One provider this build knows about: its canonical id, the aliases
/// accepted on input, and how to build one.
pub struct GatewayProviderRegistration {
    /// Canonical id. Must equal what the built provider's
    /// [`GatewayProvider::id`] returns.
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

fn builtin_gateway_providers() -> Vec<GatewayProviderRegistration> {
    vec![GatewayProviderRegistration {
        id: NATIVE_ID,
        aliases: &[],
        new: Box::new(|| Ok(Box::new(NativeGatewayProvider))),
    }]
}

/// Every registered provider. A map populated at startup, not a plugin
/// system (same as `delonix_vm::BACKENDS`): nothing loads a `.so`, and the
/// only way in is [`register_gateway_provider`], called by a process that
/// already linked the provider's crate.
static GATEWAY_PROVIDERS: std::sync::OnceLock<std::sync::RwLock<Vec<GatewayProviderRegistration>>> =
    std::sync::OnceLock::new();

fn gateway_providers() -> &'static std::sync::RwLock<Vec<GatewayProviderRegistration>> {
    GATEWAY_PROVIDERS.get_or_init(|| std::sync::RwLock::new(builtin_gateway_providers()))
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
/// (same contract as `delonix_vm::register_backend`).
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
        impl GatewayProvider for Fake {
            fn id(&self) -> &'static str {
                self.0
            }
            fn available(&self) -> bool {
                true
            }
        }
        GatewayProviderRegistration {
            id,
            aliases,
            new: Box::new(move || Ok(Box::new(Fake(id)))),
        }
    }

    #[test]
    fn native_is_registered_by_default() {
        // Not "the only one registered": another test in this suite may have
        // added more to the process-wide list before this one runs. Only the
        // essential claim is checked — that "native" is there and answers
        // available().
        assert!(gateway_provider_ids().contains(&NATIVE_ID));
        let native = gateway_provider_for(NATIVE_ID)
            .expect("native must resolve")
            .expect("native never fails to build");
        assert_eq!(native.id(), NATIVE_ID);
        assert!(native.available());
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

    #[test]
    fn gateway_provider_for_returns_none_for_an_unknown_name() {
        assert!(gateway_provider_for("this-does-not-exist-at-all").is_none());
    }

    fn sample_alias() -> GatewayAlias {
        GatewayAlias {
            name: "example".into(),
            kind: AliasKind::Host,
            content: vec!["10.0.0.1".into()],
            description: "test".into(),
        }
    }

    fn sample_rule() -> GatewayRule {
        GatewayRule {
            description: "test".into(),
            source: "example".into(),
            destination: "10.0.0.0/24".into(),
            protocol: Some("TCP".into()),
        }
    }

    #[test]
    fn native_refuses_alias_and_rule_operations() {
        let native = gateway_provider_for(NATIVE_ID)
            .expect("native must resolve")
            .expect("native never fails to build");
        let err = native.ensure_alias(&sample_alias()).unwrap_err();
        assert!(err.to_string().contains("ensure_alias"));
        assert!(err.to_string().contains(NATIVE_ID));
        assert!(native.remove_alias("example").is_err());
        assert!(native.ensure_rule(&sample_rule()).is_err());
        assert!(native.remove_rule("test").is_err());
    }

    #[test]
    fn native_commit_is_a_no_op_not_a_refusal() {
        let native = gateway_provider_for(NATIVE_ID)
            .expect("native must resolve")
            .expect("native never fails to build");
        native.commit().expect("nothing staged is not an error");
    }

    #[test]
    fn unsupported_names_the_provider_and_the_operation() {
        let e = super::unsupported("acme", "ensure_rule").to_string();
        assert!(e.contains("ensure_rule"));
        assert!(e.contains("acme"));
    }
}

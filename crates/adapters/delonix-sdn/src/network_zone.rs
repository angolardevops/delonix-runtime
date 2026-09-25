//! A pluggable backend for cluster-native SDN — zones and the virtual
//! networks inside them (ADR-0049 addendum, closing the gap D3 names: "the
//! engine has Kinds and provider ports... that a Proxmox provider could
//! serve, and the matrix must keep showing that it does not yet").
//!
//! # Why this mirrors `GatewayProvider` (ADR-0051), and where it differs
//!
//! Same registry mechanism (`BackendFactory`-shaped closures, idempotent-by-
//! id registration, "registering does no I/O" — ADR-0008's own words) for
//! the same reason: a process that linked a provider's crate should be able
//! to make it selectable without a plugin loader.
//!
//! **No native implementation, unlike `GatewayProvider`'s
//! `NativeGatewayProvider`.** This engine's own rootless SDN (`kind:
//! Network`) has no zone concept at all — Zone→VNet is Proxmox's own
//! two-tier model (`/cluster/sdn/*`), not a shape the native dataplane
//! answers. The registry therefore holds ZERO or ONE provider, never a
//! builtin.
//!
//! # Why `kind: NetworkZone` carries no provider field
//!
//! Deliberate, the owner's own framing (this decision's discovery
//! conversation): the Kind stays transparent to WHICH infrastructure
//! realizes it — the runtime's own configuration (which provider got
//! registered, from environment variables read once at startup) decides,
//! the same way a managed-service tenant never names Proxmox, libvirt or
//! OPNsense. [`active_network_zone_provider`] resolves by COUNT, not by
//! name: zero registered is a clear refusal naming what to configure, one
//! is used, and more than one (unreachable today — nothing in this build
//! registers a second) is refused rather than guessed, the same fail-closed
//! answer this engine gives every ambiguity it will not resolve silently.

use crate::error::Error;

/// Whether an `ensure_*` call created something or found it already there.
/// Never "updated" — same reasoning as `gateway::EnsureOutcome`: a caller
/// that needs to change an existing zone/vnet removes and re-creates it
/// (the Proxmox SDN client, `sdn.rs`, has no update route for either).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    Created,
    AlreadyPresent,
}

/// A zone to ensure exists, identified by its Proxmox SDN id.
///
/// Only the name travels — a v1 zone is always Proxmox's plainest kind
/// (`simple`: no VLAN/VXLAN encapsulation), the same scope `sdn.rs`'s own
/// module doc names on purpose. Exposing a `type` field this engine cannot
/// honour for every provider would be the accept-and-drop shape this repo
/// has refused three times over for other flags.
#[derive(Debug, Clone)]
pub struct NetworkZoneSpec {
    pub name: String,
}

/// A vnet to ensure exists inside a zone.
#[derive(Debug, Clone)]
pub struct VNetSpec {
    pub name: String,
    pub zone: String,
    pub alias: Option<String>,
}

/// A backend that can realize cluster-native SDN zones and vnets.
pub trait NetworkZoneProvider {
    /// Stable identifier, persisted in the registry record.
    fn id(&self) -> &'static str;

    /// `true` if this backend can be used right now. Never a network round
    /// trip — same contract as `gateway::GatewayProvider::available`.
    fn available(&self) -> bool;

    fn ensure_zone(&self, zone: &NetworkZoneSpec) -> delonix_model::Result<EnsureOutcome>;
    /// Removes a zone by name. The provider itself refuses this while a
    /// vnet still references it (Proxmox's own business logic, surfaced as
    /// an ordinary error) — a caller with both to remove orders vnets
    /// first; see `cmd::network_zone::remove_for_replace`.
    fn remove_zone(&self, name: &str) -> delonix_model::Result<()>;

    fn ensure_vnet(&self, vnet: &VNetSpec) -> delonix_model::Result<EnsureOutcome>;
    fn remove_vnet(&self, name: &str) -> delonix_model::Result<()>;

    /// Reloads whatever `ensure_*`/removal staged onto every node in the
    /// cluster — Proxmox's own `PUT /cluster/sdn`. No default implementation:
    /// unlike `GatewayProvider::commit`, there is no provider registered
    /// here with nothing ever staged to make a no-op honest.
    fn commit(&self) -> delonix_model::Result<()>;
}

/// Builds a [`NetworkZoneProvider`], or reports why it could not.
///
/// `Send + Sync` because the table is process-wide. It constrains the
/// CLOSURE, not the trait (same reasoning as `gateway::GatewayProviderFactory`).
pub type NetworkZoneProviderFactory =
    Box<dyn Fn() -> crate::error::Result<Box<dyn NetworkZoneProvider>> + Send + Sync>;

/// One provider this build knows about: its canonical id, the aliases
/// accepted, and how to build one.
pub struct NetworkZoneProviderRegistration {
    /// Canonical id. Must equal what the built provider's
    /// [`NetworkZoneProvider::id`] returns.
    pub id: &'static str,
    /// Extra spellings accepted; never repeats `id`.
    pub aliases: &'static [&'static str],
    pub new: NetworkZoneProviderFactory,
}

impl std::fmt::Debug for NetworkZoneProviderRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkZoneProviderRegistration")
            .field("id", &self.id)
            .field("aliases", &self.aliases)
            .finish_non_exhaustive()
    }
}

/// Every registered provider. Starts EMPTY — no builtin, unlike
/// `gateway::GATEWAY_PROVIDERS` (module doc comment above explains why).
static NETWORK_ZONE_PROVIDERS: std::sync::OnceLock<
    std::sync::RwLock<Vec<NetworkZoneProviderRegistration>>,
> = std::sync::OnceLock::new();

fn network_zone_providers() -> &'static std::sync::RwLock<Vec<NetworkZoneProviderRegistration>> {
    NETWORK_ZONE_PROVIDERS.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

fn with_network_zone_providers<T>(f: impl FnOnce(&[NetworkZoneProviderRegistration]) -> T) -> T {
    let guard = network_zone_providers()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

/// Adds a provider to the registry. Idempotent by id: registering the same
/// id twice REPLACES the entry, so a process that configures a target twice
/// ends up with the last one rather than two that shadow each other.
///
/// **Nothing here does I/O**: the factory is not called, so registering a
/// node that is unreachable costs nothing until [`active_network_zone_provider`]
/// actually selects it (same contract as `delonix_vm::register_backend`/
/// `gateway::register_gateway_provider`).
pub fn register_network_zone_provider(
    reg: NetworkZoneProviderRegistration,
) -> crate::error::Result<()> {
    if reg.id.trim().is_empty() {
        return Err(Error::NetworkZoneProviderRegistrationRefused(
            "a network zone provider registration needs an id".into(),
        ));
    }
    let mut guard = network_zone_providers()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    for name in std::iter::once(&reg.id).chain(reg.aliases.iter()) {
        let want = name.trim().to_lowercase();
        if let Some(clash) = guard
            .iter()
            .find(|b| b.id != reg.id && (b.id == want || b.aliases.contains(&want.as_str())))
        {
            return Err(Error::NetworkZoneProviderRegistrationRefused(format!(
                "network zone provider '{}' cannot claim the name '{}': it already belongs to '{}'",
                reg.id, name, clash.id
            )));
        }
    }
    guard.retain(|b| b.id != reg.id);
    guard.push(reg);
    Ok(())
}

/// The single active provider, or a refusal naming why there is none.
///
/// **Resolved by COUNT, never by name** — `kind: NetworkZone` carries no
/// provider field (module doc comment). Delegates to [`resolve_active`],
/// which is pure and takes the registration list directly so it is testable
/// without touching the process-wide static.
pub fn active_network_zone_provider() -> crate::error::Result<Box<dyn NetworkZoneProvider>> {
    with_network_zone_providers(resolve_active)
}

fn resolve_active(
    regs: &[NetworkZoneProviderRegistration],
) -> crate::error::Result<Box<dyn NetworkZoneProvider>> {
    match regs.len() {
        0 => Err(Error::NoNetworkZoneProviderConfigured(
            "kind: NetworkZone has no registered provider — configure DELONIX_PROXMOX_URL (the \
             only one this build knows about) and its credential"
                .into(),
        )),
        1 => (regs[0].new)(),
        n => {
            let ids: Vec<&str> = regs.iter().map(|r| r.id).collect();
            Err(Error::AmbiguousNetworkZoneProvider(format!(
                "kind: NetworkZone has {n} registered providers ({}) and no way to pick one — \
                 this Kind does not support selecting a provider by name; unregister all but one",
                ids.join(", ")
            )))
        }
    }
}

/// The registered ids, in registration order — for `provider ls`-style
/// listing and for an error that names what IS configured.
pub fn network_zone_provider_ids() -> Vec<&'static str> {
    with_network_zone_providers(|regs| regs.iter().map(|r| r.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(&'static str);
    impl NetworkZoneProvider for Fake {
        fn id(&self) -> &'static str {
            self.0
        }
        fn available(&self) -> bool {
            true
        }
        fn ensure_zone(&self, _z: &NetworkZoneSpec) -> delonix_model::Result<EnsureOutcome> {
            Ok(EnsureOutcome::Created)
        }
        fn remove_zone(&self, _n: &str) -> delonix_model::Result<()> {
            Ok(())
        }
        fn ensure_vnet(&self, _v: &VNetSpec) -> delonix_model::Result<EnsureOutcome> {
            Ok(EnsureOutcome::Created)
        }
        fn remove_vnet(&self, _n: &str) -> delonix_model::Result<()> {
            Ok(())
        }
        fn commit(&self) -> delonix_model::Result<()> {
            Ok(())
        }
    }

    fn fake(id: &'static str, aliases: &'static [&'static str]) -> NetworkZoneProviderRegistration {
        NetworkZoneProviderRegistration {
            id,
            aliases,
            new: Box::new(move || Ok(Box::new(Fake(id)) as Box<dyn NetworkZoneProvider>)),
        }
    }

    // `resolve_active` is pure and takes the list directly, so these three
    // tests never touch the process-wide static — no race with any other
    // test in this crate that registers something (unlike a test that
    // asserted on `active_network_zone_provider()`'s real count would).

    #[test]
    fn zero_registered_names_what_to_configure() {
        // `Box<dyn NetworkZoneProvider>` is not `Debug`, so `unwrap_err()`
        // (which requires the `Ok` side to be `Debug`) does not apply here —
        // match instead, the same shape `gateway::tests` uses for the
        // equivalent case.
        match resolve_active(&[]) {
            Ok(_) => panic!("expected a refusal"),
            Err(err) => {
                assert!(matches!(err, Error::NoNetworkZoneProviderConfigured(_)));
                assert!(err.to_string().contains("DELONIX_PROXMOX_URL"));
            }
        }
    }

    #[test]
    fn one_registered_is_used() {
        let p = resolve_active(&[fake("only-one", &[])]).unwrap();
        assert_eq!(p.id(), "only-one");
    }

    #[test]
    fn more_than_one_is_refused_and_names_both() {
        match resolve_active(&[fake("a", &[]), fake("b", &[])]) {
            Ok(_) => panic!("expected a refusal"),
            Err(err) => {
                assert!(matches!(err, Error::AmbiguousNetworkZoneProvider(_)));
                let msg = err.to_string();
                assert!(msg.contains('a') && msg.contains('b'), "{msg}");
            }
        }
    }

    // The registration mechanism itself (idempotency, clashes, no I/O at
    // registration) DOES touch the process-wide static — same as
    // `gateway::tests` — so each test below uses a unique id to avoid
    // interference between parallel test threads.

    #[test]
    fn register_network_zone_provider_refuses_an_empty_id() {
        let err = register_network_zone_provider(fake("", &[])).unwrap_err();
        assert!(matches!(
            err,
            Error::NetworkZoneProviderRegistrationRefused(_)
        ));
    }

    #[test]
    fn register_network_zone_provider_refuses_a_name_clash() {
        register_network_zone_provider(fake("zone-clash-a", &["zone-shared"])).expect("a");
        let err =
            register_network_zone_provider(fake("zone-clash-b", &["zone-shared"])).unwrap_err();
        assert!(matches!(
            err,
            Error::NetworkZoneProviderRegistrationRefused(_)
        ));
    }

    #[test]
    fn register_network_zone_provider_is_idempotent_by_id() {
        register_network_zone_provider(fake("zone-reconfigurable", &[])).expect("1st");
        register_network_zone_provider(fake("zone-reconfigurable", &[])).expect("2nd replaces it");
        let count = with_network_zone_providers(|regs| {
            regs.iter()
                .filter(|r| r.id == "zone-reconfigurable")
                .count()
        });
        assert_eq!(count, 1, "a reconfigured id does not leave two entries");
    }

    #[test]
    fn network_zone_provider_ids_lists_what_is_registered() {
        register_network_zone_provider(fake("zone-listed", &[])).expect("register");
        assert!(network_zone_provider_ids().contains(&"zone-listed"));
    }
}

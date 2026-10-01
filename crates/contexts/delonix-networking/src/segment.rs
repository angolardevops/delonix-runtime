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
//! **No native implementation.** This engine's own rootless SDN (`kind:
//! Network`) has no zone concept at all — Zone→VNet is Proxmox's own
//! two-tier model (`/cluster/sdn/*`), not a shape the native dataplane
//! answers. The registry therefore holds ZERO or ONE provider, never a
//! builtin (the gateway registry has had none either since ADR-0059 F2b).
//!
//! # Why `kind: NetworkZone` carries no provider field
//!
//! Deliberate, the owner's own framing (this decision's discovery
//! conversation): the Kind stays transparent to WHICH infrastructure
//! realizes it — the runtime's own configuration (which provider got
//! registered, from environment variables read once at startup) decides,
//! the same way a managed-service tenant never names Proxmox, libvirt or
//! OPNsense. Which registered provider answers is [`crate::resolve`]'s job
//! (ADR-0059 D3): the provider on the zone's record, then
//! `networkDefaults.segment`, then — only without a `providers.yaml` — the
//! single registered one, the count rule a development node always had.

use crate::error::Error;
use crate::ownership::{OwnerMark, RemoveOutcome};

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

/// A vnet to ensure exists inside a zone. `alias` is the declared,
/// human-readable part; the provider writes it followed by the caller's
/// [`OwnerMark`] — the vnet's only free-text field is where the mark lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VNetSpec {
    pub name: String,
    pub zone: String,
    pub alias: Option<String>,
}

/// What a segment provider holds for one zone under one owner mark, read
/// back from the cluster (ADR-0059 D4, observe): whether the zone exists, and
/// every vnet carrying the mark, with its alias as declared (the mark taken
/// off).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentObserved {
    pub zone_present: bool,
    pub vnets: Vec<VNetSpec>,
}

/// How what a provider holds differs from what a record declared, one line
/// per difference, sorted. Empty = in sync. Pure.
///
/// The zone is expected to exist whenever the record has been applied. A
/// vnet carrying the mark that nothing declares is a difference too: nobody
/// else will remove it. An absent alias and an empty one are the same.
pub fn segment_drift(zone: &str, vnets: &[VNetSpec], observed: &SegmentObserved) -> Vec<String> {
    let alias = |a: &Option<String>| a.as_deref().unwrap_or("").trim().to_string();
    let mut out = Vec::new();
    if !observed.zone_present {
        out.push(format!("zone '{zone}' is missing"));
    }
    for want in vnets {
        match observed.vnets.iter().find(|v| v.name == want.name) {
            None => out.push(format!("vnet '{}' is missing", want.name)),
            Some(have) => {
                if have.zone != want.zone {
                    out.push(format!(
                        "vnet '{}' is in zone '{}', declared '{}'",
                        want.name, have.zone, want.zone
                    ));
                }
                if alias(&have.alias) != alias(&want.alias) {
                    out.push(format!(
                        "vnet '{}' alias is '{}', declared '{}'",
                        want.name,
                        alias(&have.alias),
                        alias(&want.alias)
                    ));
                }
            }
        }
    }
    for have in &observed.vnets {
        if !vnets.iter().any(|v| v.name == have.name) {
            out.push(format!(
                "vnet '{}' carries this engine's mark and is not declared",
                have.name
            ));
        }
    }
    out.sort();
    out
}

/// A backend that can realize cluster-native SDN zones and vnets.
/// Extends the provider skeleton (ADR-0059 D1 rule 4): `id`, `capabilities()`
/// — the ADR-0050 report — and `health`.
pub trait SegmentProvider: delonix_compute::vm_provider::Provider {
    /// `true` if this backend can be used right now. Never a network round
    /// trip — same contract as `gateway::GatewayProvider::available`.
    fn available(&self) -> bool;

    /// Ensures a zone exists. A zone has no free-text field to carry an
    /// [`OwnerMark`] (Proxmox VE 9.2.2: `pvesh usage /cluster/sdn/zones`
    /// lists none), so this answers only whether it was there: whether an
    /// `AlreadyPresent` zone is the CALLER's is the caller's own record to
    /// decide (`cmd::network_zone` refuses one it did not create).
    fn ensure_zone(&self, zone: &NetworkZoneSpec) -> delonix_model::Result<EnsureOutcome>;
    /// Removes a zone by name. The provider itself refuses this while a
    /// vnet still references it (Proxmox's own business logic, surfaced as
    /// an ordinary error) — a caller with both to remove orders vnets
    /// first; see `cmd::network_zone::remove_for_replace`. Only a zone the
    /// caller's record says it created is ever passed here.
    fn remove_zone(&self, name: &str) -> delonix_model::Result<()>;

    /// Ensures a vnet exists, OWNED by `owner` (its alias carries the mark):
    /// one of that name without the mark is refused
    /// (`RemoteObjectNotOwned`); one with it, in another zone or with another
    /// alias, is drift (`RemoteObjectDrifted`).
    fn ensure_vnet(
        &self,
        vnet: &VNetSpec,
        owner: &OwnerMark,
    ) -> delonix_model::Result<EnsureOutcome>;
    /// Removes a vnet only when `owner` owns it; anything else of that name
    /// is left alone and reported.
    fn remove_vnet(&self, name: &str, owner: &OwnerMark) -> delonix_model::Result<RemoveOutcome>;

    /// Reads back whether `zone` exists and every vnet carrying `owner`'s
    /// mark (ADR-0059 D4, observe). Read-only: it takes no lock and stages
    /// nothing.
    fn observe(&self, zone: &str, owner: &OwnerMark) -> delonix_model::Result<SegmentObserved>;

    /// Runs `change` — the `ensure_*`/`remove_*` calls of one apply or one
    /// teardown — as ONE unit on the cluster, and makes it live:
    ///
    /// * refused BEFORE `change` runs when the cluster already carries
    ///   staged changes nobody applied, or someone else holds the SDN lock
    ///   (an apply pushes everything staged, not only this caller's);
    /// * `change` failing discards what it staged, so nothing half-done is
    ///   ever applied;
    /// * `change` succeeding applies it (Proxmox's `PUT /cluster/sdn`).
    ///
    /// No default implementation: there is no provider here with nothing
    /// ever staged to make a plain `change()` honest.
    fn transaction(
        &self,
        change: &mut dyn FnMut() -> delonix_model::Result<()>,
    ) -> delonix_model::Result<()>;
}

/// Builds a [`SegmentProvider`], or reports why it could not.
///
/// `Send + Sync` because the table is process-wide. It constrains the
/// CLOSURE, not the trait (same reasoning as `gateway::GatewayProviderFactory`).
pub type SegmentProviderFactory =
    Box<dyn Fn() -> crate::error::Result<Box<dyn SegmentProvider>> + Send + Sync>;

/// One provider this build knows about: its canonical id, the aliases
/// accepted, and how to build one.
pub struct SegmentProviderRegistration {
    /// Canonical id. Must equal what the built provider's
    /// [`SegmentProvider::id`] returns.
    pub id: &'static str,
    /// Extra spellings accepted; never repeats `id`.
    pub aliases: &'static [&'static str],
    pub new: SegmentProviderFactory,
}

impl std::fmt::Debug for SegmentProviderRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentProviderRegistration")
            .field("id", &self.id)
            .field("aliases", &self.aliases)
            .finish_non_exhaustive()
    }
}

/// Every registered provider. Starts EMPTY — no builtin, unlike
/// `gateway::GATEWAY_PROVIDERS` (module doc comment above explains why).
static NETWORK_ZONE_PROVIDERS: std::sync::OnceLock<
    std::sync::RwLock<Vec<SegmentProviderRegistration>>,
> = std::sync::OnceLock::new();

fn segment_providers() -> &'static std::sync::RwLock<Vec<SegmentProviderRegistration>> {
    NETWORK_ZONE_PROVIDERS.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

fn with_segment_providers<T>(f: impl FnOnce(&[SegmentProviderRegistration]) -> T) -> T {
    let guard = segment_providers()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

/// Adds a provider to the registry. Idempotent by id: registering the same
/// id twice REPLACES the entry, so a process that configures a target twice
/// ends up with the last one rather than two that shadow each other.
///
/// **Nothing here does I/O**: the factory is not called, so registering a
/// node that is unreachable costs nothing until
/// [`choose_segment_provider`] actually selects it (same contract as `delonix_vm::register_backend`/
/// `gateway::register_gateway_provider`).
pub fn register_segment_provider(reg: SegmentProviderRegistration) -> crate::error::Result<()> {
    if reg.id.trim().is_empty() {
        return Err(Error::NetworkZoneProviderRegistrationRefused(
            "a network zone provider registration needs an id".into(),
        ));
    }
    let mut guard = segment_providers()
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

/// The provider that answers `want` (ADR-0059 D3, [`crate::resolve`]),
/// with the id it resolved to — the caller records it, so the resource
/// never moves when a default changes.
///
/// Building happens only after the choice: resolving does no I/O.
pub fn choose_segment_provider(
    want: &crate::resolve::Wanted,
) -> crate::error::Result<(&'static str, Box<dyn SegmentProvider>)> {
    with_segment_providers(|regs| choose_in(regs, want))
}

fn choose_in(
    regs: &[SegmentProviderRegistration],
    want: &crate::resolve::Wanted,
) -> crate::error::Result<(&'static str, Box<dyn SegmentProvider>)> {
    let cands: Vec<crate::resolve::Candidate> = regs.iter().map(|r| (r.id, r.aliases)).collect();
    let id = crate::resolve::choose(&cands, want)?;
    let reg = regs
        .iter()
        .find(|r| r.id == id)
        .expect("choose returns a registered id");
    Ok((id, (reg.new)()?))
}

/// The registered ids, in registration order — for `provider ls`-style
/// listing and for an error that names what IS configured.
pub fn segment_provider_ids() -> Vec<&'static str> {
    with_segment_providers(|regs| regs.iter().map(|r| r.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                ProviderKind::Network,
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
    impl SegmentProvider for Fake {
        fn available(&self) -> bool {
            true
        }
        fn ensure_zone(&self, _z: &NetworkZoneSpec) -> delonix_model::Result<EnsureOutcome> {
            Ok(EnsureOutcome::Created)
        }
        fn remove_zone(&self, _n: &str) -> delonix_model::Result<()> {
            Ok(())
        }
        fn ensure_vnet(
            &self,
            _v: &VNetSpec,
            _o: &OwnerMark,
        ) -> delonix_model::Result<EnsureOutcome> {
            Ok(EnsureOutcome::Created)
        }
        fn remove_vnet(&self, _n: &str, _o: &OwnerMark) -> delonix_model::Result<RemoveOutcome> {
            Ok(RemoveOutcome::Removed)
        }
        fn observe(&self, _z: &str, _o: &OwnerMark) -> delonix_model::Result<SegmentObserved> {
            Ok(SegmentObserved::default())
        }
        fn transaction(
            &self,
            change: &mut dyn FnMut() -> delonix_model::Result<()>,
        ) -> delonix_model::Result<()> {
            change()
        }
    }

    fn fake(id: &'static str, aliases: &'static [&'static str]) -> SegmentProviderRegistration {
        SegmentProviderRegistration {
            id,
            aliases,
            new: Box::new(move || Ok(Box::new(Fake(id)) as Box<dyn SegmentProvider>)),
        }
    }

    // `choose_in` takes the list directly, so these tests never touch the
    // process-wide static — no race with any other test in this crate that
    // registers something. The resolution rules themselves are
    // `crate::resolve`'s tests; these check that the registry builds what
    // the rule chose.

    fn no_config() -> crate::resolve::Wanted<'static> {
        crate::resolve::Wanted {
            role: crate::resolve::Role::Segment,
            named: None,
            recorded: None,
            default: None,
            config: None,
        }
    }

    #[test]
    fn zero_registered_names_what_to_configure() {
        // `Box<dyn SegmentProvider>` is not `Debug`, so `unwrap_err()` does
        // not apply — match instead.
        match choose_in(&[], &no_config()) {
            Ok(_) => panic!("expected a refusal"),
            Err(err) => {
                assert!(matches!(err, Error::NoNetworkZoneProviderConfigured(_)));
                assert!(err.to_string().contains("DELONIX_PROXMOX_URL"));
            }
        }
    }

    #[test]
    fn one_registered_is_used() {
        let (id, p) = choose_in(&[fake("only-one", &[])], &no_config()).unwrap();
        assert_eq!((id, p.id().0), ("only-one", "only-one"));
    }

    #[test]
    fn two_registered_and_a_default_builds_the_one_it_names() {
        let want = crate::resolve::Wanted {
            default: Some("b"),
            config: Some("/p.yaml"),
            ..no_config()
        };
        let (id, p) = choose_in(&[fake("a", &[]), fake("b", &[])], &want).unwrap();
        assert_eq!((id, p.id().0), ("b", "b"));
    }

    #[test]
    fn more_than_one_without_a_config_is_refused_and_names_both() {
        match choose_in(&[fake("a", &[]), fake("b", &[])], &no_config()) {
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
    fn register_segment_provider_refuses_an_empty_id() {
        let err = register_segment_provider(fake("", &[])).unwrap_err();
        assert!(matches!(
            err,
            Error::NetworkZoneProviderRegistrationRefused(_)
        ));
    }

    #[test]
    fn register_segment_provider_refuses_a_name_clash() {
        register_segment_provider(fake("zone-clash-a", &["zone-shared"])).expect("a");
        let err = register_segment_provider(fake("zone-clash-b", &["zone-shared"])).unwrap_err();
        assert!(matches!(
            err,
            Error::NetworkZoneProviderRegistrationRefused(_)
        ));
    }

    #[test]
    fn register_segment_provider_is_idempotent_by_id() {
        register_segment_provider(fake("zone-reconfigurable", &[])).expect("1st");
        register_segment_provider(fake("zone-reconfigurable", &[])).expect("2nd replaces it");
        let count = with_segment_providers(|regs| {
            regs.iter()
                .filter(|r| r.id == "zone-reconfigurable")
                .count()
        });
        assert_eq!(count, 1, "a reconfigured id does not leave two entries");
    }

    #[test]
    fn segment_provider_ids_lists_what_is_registered() {
        register_segment_provider(fake("zone-listed", &[])).expect("register");
        assert!(segment_provider_ids().contains(&"zone-listed"));
    }

    fn vnet(name: &str, zone: &str, alias: Option<&str>) -> VNetSpec {
        VNetSpec {
            name: name.into(),
            zone: zone.into(),
            alias: alias.map(str::to_string),
        }
    }

    #[test]
    fn a_zone_that_matches_is_in_sync_and_an_empty_alias_is_no_alias() {
        let observed = SegmentObserved {
            zone_present: true,
            vnets: vec![vnet("v1", "z", None), vnet("v2", "z", Some("app tier"))],
        };
        let declared = [
            vnet("v2", "z", Some(" app tier ")),
            vnet("v1", "z", Some("")),
        ];
        assert!(segment_drift("z", &declared, &observed).is_empty());
    }

    #[test]
    fn every_difference_of_a_zone_is_named() {
        let observed = SegmentObserved {
            zone_present: false,
            vnets: vec![vnet("moved", "other", Some("x")), vnet("stray", "z", None)],
        };
        let declared = [vnet("moved", "z", Some("y")), vnet("gone", "z", None)];
        assert_eq!(
            segment_drift("z", &declared, &observed),
            [
                "vnet 'gone' is missing",
                "vnet 'moved' alias is 'x', declared 'y'",
                "vnet 'moved' is in zone 'other', declared 'z'",
                "vnet 'stray' carries this engine's mark and is not declared",
                "zone 'z' is missing",
            ]
        );
    }
}

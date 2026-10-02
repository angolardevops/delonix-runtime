//! The NAT role (ADR-0059 D1, F5): address translation done by a remote
//! provider at the perimeter — source NAT for a network leaving through an
//! interface, destination NAT forwarding a port of an interface to a host.
//!
//! **Not the node's own masquerade and publish.** Those stay in
//! `delonix-sdn`, unconditional and local; the Linux network report answers
//! `net.nat.*` for them. This port is for a provider that holds NAT rules as
//! objects of its own, found by description and owned by a mark.
//!
//! The first implementation is OPNsense (`firewall/source_nat`,
//! `firewall/d_nat`). The model is the part of those two this engine can
//! read back from the running packet filter, and nothing more: a source NAT
//! rule always has a source network, and a destination NAT rule always has a
//! protocol, a port and a target address. IPv4 only.

use crate::error::{Error, Result};
use crate::gateway::EnsureOutcome;
use crate::ownership::{OwnerMark, RemoveOutcome};

/// Which translation a [`NatRule`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatKind {
    /// Packets from `source` leaving through `interface` take `target` as
    /// their source address.
    Source,
    /// Packets to `port` of `interface`'s address are forwarded to
    /// `target`:`target_port`.
    Destination,
}

impl NatKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NatKind::Source => "snat",
            NatKind::Destination => "dnat",
        }
    }
}

/// What a source NAT rule translates to.
pub const INTERFACE_ADDRESS: &str = "interface-address";

/// One NAT rule to ensure exists, found by its DESCRIPTION, as a gateway
/// rule is. A rule with that description and without the caller's
/// [`OwnerMark`] is someone else's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NatRule {
    pub description: String,
    pub kind: NatKind,
    /// The provider's own name for the interface (OPNsense: `lan`, `wan`,
    /// `opt1`).
    pub interface: String,
    /// Source NAT: the network translated, an IPv4 CIDR at its network
    /// address. Destination NAT: who may use the forward, a CIDR or `any`.
    pub source: String,
    /// Destination NAT only: `tcp` or `udp`.
    pub protocol: Option<String>,
    /// Destination NAT only: the port on the interface's address.
    pub port: Option<u16>,
    /// Source NAT: [`INTERFACE_ADDRESS`] or an IPv4 address. Destination
    /// NAT: the IPv4 address forwarded to.
    pub target: String,
    /// Destination NAT only: the port on `target`.
    pub target_port: Option<u16>,
}

fn ipv4(s: &str) -> Option<std::net::Ipv4Addr> {
    s.parse().ok()
}

/// An IPv4 CIDR at its network address (`10.77.0.0/24`, never
/// `10.77.0.5/24`): the form the packet filter prints, which is how a rule
/// is recognized as loaded.
fn canonical_cidr(s: &str) -> bool {
    let Some((addr, len)) = s.split_once('/') else {
        return false;
    };
    let (Some(addr), Ok(len)) = (ipv4(addr), len.parse::<u32>()) else {
        return false;
    };
    if len > 32 {
        return false;
    }
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    u32::from(addr) & mask == u32::from(addr)
}

impl NatRule {
    /// Refuses what the model does not hold, before any provider is
    /// reached. Each refusal names the field.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: String| {
            Err(Error::Engine(delonix_model::Error::Invalid(format!(
                "nat rule '{}': {what}",
                self.description
            ))))
        };
        if self.description.trim().is_empty() {
            return bad("a description is required — it is how the rule is found".into());
        }
        if self.interface.is_empty()
            || !self
                .interface
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return bad(format!(
                "interface '{}' is not a provider interface name (letters, digits, '_')",
                self.interface
            ));
        }
        match self.kind {
            NatKind::Source => {
                if !canonical_cidr(&self.source) {
                    return bad(format!(
                        "source '{}' has to be an IPv4 network at its network address, like 10.77.0.0/24",
                        self.source
                    ));
                }
                if self.target != INTERFACE_ADDRESS && ipv4(&self.target).is_none() {
                    return bad(format!(
                        "target '{}' has to be '{INTERFACE_ADDRESS}' or an IPv4 address",
                        self.target
                    ));
                }
                if self.protocol.is_some() || self.port.is_some() || self.target_port.is_some() {
                    return bad(
                        "a source NAT rule has no protocol, port or targetPort — it translates every packet from the source"
                            .into(),
                    );
                }
            }
            NatKind::Destination => {
                if self.source != "any" && !canonical_cidr(&self.source) {
                    return bad(format!(
                        "source '{}' has to be 'any' or an IPv4 network at its network address",
                        self.source
                    ));
                }
                match self.protocol.as_deref() {
                    Some("tcp") | Some("udp") => {}
                    other => {
                        return bad(format!(
                            "protocol '{}' has to be tcp or udp",
                            other.unwrap_or("")
                        ))
                    }
                }
                if self.port.unwrap_or(0) == 0 {
                    return bad("a destination NAT rule needs the port it forwards".into());
                }
                if self.target_port.unwrap_or(0) == 0 {
                    return bad("a destination NAT rule needs the targetPort it forwards to".into());
                }
                if ipv4(&self.target).is_none() {
                    return bad(format!(
                        "target '{}' has to be an IPv4 address",
                        self.target
                    ));
                }
            }
        }
        Ok(())
    }
}

/// The NAT rules a provider holds under one owner mark, read back
/// (ADR-0059 D4, observe), and the descriptions of those it has disabled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NatObserved {
    pub rules: Vec<NatRule>,
    pub disabled: Vec<String>,
}

/// How what a provider holds under a mark differs from what was declared,
/// one line per difference, sorted. Empty = in sync. Pure.
pub fn nat_drift(declared: &[NatRule], observed: &NatObserved) -> Vec<String> {
    let mut out = Vec::new();
    for want in declared {
        let d = &want.description;
        let Some(have) = observed.rules.iter().find(|r| &r.description == d) else {
            out.push(format!("nat rule '{d}' is missing"));
            continue;
        };
        let mut field = |name: &str, have: String, want: String| {
            if have != want {
                out.push(format!(
                    "nat rule '{d}' {name} is '{have}', declared '{want}'"
                ));
            }
        };
        let port = |p: Option<u16>| p.map(|n| n.to_string()).unwrap_or_default();
        field("kind", have.kind.as_str().into(), want.kind.as_str().into());
        field("interface", have.interface.clone(), want.interface.clone());
        field("source", have.source.clone(), want.source.clone());
        field(
            "protocol",
            have.protocol.clone().unwrap_or_default(),
            want.protocol.clone().unwrap_or_default(),
        );
        field("port", port(have.port), port(want.port));
        field("target", have.target.clone(), want.target.clone());
        field("targetPort", port(have.target_port), port(want.target_port));
        if observed.disabled.contains(d) {
            out.push(format!("nat rule '{d}' is disabled on the provider"));
        }
    }
    for have in &observed.rules {
        if !declared.iter().any(|r| r.description == have.description) {
            out.push(format!(
                "nat rule '{}' carries this engine's mark and is not declared",
                have.description
            ));
        }
    }
    out.sort();
    out
}

/// A backend that holds NAT rules as objects of its own. Extends the
/// provider skeleton (ADR-0059 D1 rule 4). Every method is required
/// (rule 1).
pub trait NatProvider: delonix_compute::vm_provider::Provider {
    /// `true` if this backend can be used right now. Never a network round
    /// trip.
    fn available(&self) -> bool;

    /// Ensures a NAT rule exists, by description, OWNED by `owner`. One
    /// found under that description without the mark is refused
    /// (`RemoteObjectNotOwned`); one with the mark whose content differs is
    /// refused too (`RemoteObjectDrifted`).
    fn ensure_nat(&self, rule: &NatRule, owner: &OwnerMark)
        -> delonix_model::Result<EnsureOutcome>;

    /// Removes the NAT rules with this description that `owner` owns; one
    /// that matches without the mark is left alone and reported.
    fn remove_nat(
        &self,
        description: &str,
        owner: &OwnerMark,
    ) -> delonix_model::Result<RemoveOutcome>;

    /// Retires `owner` on the provider once a teardown removed everything
    /// it marked.
    fn release_owner(&self, owner: &OwnerMark) -> delonix_model::Result<RemoveOutcome>;

    /// Reads back every NAT rule carrying `owner`'s mark. Read-only.
    fn observe(&self, owner: &OwnerMark) -> delonix_model::Result<NatObserved>;

    /// Takes over the NAT rules carrying `owner`'s mark that a run which
    /// DIED staged and never applied (ADR-0059 D4). Returns what it adopted,
    /// for a message.
    fn adopt_pending(&self, owner: &OwnerMark) -> delonix_model::Result<Vec<String>>;

    /// Refuses (`RemoteForeignPending`) when the provider already carries
    /// staged NAT rules nobody applied — called BEFORE the first staged
    /// write. The [`commit`](Self::commit) checks again, and wider: a
    /// provider whose apply pushes everything refuses there for any staged
    /// change that is not this value's.
    fn check_no_foreign_pending(&self) -> delonix_model::Result<()>;

    /// Activates what `ensure_nat`/`remove_nat` staged, and proves it: a
    /// rule this value created has to be loaded afterwards, and one it
    /// removed has to be gone.
    fn commit(&self) -> delonix_model::Result<()>;
}

/// Builds a [`NatProvider`], or reports why it could not.
pub type NatProviderFactory = Box<dyn Fn() -> Result<Box<dyn NatProvider>> + Send + Sync>;

/// One provider registered for the NAT role (ADR-0059 D1 rule 2).
pub struct NatProviderRegistration {
    /// Canonical id. Must equal what the built provider's `id` returns.
    pub id: &'static str,
    /// Extra spellings accepted from a caller; never repeats `id`.
    pub aliases: &'static [&'static str],
    pub new: NatProviderFactory,
}

static NAT_PROVIDERS: std::sync::OnceLock<std::sync::RwLock<Vec<NatProviderRegistration>>> =
    std::sync::OnceLock::new();

fn nat_providers() -> &'static std::sync::RwLock<Vec<NatProviderRegistration>> {
    NAT_PROVIDERS.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

fn with_nat_providers<T>(f: impl FnOnce(&[NatProviderRegistration]) -> T) -> T {
    let guard = nat_providers().read().unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

/// Adds a provider to the NAT registry. Idempotent by id; does no I/O (the
/// factory is not called). An empty id, or a name that already belongs to a
/// DIFFERENT provider, is refused — the same four rules as the gateway
/// registry, and the same refusal variant: both are `invalid` registrations
/// of a network role.
pub fn register_nat_provider(reg: NatProviderRegistration) -> Result<()> {
    if reg.id.trim().is_empty() {
        return Err(Error::GatewayProviderRegistrationRefused(
            "a nat provider registration needs an id".into(),
        ));
    }
    let mut guard = nat_providers().write().unwrap_or_else(|e| e.into_inner());
    for name in std::iter::once(&reg.id).chain(reg.aliases.iter()) {
        let want = name.trim().to_lowercase();
        if let Some(clash) = guard
            .iter()
            .find(|b| b.id != reg.id && (b.id == want || b.aliases.contains(&want.as_str())))
        {
            return Err(Error::GatewayProviderRegistrationRefused(format!(
                "nat provider '{}' cannot claim the name '{}': it already belongs to '{}'",
                reg.id, name, clash.id
            )));
        }
    }
    guard.retain(|b| b.id != reg.id);
    guard.push(reg);
    Ok(())
}

/// Builds the NAT provider registered under `name` (id or alias), or `None`
/// when nothing is. A document's NAT rules are served by the provider that
/// serves the document, so the lookup is by that provider's id: one that is
/// not in this registry does not have the role (ADR-0059 D1 rule 2).
pub fn nat_provider_for(name: &str) -> Option<Result<Box<dyn NatProvider>>> {
    let want = name.trim().to_lowercase();
    with_nat_providers(|regs| {
        regs.iter()
            .find(|r| r.id == want || r.aliases.contains(&want.as_str()))
            .map(|r| (r.new)())
    })
}

/// The registered ids, in registration order.
pub fn nat_provider_ids() -> Vec<&'static str> {
    with_nat_providers(|regs| regs.iter().map(|r| r.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snat() -> NatRule {
        NatRule {
            description: "out".into(),
            kind: NatKind::Source,
            interface: "wan".into(),
            source: "10.77.0.0/24".into(),
            protocol: None,
            port: None,
            target: INTERFACE_ADDRESS.into(),
            target_port: None,
        }
    }

    fn dnat() -> NatRule {
        NatRule {
            description: "web".into(),
            kind: NatKind::Destination,
            interface: "wan".into(),
            source: "any".into(),
            protocol: Some("tcp".into()),
            port: Some(8443),
            target: "10.77.0.10".into(),
            target_port: Some(443),
        }
    }

    #[test]
    fn the_two_shapes_validate() {
        snat().validate().unwrap();
        dnat().validate().unwrap();
    }

    #[test]
    fn each_refusal_names_its_field() {
        let refused = |r: NatRule, needle: &str| {
            let e = r.validate().unwrap_err().to_string();
            assert!(e.contains(needle), "{e}");
        };
        refused(
            NatRule {
                source: "10.77.0.5/24".into(),
                ..snat()
            },
            "network address",
        );
        refused(
            NatRule {
                source: "any".into(),
                ..snat()
            },
            "source 'any'",
        );
        refused(
            NatRule {
                source: "fd00::/64".into(),
                ..snat()
            },
            "IPv4",
        );
        refused(
            NatRule {
                target: "wan".into(),
                ..snat()
            },
            "target 'wan'",
        );
        refused(
            NatRule {
                port: Some(80),
                ..snat()
            },
            "no protocol, port",
        );
        refused(
            NatRule {
                interface: "wan;x".into(),
                ..snat()
            },
            "interface",
        );
        refused(
            NatRule {
                description: " ".into(),
                ..snat()
            },
            "description",
        );
        refused(
            NatRule {
                protocol: Some("icmp".into()),
                ..dnat()
            },
            "tcp or udp",
        );
        refused(
            NatRule {
                protocol: None,
                ..dnat()
            },
            "tcp or udp",
        );
        refused(
            NatRule {
                port: None,
                ..dnat()
            },
            "needs the port",
        );
        refused(
            NatRule {
                target_port: None,
                ..dnat()
            },
            "targetPort",
        );
        refused(
            NatRule {
                target: INTERFACE_ADDRESS.into(),
                ..dnat()
            },
            "IPv4 address",
        );
    }

    #[test]
    fn drift_names_what_differs_and_what_nobody_declared() {
        let declared = [snat(), dnat()];
        let same = NatObserved {
            rules: declared.to_vec(),
            disabled: vec![],
        };
        assert!(nat_drift(&declared, &same).is_empty());

        let observed = NatObserved {
            rules: vec![
                NatRule {
                    target_port: Some(8080),
                    ..dnat()
                },
                NatRule {
                    description: "left".into(),
                    ..snat()
                },
            ],
            disabled: vec!["web".into()],
        };
        assert_eq!(
            nat_drift(&declared, &observed),
            vec![
                "nat rule 'left' carries this engine's mark and is not declared".to_string(),
                "nat rule 'out' is missing".to_string(),
                "nat rule 'web' is disabled on the provider".to_string(),
                "nat rule 'web' targetPort is '8080', declared '443'".to_string(),
            ]
        );
    }
}

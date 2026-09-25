//! A VM's own firewall, as the engine asks for it (ADR-0052).
//!
//! The per-workload firewall of a CONTAINER is the Linux provider's nftables
//! chain in the holder (`delonix-sdn`). A VM on a REMOTE node is out of that
//! chain's reach: its traffic never crosses this host. What reaches it is the
//! firewall of the node it runs on, and [`VmBackend::apply_firewall`] is the
//! port through which a backend offers it. A backend without one refuses by
//! name (the default method), never accepts and ignores.
//!
//! One [`VmFirewallPolicy`] is the WHOLE desired state of one direction of one
//! VM — the same contract `kind: NetworkPolicy` has for a container: applying
//! it replaces what the engine wrote for that direction before, and leaves the
//! other direction alone.
//!
//! [`VmBackend::apply_firewall`]: crate::VmBackend::apply_firewall

use std::fmt;

/// Inbound or outbound, from the VM's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

impl Direction {
    /// `in`/`out` — the words both `ContainerFw` and the Proxmox API use.
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The transport a rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
    /// Any protocol. With a port it means TCP **and** UDP — the meaning
    /// `net ingress allow <c> 80` has for a container.
    Any,
}

impl Proto {
    pub fn as_str(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
            Proto::Any => "any",
        }
    }

    /// Parses `tcp`/`udp`/`any`. Anything else is refused — a protocol a
    /// backend would silently widen to "any" is a rule that allows more than
    /// it says.
    pub fn parse(s: &str) -> Option<Proto> {
        match s {
            "tcp" => Some(Proto::Tcp),
            "udp" => Some(Proto::Udp),
            "any" => Some(Proto::Any),
            _ => None,
        }
    }
}

/// One rule. `port: None` is every port; `peer: None` is every address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// `true` = allow, `false` = deny.
    pub allow: bool,
    pub proto: Proto,
    /// A port or an `n-m` range, already validated by the caller.
    pub port: Option<String>,
    /// The OTHER end: the source of an inbound rule, the destination of an
    /// outbound one. An IPv4 address or CIDR.
    pub peer: Option<String>,
}

impl Rule {
    /// The comparable rendering the reconciler keys on:
    /// `action|proto|port|peer|`. The same shape `cmd::firewall` renders a
    /// container's stored rule in, so a VM's plan reads like a container's.
    pub fn key(&self) -> String {
        format!(
            "{}|{}|{}|{}|",
            if self.allow { "allow" } else { "deny" },
            self.proto.as_str(),
            self.port.as_deref().unwrap_or("*"),
            self.peer.as_deref().unwrap_or("0.0.0.0/0"),
        )
    }
}

/// The desired (or observed) state of one direction of one VM's firewall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub direction: Direction,
    /// What happens to traffic no rule matches. `false` = drop.
    pub default_allow: bool,
    /// In evaluation order: the first match wins.
    pub rules: Vec<Rule>,
}

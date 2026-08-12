//! `kind: NetworkRoute` — a DIRECTED path between two networks (ADR-0013 tier B).
//!
//! Networks are isolated from each other by default. This declares that one may
//! reach another, and it compiles to ONE element in the holder's `@netpair`
//! verdict map — the spike behind the ADR proved the forwarding already exists
//! and that an explicit pairwise drop is what closes it, so opening a path is an
//! exemption and not a dataplane.
//!
//! **A route says a packet MAY cross; it never says it is allowed.** The
//! per-workload `fwcont` chains still decide, and a namespace boundary crossed
//! by a route still needs its own `kind: Dependency` or policy. That is what
//! makes this compose with the isolation model instead of undermining it.
//!
//! Its own document type rather than a `routes:` field inside `kind: Network`,
//! because a route is a RELATIONSHIP and belongs to neither end: expressible
//! from both sides is how two documents come to disagree about one route — the
//! bug `FirewallPolicy` already pays for by REFUSING two policies for the same
//! target and direction.

use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use delonix_runtime_core::Result;

/// `spec` of `kind: NetworkRoute`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct NetworkRouteSpec {
    /// Network that may INITIATE. Its workloads reach `to`.
    pub from: String,
    /// Network reached. Its workloads do NOT get to reach `from` back — the
    /// return traffic of a conversation `from` started flows (established), a
    /// new one from this side does not. Same asymmetry `kind: Dependency` gives
    /// between containers.
    pub to: String,
}

/// Known fields of the `spec` (drift-guard).
pub const NETWORK_ROUTE_SPEC_FIELDS: &[&str] = &["from", "to"];

/// Dry-run: the spec with every `#[serde(default)]` materialized.
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: NetworkRouteSpec = manifest::spec_of(doc)?;
    serde_yaml::to_value(spec)
        .map_err(|e| delonix_runtime_core::Error::Invalid(format!("dry-run: {e}")))
}

/// Applies the `kind: NetworkRoute` documents.
///
/// Idempotent, like every other apply here: `nft add element` on an element that
/// is already there is a no-op, so re-applying a manifest opens nothing twice.
pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, "NetworkRoute") {
        let spec: NetworkRouteSpec = manifest::spec_of(doc)?;
        delonix_net::infra::network_route(&spec.from, &spec.to, true)?;
        println!(
            "{}",
            super::po::tf(
                "networkroute/{name}: {from} -> {to} open",
                &[
                    ("name", &doc.metadata.name),
                    ("from", &spec.from),
                    ("to", &spec.to),
                ],
            )
        );
    }
    Ok(())
}

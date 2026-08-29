//! The canonical security event — and the log it rides on.
//!
//! # No new bus
//!
//! Guardrail #1 of this repo is *daemonless by design*, so a security event
//! does not get a pipeline of its own. It maps onto the append-only log the
//! engine already has (`delonix_runtime_core::events`, `<root>/events.jsonl`),
//! under `kind = "security"`. The file IS the bus; a reader tails it.
//!
//! # No tenant
//!
//! There is no `tenant`, `project` or `environment` field here, and there will
//! not be one. ADR-0010 (Rejected, 2026-08-10) and ADR-0025 (Accepted,
//! 2026-08-29) put tenancy in `delonix-paas`; a layer that has tenants wraps
//! this event and adds them on its side of the boundary.
//!
//! # What this log is NOT
//!
//! `events::emit` is **best-effort and infallible by design** — an error while
//! recording can never fail the operation that produced it. That is right for
//! lifecycle events and it is a real limitation for security ones: this log
//! detects nothing about its own gaps, and an attacker with write access to the
//! state root can edit it. The tamper-evident trail is the hash-chained audit
//! log with the Ed25519 anchor, which lives in `delonix-paas`. This is
//! operational signal, not evidence — and calling it evidence would be the
//! kind of overclaim the engine refuses elsewhere.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::admission::{Decision, Request, Rule, Violation};
use crate::redact::redact_text;
use crate::severity::{Confidence, Severity};

/// Keeps one event comfortably under `PIPE_BUF` (4 KiB), which is what makes
/// the append-only log lock-free. See the `events` module doc.
const MAX_DETAIL: usize = 512;

/// The `kind` every security event carries in the engine log, so
/// `delonix system events` can filter on it.
pub const EVENT_KIND: &str = "security";

/// What area of the system a finding came from.
///
/// `non_exhaustive`: the layers above add categories (malware, ransomware,
/// integrity) as their producers land. Only the ones this repo can actually
/// emit today are listed — a variant with no producer is a promise, not a type.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Category {
    /// A request the node refused, or would have refused.
    Admission,
    /// Image provenance: signature, SBOM, advisories.
    SupplyChain,
    /// The policy file itself — unreadable, or contradictory.
    Policy,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Admission => "admission",
            Category::SupplyChain => "supply_chain",
            Category::Policy => "policy",
        }
    }
}

/// What the node did about it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Refused. The workload does not exist.
    Denied,
    /// Allowed under `mode: warn`. The workload EXISTS and the rule did not
    /// stop it — a distinction an operator must never have to infer.
    Warned,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Denied => "denied",
            Outcome::Warned => "warned",
        }
    }
}

/// One security finding, tenancy-free.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SecurityEvent {
    /// Unix instant (seconds).
    pub ts: u64,
    pub category: Category,
    pub severity: Severity,
    /// Deterministic rules are certain; detectors that guess are not. Kept
    /// separate from `severity` on purpose — see [`crate::severity`].
    pub confidence: Confidence,
    pub outcome: Outcome,
    /// Stable rule identifier (`ADM-DEVICE-PASSTHROUGH`), greppable and
    /// alertable. Never a translated sentence.
    pub rule: String,
    /// `container` | `vm`.
    pub workload: String,
    /// What was being created. A name, not an id: at admission time the request
    /// was refused, so no id was ever allocated.
    pub resource: String,
    /// Short, REDACTED, bounded context. Never a raw manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl SecurityEvent {
    /// The events a decision produced. `Decision::Allow` produces none — an
    /// engine that logged every permitted request would drown the signal it is
    /// there to carry.
    pub fn from_decision(d: &Decision, r: &Request<'_>, resource: &str) -> Vec<SecurityEvent> {
        let outcome = match d {
            Decision::Allow => return Vec::new(),
            Decision::AllowWithWarnings(_) => Outcome::Warned,
            Decision::Deny(_) => Outcome::Denied,
        };
        let ts = now_unix();
        d.violations()
            .iter()
            .map(|v| SecurityEvent {
                ts,
                category: Category::Admission,
                severity: v.severity(),
                confidence: Confidence::CERTAIN,
                outcome,
                rule: v.rule().id().to_string(),
                workload: r.workload.as_str().to_string(),
                resource: resource.to_string(),
                detail: detail_of(v),
            })
            .collect()
    }

    /// Appends to the engine's log. Best-effort, like every other event — see
    /// the module doc for why that is a stated limitation and not a design
    /// choice made here.
    pub fn emit(&self, root: &Path) {
        delonix_runtime_core::events::emit(
            root,
            EVENT_KIND,
            &self.rule,
            "",
            &self.resource,
            self.detail.as_deref(),
        );
    }

    /// One line for a human, severity first because that is what is scanned.
    pub fn to_line(&self) -> String {
        let detail = self
            .detail
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        format!(
            "{:<8} {:<7} {:<22} {} {}{}",
            self.severity.as_str(),
            self.outcome.as_str(),
            self.rule,
            self.workload,
            self.resource,
            detail
        )
    }
}

/// The short context a violation carries into the log: redacted, then bounded.
///
/// Redaction runs FIRST. Truncating a secret does not make it not a secret, and
/// a 512-byte prefix of a private key is still key material.
fn detail_of(v: &Violation) -> Option<String> {
    let raw = match v {
        Violation::Privileged | Violation::HostNetwork => return None,
        Violation::LatestTag { image } | Violation::LatestVmImage { image } => image.clone(),
        Violation::Registry { host, .. } => host.clone(),
        Violation::DevicePassthrough { devices } => devices.join(","),
        Violation::ImageUrlHost { host, .. } => host.clone(),
    };
    if raw.is_empty() {
        return None;
    }
    let safe = redact_text(&raw);
    let (cut, truncated) = truncate_on_boundary(&safe, MAX_DETAIL);
    Some(if truncated {
        format!("{cut}…")
    } else {
        cut.to_string()
    })
}

fn truncate_on_boundary(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Every rule this crate can emit an event for. Exists so a test can prove the
/// set is complete, and so an operator can build an alert list without reading
/// the source.
pub const EMITTED_RULES: &[Rule] = &[
    Rule::Privileged,
    Rule::HostNetwork,
    Rule::LatestTag,
    Rule::Registry,
    Rule::DevicePassthrough,
    Rule::LatestVmImage,
    Rule::ImageUrlHost,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Mode, SecurityPolicy};

    fn strict() -> SecurityPolicy {
        SecurityPolicy {
            deny_privileged: true,
            deny_device_passthrough: true,
            ..Default::default()
        }
    }

    #[test]
    fn an_allowed_request_produces_no_event_at_all() {
        let d =
            crate::admission::evaluate(&strict(), &Request::container("alpine:3.20", false, false));
        assert!(SecurityEvent::from_decision(
            &d,
            &Request::container("alpine:3.20", false, false),
            "web"
        )
        .is_empty());
    }

    #[test]
    fn a_refusal_produces_one_event_per_reason_with_a_stable_rule() {
        let devices = vec!["0000:01:00.0".to_string()];
        let r = Request::virtual_machine(None, &devices, None);
        let d = crate::admission::evaluate(&strict(), &r);
        let evs = SecurityEvent::from_decision(&d, &r, "db-01");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].rule, "ADM-DEVICE-PASSTHROUGH");
        assert_eq!(evs[0].outcome, Outcome::Denied);
        assert_eq!(evs[0].severity, Severity::Critical);
        assert_eq!(evs[0].workload, "vm");
        assert_eq!(evs[0].resource, "db-01");
    }

    #[test]
    fn warned_and_denied_are_different_states_in_the_event() {
        // An operator must never have to infer whether the workload exists.
        let p = SecurityPolicy {
            mode: Mode::Warn,
            ..strict()
        };
        let r = Request::container("alpine:3.20", true, false);
        let d = crate::admission::evaluate(&p, &r);
        let evs = SecurityEvent::from_decision(&d, &r, "web");
        assert_eq!(evs[0].outcome, Outcome::Warned);
    }

    #[test]
    fn the_detail_is_redacted_before_it_is_truncated() {
        // Truncating a secret does not make it not a secret: redaction first.
        let v = Violation::Registry {
            image: "x".into(),
            host: "token=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxIn0.abcdefghijklmnop"
                .into(),
            allowed: vec![],
        };
        let d = detail_of(&v).unwrap();
        assert!(!d.contains("eyJ"), "{d}");
        assert!(d.len() <= MAX_DETAIL + 4);
    }

    #[test]
    fn the_detail_always_fits_below_pipe_buf() {
        // The log is lock-free because every line fits inside PIPE_BUF (4 KiB).
        let v = Violation::DevicePassthrough {
            devices: (0..5000).map(|i| format!("0000:{i:02}:00.0")).collect(),
        };
        let d = detail_of(&v).unwrap();
        assert!(d.len() <= MAX_DETAIL + 4, "{}", d.len());
    }

    #[test]
    fn the_event_round_trips_through_json() {
        let devices = vec!["0000:01:00.0".to_string()];
        let r = Request::virtual_machine(None, &devices, None);
        let d = crate::admission::evaluate(&strict(), &r);
        let ev = &SecurityEvent::from_decision(&d, &r, "db-01")[0];
        let json = serde_json::to_string(ev).unwrap();
        assert_eq!(&serde_json::from_str::<SecurityEvent>(&json).unwrap(), ev);
        // And it carries no tenant whatsoever — guardrail #2.
        for forbidden in ["tenant", "project", "environment"] {
            assert!(!json.contains(forbidden), "{json}");
        }
    }
}

//! The three axes a security finding is routinely collapsed into one — and why
//! they must stay apart.
//!
//! - [`Severity`] is **operational**: how much this matters if it is real.
//! - [`ActionRisk`] is **structural**: how much damage the operation itself can
//!   do, known before anything is evaluated.
//! - [`Confidence`] is **epistemic**: how sure the detector is.
//!
//! Collapsing confidence into severity is the mistake that produces both alert
//! fatigue and missed incidents: a 0.30-confidence `Critical` and a
//! 0.99-confidence `Critical` are not the same page, and a single number cannot
//! tell an operator which one they are holding.

use serde::{Deserialize, Serialize};

/// How much a finding matters **if it is real**.
///
/// `Ord` is derived and the variants are declared least-to-most severe, so
/// `>=` comparisons against a threshold read the way they are meant to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// The stable wire name. Identical to the serde representation on purpose:
    /// a log line and a JSON field must never disagree about a severity.
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    /// Parses a threshold as an operator would type it. Case-insensitive
    /// because `DELONIX_SCAN_ON_PULL=HIGH` is not a different intent from
    /// `high`, and refusing it would be pedantry, not a gate.
    ///
    /// Returns `None` for anything else — the caller decides, and the engine's
    /// convention (`scan.rs`) is that an unknown value is **refused**, never
    /// silently treated as "no policy".
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "info" => Some(Severity::Info),
            "low" => Some(Severity::Low),
            "medium" => Some(Severity::Medium),
            "high" => Some(Severity::High),
            "critical" => Some(Severity::Critical),
            _ => None,
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much damage an operation can do, **before** anything about the caller or
/// the target is known.
///
/// The variant names are taken verbatim from ADR-0025's per-tool table so that
/// promoting that table here later is a move, not a translation. Ordered
/// least-to-most dangerous.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "camelCase")]
pub enum ActionRisk {
    /// Observes state. Cannot change anything.
    Read,
    /// Creates or updates without destroying: the worst outcome is more state.
    SafeWrite,
    /// Interrupts a running workload; the state survives (`stop`, `restart`).
    Disruptive,
    /// Removes state that is not recoverable from within this node (`rm`,
    /// volume delete).
    Destructive,
    /// Widens the blast radius beyond the workload itself — `--privileged`,
    /// device passthrough, host namespaces.
    Privileged,
}

impl ActionRisk {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionRisk::Read => "read",
            ActionRisk::SafeWrite => "safeWrite",
            ActionRisk::Disruptive => "disruptive",
            ActionRisk::Destructive => "destructive",
            ActionRisk::Privileged => "privileged",
        }
    }
}

impl std::fmt::Display for ActionRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How sure a detector is, in `0.0..=1.0`.
///
/// A newtype rather than a bare `f32` for one reason: it is constructed through
/// [`Confidence::new`], which clamps and rejects `NaN`. A detector fed
/// malformed external input must not be able to put a `NaN` into a comparison
/// that then silently answers `false` to every threshold — that is a detector
/// that has been switched off without anybody noticing.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, PartialOrd)]
#[serde(transparent)]
pub struct Confidence(f32);

impl Confidence {
    /// Certain — a deterministic rule that read the request and found the field
    /// it refuses. Admission never guesses.
    pub const CERTAIN: Confidence = Confidence(1.0);

    /// Clamps to `0.0..=1.0`. `NaN` becomes `0.0`: an unmeasurable confidence is
    /// no confidence, never a silently-passing one.
    pub fn new(v: f32) -> Self {
        if v.is_nan() {
            return Confidence(0.0);
        }
        Confidence(v.clamp(0.0, 1.0))
    }

    pub fn value(&self) -> f32 {
        self.0
    }
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.2}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_orders_least_to_most_severe() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Info < Severity::Low);
    }

    #[test]
    fn severity_round_trips_through_serde_under_the_same_name() {
        for s in [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            assert_eq!(serde_json::from_str::<Severity>(&json).unwrap(), s);
        }
    }

    #[test]
    fn severity_parse_accepts_uppercase_and_refuses_the_rest() {
        assert_eq!(Severity::parse("HIGH"), Some(Severity::High));
        assert_eq!(Severity::parse("  critical "), Some(Severity::Critical));
        assert_eq!(Severity::parse("catastrophic"), None);
        assert_eq!(Severity::parse(""), None);
    }

    #[test]
    fn action_risk_orders_privileged_above_destructive() {
        assert!(ActionRisk::Privileged > ActionRisk::Destructive);
        assert!(ActionRisk::Read < ActionRisk::SafeWrite);
    }

    #[test]
    fn confidence_turns_nan_into_zero_not_one() {
        // The regression this test locks shut: `NaN >= threshold` is `false`, so
        // a NaN that got through would switch the detector off in silence.
        assert_eq!(Confidence::new(f32::NAN).value(), 0.0);
        assert_eq!(Confidence::new(-3.0).value(), 0.0);
        assert_eq!(Confidence::new(9.0).value(), 1.0);
        assert_eq!(Confidence::CERTAIN.value(), 1.0);
    }
}

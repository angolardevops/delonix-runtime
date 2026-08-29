//! Risk classification for MCP tools (ADR-0025 §5) — scoped to this crate only,
//! never wired into `delonix-mgmt`/`delonix-runtime-core` in this pass.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RiskLevel {
    Read,
    SafeWrite,
    Disruptive,
    Destructive,
    Privileged,
}

impl RiskLevel {
    /// Anything above `SafeWrite` must not execute without the caller explicitly
    /// setting `confirm: true` on the call.
    pub fn requires_confirm(self) -> bool {
        !matches!(self, RiskLevel::Read | RiskLevel::SafeWrite)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RiskLevel::Read => "READ",
            RiskLevel::SafeWrite => "SAFE_WRITE",
            RiskLevel::Disruptive => "DISRUPTIVE",
            RiskLevel::Destructive => "DESTRUCTIVE",
            RiskLevel::Privileged => "PRIVILEGED",
        }
    }
}

/// One row per MCP tool this server exposes — what `delonix mcp capabilities` prints,
/// and the table every mutation is gated against. A tool not listed here is treated
/// as `Privileged` (fail closed), never as `Read`.
pub const TOOL_RISK: &[(&str, RiskLevel)] = &[
    ("runtime.info", RiskLevel::Read),
    ("runtime.health", RiskLevel::Read),
    ("resource.list", RiskLevel::Read),
    ("resource.get", RiskLevel::Read),
    ("resource.describe", RiskLevel::Read),
    ("metrics.query", RiskLevel::Read),
    ("logs.query", RiskLevel::Read),
    ("network.inspect", RiskLevel::Read),
    ("storage.inspect", RiskLevel::Read),
    ("container.restart", RiskLevel::Disruptive),
    ("task.get", RiskLevel::Read),
    ("task.cancel", RiskLevel::SafeWrite),
    ("audit.query", RiskLevel::Read),
];

pub fn risk_of(tool: &str) -> RiskLevel {
    TOOL_RISK
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, r)| *r)
        .unwrap_or(RiskLevel::Privileged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tools_have_the_expected_risk() {
        assert_eq!(risk_of("runtime.info"), RiskLevel::Read);
        assert_eq!(risk_of("task.cancel"), RiskLevel::SafeWrite);
        assert_eq!(risk_of("container.restart"), RiskLevel::Disruptive);
    }

    #[test]
    fn an_unknown_tool_fails_closed_to_privileged() {
        assert_eq!(risk_of("resource.apply"), RiskLevel::Privileged);
        assert!(risk_of("resource.apply").requires_confirm());
    }

    #[test]
    fn only_read_and_safe_write_skip_confirmation() {
        assert!(!RiskLevel::Read.requires_confirm());
        assert!(!RiskLevel::SafeWrite.requires_confirm());
        assert!(RiskLevel::Disruptive.requires_confirm());
        assert!(RiskLevel::Destructive.requires_confirm());
        assert!(RiskLevel::Privileged.requires_confirm());
    }
}

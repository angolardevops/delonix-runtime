//! The condition a resource reports: one prerequisite, met or not.

/// A condition of a resource — `ok=false` is what matters (the missing
/// prerequisite). `reason` is a short stable code; `message` is actionable.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Condition {
    pub kind: &'static str,
    pub ok: bool,
    pub reason: &'static str,
    pub message: String,
}

impl Condition {
    pub fn ok(kind: &'static str) -> Self {
        Condition {
            kind,
            ok: true,
            reason: "",
            message: String::new(),
        }
    }
    pub fn bad(kind: &'static str, reason: &'static str, message: impl Into<String>) -> Self {
        Condition {
            kind,
            ok: false,
            reason,
            message: message.into(),
        }
    }
}

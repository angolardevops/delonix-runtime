//! `delonix-security-runtime` — the node's security decisions, in one place.
//!
//! # What this crate is
//!
//! The **decision** half of the runtime security fabric: what the node refuses
//! to run, why, how bad it is, and what the resulting event looks like. It is
//! pure — no filesystem beyond appending an event, no network, no privileges,
//! no background work. Everything here is a function of its arguments, which is
//! what makes a security control testable without the infrastructure it guards.
//!
//! ```
//! use delonix_security_runtime::{admission, policy::SecurityPolicy};
//!
//! let p = SecurityPolicy::parse(r#"{"denyDevicePassthrough": true}"#).unwrap();
//! let devices = vec!["0000:01:00.0".to_string()];
//! let req = admission::Request::virtual_machine(None, &devices, None);
//! assert!(admission::evaluate(&p, &req).is_denied());
//! ```
//!
//! # What this crate is NOT, and why
//!
//! It does not scan, sniff, watch or reside. The canonical brief this was built
//! from asked for eBPF sensors, file-integrity watchers, behavioural ransomware
//! scoring, malware engines and continuous network detection. Each of those is
//! a **resident process**, and guardrail #1 of this repo is *daemonless by
//! design*: a permanent background service is a change of product philosophy
//! that needs its own ADR with a proven need, not a module added quietly.
//!
//! There is also a measured reason, not only a doctrinal one. The engine's
//! primary mode is rootless. `delonix-net`'s existing eBPF loader documents
//! that loading a program needs `CAP_BPF` + `CAP_NET_ADMIN` in the init
//! namespace, which a rootless runtime does not have — so it no-ops. A sensor
//! layer built on the same footing would be inert exactly where this engine
//! usually runs, and shipping an inert security control is worse than shipping
//! none: it answers «protected» to a question nobody actually measured.
//!
//! # And it has no tenants
//!
//! No `tenant`, `project` or `environment` field appears anywhere in this
//! crate. ADR-0010 (Rejected, 2026-08-10) and ADR-0025 (Accepted, 2026-08-29)
//! both place tenancy, identity and approval in `delonix-paas`. The layer that
//! has tenants consumes this one and adds them on its own side of the line —
//! the same way `RemoteRuntime` already layers on `delonix-mgmt` without
//! linking its crates. See ADR-0026.
//!
//! # Modules
//!
//! - [`policy`] — what the node refuses, and the lints for what the operator
//!   left open.
//! - [`admission`] — the single evaluation point, for containers and VMs.
//! - [`event`] — the tenancy-free finding, on the log the engine already has.
//! - [`score`] — posture as a number that always arrives with its reasons.
//! - [`redact`] — the one module whose input is genuinely hostile.
//! - [`severity`] — the three axes that must not collapse into one.

pub mod admission;
pub mod event;
pub mod policy;
pub mod redact;
pub mod score;
pub mod severity;

pub use admission::{evaluate, Decision, Request, Rule, Violation, Workload};
pub use event::SecurityEvent;
pub use policy::{Mode, SecurityPolicy};
pub use score::Score;
pub use severity::{ActionRisk, Confidence, Severity};

#[cfg(test)]
mod boundary_tests {
    /// Guardrail #2 as a test rather than an intention: no notion of a tenant
    /// enters this repo. A `tenant` field added by accident fails here, not in
    /// a review that may or may not happen.
    #[test]
    fn no_file_in_this_crate_speaks_of_a_tenant() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("src/") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read");
            for (n, line) in text.lines().enumerate() {
                let l = line.to_ascii_lowercase();
                // Only field/variant declarations count: the word legitimately
                // appears in the docs that EXPLAIN why it is not here.
                let decl = l.trim_start();
                let is_decl = decl.starts_with("pub tenant")
                    || decl.starts_with("tenant:")
                    || decl.starts_with("pub project:")
                    || decl.starts_with("pub environment:");
                if is_decl {
                    offenders.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
        assert!(offenders.is_empty(), "tenant fields: {offenders:?}");
    }
}

//! Node-level runtime policy — a ceiling this host puts on what may run.
//!
//! # Why in the runtime and not only in an admission controller
//!
//! The same reasoning that put the capability ceiling in the CRI
//! (`DELONIX_CRI_CAP_CEILING`): everything reaching `cmd_run` has already been
//! authorised by whoever called it. A policy that lives only in a cluster's
//! admission chain runs in another process, on another machine, and this node
//! cannot see or verify its configuration.
//!
//! This is the local answer. It holds with Pod Security misconfigured, with a
//! `crictl` talking straight to the socket, and with somebody typing
//! `delonix container run --privileged` by hand.
//!
//! # Fail-closed, and loudly
//!
//! A policy that cannot be READ is not an absent policy — a truncated or
//! malformed file means somebody's intent is unknown, and running the workload
//! anyway is the silent degradation this engine refuses everywhere else.
//!
//! Absent, on the other hand, means absent: no file, no ceiling, byte-for-byte
//! the behaviour this engine has always had. Turning a missing file into a
//! default-deny would break every existing host on upgrade.
//!
//! # What moved, and what stayed
//!
//! The decision itself now lives in `delonix-security-runtime`, so the VM path
//! can reach the same gate the container path already did (ADR-0026). This file
//! is what is left over and cannot move: reading the file, rendering a refusal
//! in the operator's language, and recording the event.

use delonix_runtime_core::{Error, Result};
use delonix_security_runtime as srt;
use srt::admission::{Decision, Violation, Workload};
use srt::policy::SecurityPolicy;
use std::path::{Path, PathBuf};

pub(crate) use srt::admission::Request;

/// `<root>/policy.json`.
pub(crate) fn path(root: &Path) -> PathBuf {
    root.join("policy.json")
}

/// Reads the node policy. `Ok(None)` when there is none.
///
/// A file that exists and cannot be parsed is an ERROR, not a missing policy —
/// see the module doc.
pub(crate) fn load(root: &Path) -> Result<Option<SecurityPolicy>> {
    let p = path(root);
    let raw = match std::fs::read_to_string(&p) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::Invalid(super::po::tf(
                "runtime policy {path}: {err} — a policy that cannot be read is not an \
                 absent policy",
                &[("path", &p.display().to_string()), ("err", &e.to_string())],
            )))
        }
    };
    SecurityPolicy::parse(&raw).map(Some).map_err(|e| {
        Error::Invalid(super::po::tf(
            "runtime policy {path}: {err}",
            &[("path", &p.display().to_string()), ("err", &e.to_string())],
        ))
    })
}

/// One refusal, in the operator's language.
///
/// The first four `msgid`s are byte-identical to the ones that shipped in
/// v0.69.0 — changing the English would orphan the Portuguese catalog entry and
/// silently drop the translation.
fn render(v: &Violation) -> String {
    match v {
        Violation::Privileged => {
            super::po::t("--privileged is refused by this node's runtime policy").to_string()
        }
        Violation::HostNetwork => {
            super::po::t("--net host is refused by this node's runtime policy").to_string()
        }
        Violation::LatestTag { image } => super::po::tf(
            "image '{image}': this node's runtime policy refuses `:latest` and untagged \
             references — pin a version or a digest",
            &[("image", image)],
        ),
        Violation::Registry {
            image,
            host,
            allowed,
        } => super::po::tf(
            "image '{image}': registry '{host}' is not in this node's allowed list ({allowed})",
            &[
                ("image", image),
                ("host", host),
                ("allowed", &allowed.join(", ")),
            ],
        ),
        Violation::DevicePassthrough { devices } => super::po::tf(
            "device passthrough ({devices}) is refused by this node's runtime policy — a \
             passed-through device gives the guest DMA to host hardware",
            &[("devices", &devices.join(", "))],
        ),
        Violation::LatestVmImage { image } => super::po::tf(
            "VM image '{image}': this node's runtime policy refuses `:latest` and untagged \
             references — pin a version",
            &[("image", image)],
        ),
        Violation::ImageUrlHost { url, host, allowed } => super::po::tf(
            "boot image '{url}': host '{host}' is not in this node's allowed list ({allowed})",
            &[
                ("url", url),
                ("host", host),
                ("allowed", &allowed.join(", ")),
            ],
        ),
    }
}

/// Shows, at most once per process, the gaps the operator left open on the path
/// they are using.
///
/// Once per process is exactly once per command — this engine is daemonless, so
/// a CLI invocation is a process that is born, works and dies. Silenceable with
/// `DELONIX_POLICY_LINT=0` for anyone who has read it and decided otherwise.
fn show_lints(p: &SecurityPolicy, w: Workload) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SHOWN: AtomicBool = AtomicBool::new(false);

    if std::env::var("DELONIX_POLICY_LINT").as_deref() == Ok("0") {
        return;
    }
    if SHOWN.swap(true, Ordering::Relaxed) {
        return;
    }
    for l in p.lint().iter().filter(|l| l.applies_to(w)) {
        eprintln!(
            "{} [{}] {}",
            super::po::t("warning: runtime policy"),
            l.id,
            super::po::t(l.message)
        );
    }
}

/// Refuses the request when the node policy says so. No policy = no ceiling.
///
/// Under `mode: warn` the request proceeds and the reasons are printed — the
/// distinction between «refused» and «allowed, and here is what would have
/// refused it» is one an operator must never have to infer.
pub(crate) fn enforce(root: &Path, resource: &str, r: &Request<'_>) -> Result<()> {
    let Some(p) = load(root)? else {
        return Ok(());
    };
    show_lints(&p, r.workload);

    let decision = srt::admission::evaluate(&p, r);

    // The event carries the refusal into the engine's log whether or not the
    // request was stopped, so a `mode: warn` rollout leaves a trail to count.
    for ev in srt::SecurityEvent::from_decision(&decision, r, resource) {
        ev.emit(root);
    }

    match decision {
        Decision::Allow => Ok(()),
        Decision::AllowWithWarnings(v) => {
            for line in v.iter().map(render) {
                eprintln!(
                    "{} {line}",
                    super::po::t("warning: runtime policy would refuse:")
                );
            }
            Ok(())
        }
        Decision::Deny(v) => Err(Error::Invalid(format!(
            "{}\n  {}",
            super::po::tf(
                "refused by this node's runtime policy ({path}):",
                &[("path", &path(root).display().to_string())],
            ),
            v.iter().map(render).collect::<Vec<_>>().join("\n  ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dlx-pol-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A file that exists and does not parse is an ERROR. Treating it as «no
    /// policy» would let a typo silently disable the node's ceiling.
    #[test]
    fn an_unparseable_policy_is_an_error_not_an_absent_one() {
        let d = tmp("bad");
        std::fs::write(path(&d), "{ not json").unwrap();
        assert!(load(&d).is_err());
        // An unknown field is refused too — `denyPriviledged` spelled wrong
        // would otherwise read as "allowed".
        std::fs::write(path(&d), r#"{"denyPriviledged": true}"#).unwrap();
        assert!(load(&d).is_err(), "a typo must not read as permissive");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn no_file_means_no_ceiling() {
        let d = tmp("none");
        assert_eq!(load(&d).unwrap(), None);
        assert!(enforce(&d, "web", &Request::container("alpine:latest", true, true)).is_ok());
        std::fs::remove_dir_all(&d).ok();
    }

    /// The gap this crate was written to close, end to end through the file.
    #[test]
    fn a_vm_with_passthrough_is_refused_by_the_file_on_disk() {
        let d = tmp("vm");
        std::fs::write(path(&d), r#"{"denyDevicePassthrough": true}"#).unwrap();
        let devices = vec!["0000:01:00.0".to_string()];
        let e = enforce(&d, "db-01", &Request::virtual_machine(None, &devices, None)).unwrap_err();
        assert!(format!("{e}").contains("0000:01:00.0"), "{e}");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Every refusal is rendered — a violation with no message would reach the
    /// operator as a blank line.
    #[test]
    fn every_violation_renders_to_something() {
        let all = [
            Violation::Privileged,
            Violation::HostNetwork,
            Violation::LatestTag { image: "a".into() },
            Violation::Registry {
                image: "a".into(),
                host: "h".into(),
                allowed: vec!["g".into()],
            },
            Violation::DevicePassthrough {
                devices: vec!["0000:01:00.0".into()],
            },
            Violation::LatestVmImage { image: "a".into() },
            Violation::ImageUrlHost {
                url: "https://x/y".into(),
                host: "x".into(),
                allowed: vec!["z".into()],
            },
        ];
        for v in &all {
            assert!(!render(v).trim().is_empty(), "{v:?} rendered empty");
        }
    }
}

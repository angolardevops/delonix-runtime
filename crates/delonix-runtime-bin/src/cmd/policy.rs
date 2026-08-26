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

use delonix_runtime_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `<root>/policy.json`.
pub(crate) fn path(root: &Path) -> PathBuf {
    root.join("policy.json")
}

/// What this node refuses to run.
///
/// Every field defaults to «no opinion», so a policy file states only what it
/// wants to restrict. A field nobody sets must never start refusing things.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RuntimePolicy {
    /// Refuse `--privileged`.
    #[serde(default)]
    pub deny_privileged: bool,
    /// Refuse `--net host`, which is this engine's DEFAULT network mode — a host
    /// that sets this is asking every workload to name a network explicitly.
    #[serde(default)]
    pub deny_host_network: bool,
    /// Refuse an image reference with no tag or with `:latest`.
    ///
    /// `latest` is not a version: the same manifest gives a different container
    /// tomorrow, and an incident becomes unreproducible.
    #[serde(default)]
    pub deny_latest_tag: bool,
    /// Only these registries may be pulled from. Empty = no opinion.
    #[serde(default)]
    pub allowed_registries: Vec<String>,
}

impl RuntimePolicy {
    /// Reads the node policy. `Ok(None)` when there is none.
    ///
    /// A file that exists and cannot be parsed is an ERROR, not a missing
    /// policy — see the module doc.
    pub(crate) fn load(root: &Path) -> Result<Option<Self>> {
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
        serde_json::from_str(&raw).map(Some).map_err(|e| {
            Error::Invalid(super::po::tf(
                "runtime policy {path}: {err}",
                &[("path", &p.display().to_string()), ("err", &e.to_string())],
            ))
        })
    }

    /// Every reason this policy refuses the request — all of them, not the first.
    ///
    /// Reporting one at a time turns fixing a manifest into one round trip per
    /// violation, which is the drip-feed that makes people stop reading output.
    /// Same reason `stack plan` names every cold field at once.
    pub(crate) fn violations(&self, r: &Request<'_>) -> Vec<String> {
        let mut out = Vec::new();
        if self.deny_privileged && r.privileged {
            out.push(
                super::po::t("--privileged is refused by this node's runtime policy").to_string(),
            );
        }
        if self.deny_host_network && r.host_network {
            out.push(
                super::po::t("--net host is refused by this node's runtime policy").to_string(),
            );
        }
        if self.deny_latest_tag && is_latest(r.image) {
            out.push(super::po::tf(
                "image '{image}': this node's runtime policy refuses `:latest` and untagged \
                 references — pin a version or a digest",
                &[("image", r.image)],
            ));
        }
        if !self.allowed_registries.is_empty() {
            let host = registry_of(r.image);
            if !self.allowed_registries.iter().any(|a| a == &host) {
                out.push(super::po::tf(
                    "image '{image}': registry '{host}' is not in this node's allowed list \
                     ({allowed})",
                    &[
                        ("image", r.image),
                        ("host", &host),
                        ("allowed", &self.allowed_registries.join(", ")),
                    ],
                ));
            }
        }
        out
    }
}

/// What is being asked of the node.
pub(crate) struct Request<'a> {
    pub image: &'a str,
    pub privileged: bool,
    pub host_network: bool,
}

/// The registry host of a reference, using Docker's own rule.
///
/// The first segment is a registry only when it looks like a host — it contains
/// a `.` or a `:`, or it is `localhost`. Without that rule `library/alpine`
/// would have `library` as its registry, and `alpine` would have none.
pub(crate) fn registry_of(image: &str) -> String {
    let first = image.split('/').next().unwrap_or("");
    if image.contains('/') && (first.contains('.') || first.contains(':') || first == "localhost") {
        first.to_string()
    } else {
        "docker.io".to_string()
    }
}

/// Whether a reference is `:latest` or carries no tag at all.
///
/// Untagged counts: `alpine` and `alpine:latest` resolve to the same thing, and
/// a policy refusing only the explicit form would be bypassed by dropping four
/// characters.
pub(crate) fn is_latest(image: &str) -> bool {
    if image.contains('@') {
        return false; // pinned by digest — the strongest form there is
    }
    // Strip the registry before looking for a tag: the `:` in `localhost:5000/x`
    // is a PORT, and reading it as a tag separator would call that image
    // untagged.
    let after_registry = match image.rfind('/') {
        Some(i) => &image[i + 1..],
        None => image,
    };
    match after_registry.rsplit_once(':') {
        Some((_, tag)) => tag == "latest",
        None => true,
    }
}

/// Refuses the request when the node policy says so. No policy = no ceiling.
pub(crate) fn enforce(root: &Path, r: &Request<'_>) -> Result<()> {
    let Some(p) = RuntimePolicy::load(root)? else {
        return Ok(());
    };
    let v = p.violations(r);
    if v.is_empty() {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "{}\n  {}",
        super::po::tf(
            "refused by this node's runtime policy ({path}):",
            &[("path", &path(root).display().to_string())],
        ),
        v.join("\n  ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(image: &str) -> Request<'_> {
        Request {
            image,
            privileged: false,
            host_network: false,
        }
    }

    /// A policy with nothing set refuses nothing. This is the upgrade path: a
    /// host that never wrote a policy must behave exactly as before.
    #[test]
    fn an_empty_policy_refuses_nothing() {
        let p = RuntimePolicy::default();
        assert!(p.violations(&req("alpine:latest")).is_empty());
        assert!(p
            .violations(&Request {
                image: "alpine",
                privileged: true,
                host_network: true,
            })
            .is_empty());
    }

    /// **The substring trap.** A policy allowing `ghcr.io` must not accept
    /// `evil-ghcr.io`. This repo has already paid for a substring match on an IP
    /// (`10.0.0.5` matching `10.0.0.50`) in the overlay peer code.
    #[test]
    fn an_allowed_registry_is_matched_whole_never_as_a_substring() {
        let p = RuntimePolicy {
            allowed_registries: vec!["ghcr.io".into()],
            ..Default::default()
        };
        assert!(p.violations(&req("ghcr.io/org/app:1.0")).is_empty());
        assert_eq!(p.violations(&req("evil-ghcr.io/org/app:1.0")).len(), 1);
        assert_eq!(p.violations(&req("ghcr.io.evil.com/app:1.0")).len(), 1);
    }

    /// Docker's own rule: the first segment is a registry only when it looks
    /// like a host. Otherwise `library/alpine` would have registry `library`.
    #[test]
    fn the_registry_is_read_the_way_docker_reads_it() {
        assert_eq!(registry_of("alpine"), "docker.io");
        assert_eq!(registry_of("library/alpine"), "docker.io");
        assert_eq!(registry_of("ghcr.io/org/app"), "ghcr.io");
        assert_eq!(registry_of("localhost:5000/app"), "localhost:5000");
        assert_eq!(
            registry_of("registry.example.com/a/b/c"),
            "registry.example.com"
        );
    }

    /// Untagged has to count as `latest`, or the rule is bypassed by dropping
    /// four characters. A digest pin is the opposite and must pass.
    #[test]
    fn untagged_counts_as_latest_and_a_digest_does_not() {
        assert!(is_latest("alpine"));
        assert!(is_latest("alpine:latest"));
        assert!(is_latest("ghcr.io/org/app"));
        assert!(!is_latest("alpine:3.20"));
        assert!(!is_latest("alpine@sha256:abc"));
        // A port in the registry is not a tag separator.
        assert!(is_latest("localhost:5000/app"));
        assert!(!is_latest("localhost:5000/app:1.2"));
    }

    /// Every reason at once, never one per round trip.
    #[test]
    fn all_violations_are_reported_together() {
        let p = RuntimePolicy {
            deny_privileged: true,
            deny_host_network: true,
            deny_latest_tag: true,
            allowed_registries: vec!["ghcr.io".into()],
        };
        let v = p.violations(&Request {
            image: "docker.io/alpine",
            privileged: true,
            host_network: true,
        });
        assert_eq!(v.len(), 4, "expected all four reasons, got {v:?}");
    }

    /// A file that exists and does not parse is an ERROR. Treating it as «no
    /// policy» would let a typo silently disable the node's ceiling.
    #[test]
    fn an_unparseable_policy_is_an_error_not_an_absent_one() {
        let d = std::env::temp_dir().join(format!("dlx-pol-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(path(&d), "{ not json").unwrap();
        assert!(RuntimePolicy::load(&d).is_err());
        // An unknown field is refused too — `denyPriviledged` spelled wrong
        // would otherwise read as "allowed".
        std::fs::write(path(&d), r#"{"denyPriviledged": true}"#).unwrap();
        assert!(
            RuntimePolicy::load(&d).is_err(),
            "a typo must not read as permissive"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn no_file_means_no_ceiling() {
        let d = std::env::temp_dir().join(format!("dlx-pol-none-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(RuntimePolicy::load(&d).unwrap(), None);
        assert!(enforce(&d, &req("alpine:latest")).is_ok());
        std::fs::remove_dir_all(&d).ok();
    }
}

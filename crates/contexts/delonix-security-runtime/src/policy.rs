//! The node's security policy — the ceiling this host puts on what may run,
//! whatever asked for it.
//!
//! # Why the VM fields are separate, and default-off
//!
//! This module widens `cmd/policy.rs`'s four-field `RuntimePolicy` to cover the
//! VM path, which had **no node policy at all** (measured on `origin/main`:
//! `policy::enforce` has exactly one caller, `cmd/container.rs`). The obvious
//! shortcut — let `denyPrivileged` also refuse `vm create --device` — was
//! rejected for the reason the original module states in its own doc: turning a
//! setting into something that refuses MORE breaks existing hosts on upgrade.
//! An operator who wrote `denyPrivileged: true` last month consented to a rule
//! about containers; silently extending it to their VM fleet is a surprise
//! shipped as a fix.
//!
//! So the VM rules are new fields, every one of them `false`/empty by default,
//! and [`SecurityPolicy::lint`] tells the operator which half they left open —
//! automatically, at load time, by name. The gap closes with one line of
//! config, and nothing breaks for anyone who has not read the release note yet.
//!
//! # Fail-closed on an unreadable file, absent on an absent one
//!
//! Inherited unchanged from `cmd/policy.rs`, and it is the distinction that
//! matters: a file that will not parse means somebody's intent is UNKNOWN, and
//! running anyway is silent degradation. No file means no ceiling.

use delonix_model::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::admission::Workload;
use crate::severity::Severity;

/// What the node does with a request that violates the policy.
///
/// The common vocabulary for this idea (`mode: "enforce" | "warn"`, as admission
/// controllers use it), so a policy written elsewhere maps onto it word for word.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Refuse the request. The default, and the behaviour that shipped.
    #[default]
    Enforce,
    /// Let it through, and say what would have been refused. For rolling a new
    /// rule out across a fleet before it bites.
    Warn,
}

/// A named profile of container hardening the node requires, on top of the
/// individual fields of [`SecurityPolicy`].
///
/// The vocabulary is the Kubernetes Pod Security Standards' on purpose: an
/// operator who knows what `baseline` refuses there knows what it refuses
/// here, and the OWASP Kubernetes Top 10 (K01, insecure workload
/// configuration) points at exactly that profile. Only `baseline` exists:
/// `restricted` needs the image's effective user at admission time, which this
/// gate runs before resolving, and a variant this engine cannot enforce would
/// be a promise in a type.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkloadSecurity {
    /// Refuses what lets a container out of its own isolation without being
    /// called `--privileged`: capabilities beyond the Pod Security baseline
    /// set (`--cap-add ALL`, `SYS_ADMIN`, …), the host's PID or IPC namespace,
    /// `seccomp=unconfined`/`apparmor=unconfined`, a bind mount of a host
    /// path, and a host device. `--privileged` itself is refused too. The
    /// host NETWORK is not part of it: it is this engine's default network
    /// mode, and [`SecurityPolicy::deny_host_network`] already decides it.
    Baseline,
}

/// What this node refuses to run.
///
/// Every field defaults to «no opinion», so a policy file states only what it
/// wants to restrict. A field nobody sets must never start refusing things —
/// this is the invariant that governs every future addition to this struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecurityPolicy {
    /// `enforce` (refuse) or `warn` (allow and report). Absent = `enforce`.
    #[serde(default)]
    pub mode: Mode,

    // ---- Container path. Shipped in v0.69.0 as `RuntimePolicy`; the names,
    // ---- defaults and meanings are frozen for backward compatibility.
    /// Refuse `--privileged`.
    #[serde(default)]
    pub deny_privileged: bool,
    /// Refuse `--net host`, which is this engine's DEFAULT network mode — a host
    /// that sets this is asking every workload to name a network explicitly.
    #[serde(default)]
    pub deny_host_network: bool,
    /// Refuse a container image reference with no tag or with `:latest`.
    ///
    /// `latest` is not a version: the same manifest gives a different container
    /// tomorrow, and an incident becomes unreproducible.
    #[serde(default)]
    pub deny_latest_tag: bool,
    /// Only these registries may be pulled from. Empty = no opinion.
    #[serde(default)]
    pub allowed_registries: Vec<String>,
    /// A hardening profile every container must meet. Absent = no opinion.
    ///
    /// `denyPrivileged` refuses one flag; this refuses what that flag is made
    /// of. Measured before it existed: a node with `denyPrivileged: true`
    /// admitted `--cap-add ALL --security-opt seccomp=unconfined --pid host
    /// -v /:/host`, which is the same container under four other names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_security: Option<WorkloadSecurity>,

    // ---- VM path. New, and the reason this struct exists.
    /// Refuse `vm create --device` (VFIO PCI passthrough).
    ///
    /// This is the VM's `--privileged`, and then some: a passed-through device
    /// gives the guest DMA to host hardware, which is a wider hole than any
    /// capability a privileged container gets. It had no node policy at all.
    #[serde(default)]
    pub deny_device_passthrough: bool,
    /// Refuse a VM disk image reference with no tag or with `:latest`.
    ///
    /// Separate from [`Self::deny_latest_tag`] on purpose — see the module doc.
    /// `vm --vmfile` builds tag their output `<name>:latest` by default, so
    /// folding the two would have refused a normal build on upgrade.
    #[serde(default)]
    pub deny_latest_vm_image: bool,
    /// Hosts a `vm create --url-img` qcow2 may be fetched from. Empty = no
    /// opinion.
    ///
    /// The CLI's own help is honest that, without a sibling `.sha256`, such a
    /// download is «trusted on TLS alone». An allowlist is the node-level
    /// answer to that: TLS proves you reached the host you named, not that the
    /// host deserved to be named.
    #[serde(default)]
    pub allowed_image_url_hosts: Vec<String>,

    // ---- Anti-spoofing GRANTS. These two invert the rule above: they do not
    // ---- restrict anything, they let something through, so «absent» means
    // ---- «not granted» and no policy file grants nothing.
    /// Source prefixes a container may be authorised to use besides its own
    /// address (`container run --allow-source`), for a container that routes.
    /// A request's prefix must sit inside one of these. Empty = none granted.
    #[serde(default)]
    pub allowed_source_prefixes: Vec<String>,
    /// Lets a container switch the source check off (`--no-source-check`).
    /// Exceptional: a workload without it can forge any address on its
    /// bridge. Every use is written to the event log. Absent = refused.
    #[serde(default)]
    pub allow_source_check_opt_out: bool,

    // ---- Egress towards private address space. A CEILING like the fields
    // ---- above the grants: absent = no opinion, byte for byte what a node did
    // ---- before it existed.
    /// Refuse NEW connections from a workload on an engine network to private
    /// address space reached through the node's uplink: RFC 1918
    /// (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`) and shared/CGNAT space
    /// (`100.64.0.0/10`). That is where the node's own management plane, its
    /// neighbours and the rest of the LAN live; a workload that can open a
    /// connection there can move laterally out of the network it was given.
    ///
    /// The workload's own network, and every network routed to it on this
    /// node, are not reached through the uplink and are never matched. Under
    /// `mode: warn` the connections are counted and let through.
    #[serde(default)]
    pub deny_private_egress: bool,
    /// Private destinations a workload may still open connections to while
    /// [`Self::deny_private_egress`] is on — a resolver, a gateway, a registry
    /// the node legitimately depends on. Canonical `a.b.c.d/len` prefixes. A
    /// GRANT: empty = none, and only the node policy can grant one, so a
    /// workload cannot reopen the range on its own.
    #[serde(default)]
    pub allowed_egress_prefixes: Vec<String>,
}

/// A policy that parses but probably does not mean what its author intended.
///
/// **Warnings, never rejections.** A contradiction that refuses to load would
/// break hosts on upgrade — the failure mode this whole module is written to
/// avoid. `deny_unknown_fields` already rejects the one class worth rejecting:
/// a typo, where the operator's intent is genuinely unknowable.
#[derive(Debug, Clone, PartialEq)]
pub struct Lint {
    /// Stable identifier, safe to grep for and to key an alert on.
    pub id: &'static str,
    /// Which field the operator should look at.
    pub field: &'static str,
    /// What is wrong, in one sentence, in English (the `-bin` translates).
    pub message: &'static str,
    pub severity: Severity,
    /// The path this lint is about, when it is about one.
    ///
    /// A warning about the VM path printed on every `container run` is noise
    /// the operator learns to scroll past, which is how a real finding gets
    /// missed. The caller shows a lint on the path it concerns.
    pub workload: Option<Workload>,
}

impl Lint {
    /// `true` when this lint is worth showing to someone doing `w`.
    pub fn applies_to(&self, w: Workload) -> bool {
        self.workload.is_none_or(|lw| lw == w)
    }
}

impl SecurityPolicy {
    /// Parses the policy. Pure — no filesystem, so it is testable without one.
    pub fn parse(raw: &str) -> Result<Self> {
        serde_json::from_str(raw).map_err(|e| Error::Invalid(e.to_string()))
    }

    /// `true` when the policy expresses no opinion about anything, and so
    /// refuses nothing. A file that exists and does this is almost always a
    /// mistake — see [`Self::lint`].
    pub fn is_silent(&self) -> bool {
        !self.deny_privileged
            && !self.deny_host_network
            && !self.deny_latest_tag
            && self.allowed_registries.is_empty()
            && self.workload_security.is_none()
            && !self.deny_device_passthrough
            && !self.deny_latest_vm_image
            && self.allowed_image_url_hosts.is_empty()
            // A policy that only GRANTS anti-spoofing exceptions says something
            // on purpose; calling it «probably a mistake» would be wrong.
            && self.allowed_source_prefixes.is_empty()
            && !self.allow_source_check_opt_out
            && !self.deny_private_egress
            // A grant for an egress exception says something on purpose too —
            // whether it has any effect is what `lint` is for.
            && self.allowed_egress_prefixes.is_empty()
    }

    /// `true` when at least one field of this policy could refuse a request
    /// on `w`'s path — the per-workload mirror of [`Self::is_silent`]. A
    /// policy that denies `--privileged` constrains the container path and
    /// says nothing about VMs; asking the wrong half of [`admission::
    /// evaluate`]'s split here is exactly the asymmetry [`Self::lint`]
    /// already reports at load time, turned into a question a caller can
    /// ask before admission instead of a warning only a terminal sees.
    ///
    /// Deliberately excludes the anti-spoofing GRANTS
    /// (`allowed_source_prefixes`, `allow_source_check_opt_out`): those are
    /// not evaluated by [`crate::admission::evaluate`] at all, so they are
    /// not what "constrains this workload's admission path" means here —
    /// mirrors exactly the fields [`crate::admission::evaluate`] actually
    /// reads for each [`Workload`]. The private-egress fields are excluded
    /// for the same reason: they are enforced in the node's dataplane, on
    /// every packet, not at admission.
    pub fn constrains(&self, w: Workload) -> bool {
        match w {
            Workload::Container => {
                self.deny_privileged
                    || self.deny_host_network
                    || self.deny_latest_tag
                    || !self.allowed_registries.is_empty()
                    || self.workload_security.is_some()
            }
            Workload::VirtualMachine => {
                self.deny_device_passthrough
                    || self.deny_latest_vm_image
                    || !self.allowed_image_url_hosts.is_empty()
            }
        }
    }

    /// Semantic checks that a schema cannot express.
    ///
    /// The three that matter are all the same shape: the operator guarded the
    /// container path and left the VM path — which is strictly more
    /// privileged — wide open. That asymmetry was invisible before this crate,
    /// and naming it at load time is what turns the new fields from an option
    /// nobody discovers into a gap the node reports on itself.
    pub fn lint(&self) -> Vec<Lint> {
        let mut out = Vec::new();

        if self.deny_privileged && !self.deny_device_passthrough {
            out.push(Lint {
                id: "POLICY-VM-PASSTHROUGH-OPEN",
                field: "denyDevicePassthrough",
                message: "this node refuses `--privileged` containers but allows \
                          `vm create --device` (VFIO PCI passthrough), which gives a \
                          guest DMA to host hardware — set `denyDevicePassthrough: true` \
                          unless passthrough is deliberate here",
                severity: Severity::High,
                workload: Some(Workload::VirtualMachine),
            });
        }

        // `denyPrivileged` alone reads like «no privileged containers» and
        // refuses one flag: every capability, the host's namespaces and its
        // filesystem stay reachable under other names. The node says so.
        if self.deny_privileged && self.workload_security.is_none() {
            out.push(Lint {
                id: "POLICY-PRIVILEGE-EQUIVALENTS-OPEN",
                field: "workloadSecurity",
                message: "this node refuses `--privileged` but admits what it is made of \
                          (`--cap-add ALL`, `--pid host`, `seccomp=unconfined`, `-v /:/host`, \
                          `--device`) — set `workloadSecurity: baseline` to refuse those too",
                severity: Severity::High,
                workload: Some(Workload::Container),
            });
        }

        if self.deny_latest_tag && !self.deny_latest_vm_image {
            out.push(Lint {
                id: "POLICY-VM-LATEST-OPEN",
                field: "denyLatestVmImage",
                message: "this node refuses `:latest` container images but not `:latest` \
                          VM disk images — set `denyLatestVmImage: true` for the same \
                          reproducibility guarantee on the VM path",
                severity: Severity::Medium,
                workload: Some(Workload::VirtualMachine),
            });
        }

        if !self.allowed_registries.is_empty() && self.allowed_image_url_hosts.is_empty() {
            out.push(Lint {
                id: "POLICY-VM-URL-OPEN",
                field: "allowedImageUrlHosts",
                message: "this node restricts container registries but `vm create --url-img` \
                          may still fetch a qcow2 from any host — list the hosts you trust \
                          in `allowedImageUrlHosts`",
                severity: Severity::Medium,
                workload: Some(Workload::VirtualMachine),
            });
        }

        // The private-egress guard sits on the engine's network uplink. A
        // workload outside every engine network — `--net host`, or a container
        // with its own slirp (no `--net`, only `-p`) — never crosses it, and
        // admission counts both as host network. Leaving them admitted reads
        // like protection and is not.
        if self.deny_private_egress && !self.deny_host_network {
            out.push(Lint {
                id: "POLICY-EGRESS-HOST-NET-OPEN",
                field: "denyHostNetwork",
                message: "this node refuses egress to private address space, but a workload \
                          started without `--net <network>` is not behind that guard — set \
                          `denyHostNetwork: true` so every container is placed on an engine \
                          network",
                severity: Severity::High,
                workload: Some(Workload::Container),
            });
        }

        // A grant with nothing to be an exception to.
        if !self.allowed_egress_prefixes.is_empty() && !self.deny_private_egress {
            out.push(Lint {
                id: "POLICY-EGRESS-GRANT-UNUSED",
                field: "allowedEgressPrefixes",
                message: "`allowedEgressPrefixes` is set but `denyPrivateEgress` is not, so \
                          nothing is refused and the grant changes nothing",
                severity: Severity::Low,
                workload: None,
            });
        }

        if self.is_silent() {
            out.push(Lint {
                id: "POLICY-SILENT",
                field: "",
                message: "this policy file refuses nothing — every field is at its default. \
                          A file that exists and expresses no opinion is usually a mistake; \
                          delete it, or state what the node should refuse",
                severity: Severity::Low,
                workload: None,
            });
        }

        // A registry allowlist takes HOSTS. An entry with a path is a reference,
        // and would never match — a rule that silently matches nothing is worse
        // than no rule, because it reads like protection.
        for entry in &self.allowed_registries {
            if entry.contains('/') {
                out.push(Lint {
                    id: "POLICY-REGISTRY-NOT-A-HOST",
                    field: "allowedRegistries",
                    message: "an entry in `allowedRegistries` contains `/` — this field takes \
                              registry HOSTS (`docker.io`, `ghcr.io`), not image references, \
                              and an entry with a path can never match",
                    severity: Severity::Medium,
                    workload: None,
                });
                break;
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file a v0.69.0 node has in `<root>/policy.json` today.
    const SHIPPED: &str = r#"{
        "denyPrivileged": true,
        "denyHostNetwork": true,
        "denyLatestTag": true,
        "allowedRegistries": ["docker.io", "ghcr.io"]
    }"#;

    #[test]
    fn todays_policy_still_parses_and_still_means_the_same() {
        // The regression this test locks shut: a new field changing how a file
        // already in production is read.
        let p = SecurityPolicy::parse(SHIPPED).unwrap();
        assert!(p.deny_privileged);
        assert!(p.deny_host_network);
        assert!(p.deny_latest_tag);
        assert_eq!(p.allowed_registries, ["docker.io", "ghcr.io"]);
        // And the VM path still has no opinion — nothing starts being refused.
        assert!(!p.deny_device_passthrough);
        assert!(!p.deny_latest_vm_image);
        assert!(p.allowed_image_url_hosts.is_empty());
        assert_eq!(p.mode, Mode::Enforce);
    }

    #[test]
    fn an_empty_file_is_a_policy_with_no_opinion_not_an_error() {
        let p = SecurityPolicy::parse("{}").unwrap();
        assert_eq!(p, SecurityPolicy::default());
        assert!(p.is_silent());
    }

    #[test]
    fn an_unknown_field_is_refused_because_the_intent_is_unknowable() {
        // A misspelt `denyPriviledged` must not read as «no opinion».
        let e = SecurityPolicy::parse(r#"{"denyPriviledged": true}"#).unwrap_err();
        assert!(format!("{e}").contains("denyPriviledged"), "{e}");
    }

    #[test]
    fn a_vm_lint_is_not_shown_to_someone_running_a_container() {
        // A warning about VMs on every `container run` is noise an operator
        // learns to scroll past — which is how a real finding gets missed.
        let p = SecurityPolicy::parse(SHIPPED).unwrap();
        let em_container: Vec<_> = p
            .lint()
            .into_iter()
            .filter(|l| l.applies_to(Workload::Container))
            .map(|l| l.id)
            .collect();
        // The one container lint is about the container path itself: the
        // shipped policy refuses `--privileged` and admits its equivalents.
        assert_eq!(em_container, ["POLICY-PRIVILEGE-EQUIVALENTS-OPEN"]);
        let em_vm: Vec<_> = p
            .lint()
            .into_iter()
            .filter(|l| l.applies_to(Workload::VirtualMachine))
            .map(|l| l.id)
            .collect();
        assert_eq!(em_vm.len(), 3, "{em_vm:?}");
    }

    #[test]
    fn lint_names_the_vm_path_the_operator_left_open() {
        let p = SecurityPolicy::parse(SHIPPED).unwrap();
        let ids: Vec<_> = p.lint().iter().map(|l| l.id).collect();
        assert!(ids.contains(&"POLICY-VM-PASSTHROUGH-OPEN"), "{ids:?}");
        assert!(ids.contains(&"POLICY-VM-LATEST-OPEN"), "{ids:?}");
        assert!(ids.contains(&"POLICY-VM-URL-OPEN"), "{ids:?}");
    }

    #[test]
    fn a_complete_policy_produces_no_lint_at_all() {
        let p = SecurityPolicy::parse(
            r#"{
                "denyPrivileged": true, "denyHostNetwork": true, "denyLatestTag": true,
                "allowedRegistries": ["ghcr.io"], "workloadSecurity": "baseline",
                "denyDevicePassthrough": true, "denyLatestVmImage": true,
                "allowedImageUrlHosts": ["cloud.debian.org"]
            }"#,
        )
        .unwrap();
        assert_eq!(p.lint(), vec![]);
    }

    #[test]
    fn lint_catches_a_reference_put_where_a_host_was_expected() {
        let p =
            SecurityPolicy::parse(r#"{"allowedRegistries": ["ghcr.io/angolardevops"]}"#).unwrap();
        assert!(p
            .lint()
            .iter()
            .any(|l| l.id == "POLICY-REGISTRY-NOT-A-HOST"));
    }

    #[test]
    fn a_policy_that_only_grants_is_not_silent() {
        let p = SecurityPolicy::parse(r#"{"allowedSourcePrefixes": ["10.99.0.0/16"]}"#).unwrap();
        assert!(!p.is_silent());
        let p = SecurityPolicy::parse(r#"{"allowSourceCheckOptOut": true}"#).unwrap();
        assert!(!p.is_silent());
    }

    #[test]
    fn a_file_that_refuses_nothing_is_pointed_at() {
        let p = SecurityPolicy::parse("{}").unwrap();
        assert!(p.lint().iter().any(|l| l.id == "POLICY-SILENT"));
    }

    #[test]
    fn constrains_is_scoped_to_the_workloads_own_path() {
        let container_only = SecurityPolicy {
            deny_privileged: true,
            ..Default::default()
        };
        assert!(container_only.constrains(Workload::Container));
        assert!(!container_only.constrains(Workload::VirtualMachine));

        let vm_only = SecurityPolicy {
            deny_device_passthrough: true,
            ..Default::default()
        };
        assert!(!vm_only.constrains(Workload::Container));
        assert!(vm_only.constrains(Workload::VirtualMachine));

        assert!(!SecurityPolicy::default().constrains(Workload::Container));
        assert!(!SecurityPolicy::default().constrains(Workload::VirtualMachine));
    }

    #[test]
    fn constrains_ignores_the_anti_spoofing_grants() {
        // A policy that only grants `--allow-source`/`--no-source-check`
        // constrains nothing: those two fields are not read by
        // `admission::evaluate` at all, for either workload.
        let p = SecurityPolicy {
            allowed_source_prefixes: vec!["10.99.0.0/16".into()],
            allow_source_check_opt_out: true,
            ..Default::default()
        };
        assert!(!p.constrains(Workload::Container));
        assert!(!p.constrains(Workload::VirtualMachine));
    }

    #[test]
    fn todays_policy_has_no_opinion_on_private_egress() {
        // Upgrading must not start refusing anything: a file written before the
        // field existed leaves the guard off and grants nothing.
        let p = SecurityPolicy::parse(SHIPPED).unwrap();
        assert!(!p.deny_private_egress);
        assert!(p.allowed_egress_prefixes.is_empty());
    }

    #[test]
    fn the_private_egress_fields_parse_by_their_manifest_names() {
        let p = SecurityPolicy::parse(
            r#"{"denyPrivateEgress": true, "allowedEgressPrefixes": ["192.168.50.10/32"]}"#,
        )
        .unwrap();
        assert!(p.deny_private_egress);
        assert_eq!(p.allowed_egress_prefixes, ["192.168.50.10/32"]);
        assert!(!p.is_silent());
        // Enforced in the dataplane, not at admission.
        assert!(!p.constrains(Workload::Container));
        assert!(!p.constrains(Workload::VirtualMachine));
    }

    #[test]
    fn a_private_egress_guard_without_host_network_denied_is_pointed_at() {
        let p = SecurityPolicy::parse(r#"{"denyPrivateEgress": true}"#).unwrap();
        let ids: Vec<_> = p.lint().iter().map(|l| l.id).collect();
        assert!(ids.contains(&"POLICY-EGRESS-HOST-NET-OPEN"), "{ids:?}");
        let p = SecurityPolicy::parse(r#"{"denyPrivateEgress": true, "denyHostNetwork": true}"#)
            .unwrap();
        assert!(!p
            .lint()
            .iter()
            .any(|l| l.id == "POLICY-EGRESS-HOST-NET-OPEN"));
    }

    #[test]
    fn an_egress_grant_with_nothing_denied_is_pointed_at() {
        let p = SecurityPolicy::parse(r#"{"allowedEgressPrefixes": ["10.9.0.0/16"]}"#).unwrap();
        assert!(p
            .lint()
            .iter()
            .any(|l| l.id == "POLICY-EGRESS-GRANT-UNUSED"));
    }

    #[test]
    fn mode_warn_parses_and_the_default_is_enforce() {
        assert_eq!(
            SecurityPolicy::parse(r#"{"mode":"warn"}"#).unwrap().mode,
            Mode::Warn
        );
        assert_eq!(SecurityPolicy::parse("{}").unwrap().mode, Mode::Enforce);
    }

    #[test]
    fn workload_security_parses_by_its_manifest_name_and_only_as_baseline() {
        let p = SecurityPolicy::parse(r#"{"workloadSecurity": "baseline"}"#).unwrap();
        assert_eq!(p.workload_security, Some(WorkloadSecurity::Baseline));
        assert!(p.constrains(Workload::Container));
        assert!(!p.constrains(Workload::VirtualMachine));
        assert!(!p.is_silent());
        // `restricted` is not something this gate can enforce: refused, never
        // read as a weaker profile.
        assert!(SecurityPolicy::parse(r#"{"workloadSecurity": "restricted"}"#).is_err());
    }

    /// Absent stays absent on the way back out: a `policy.json` written by
    /// this build for a node that never asked for the field is byte for byte
    /// what the previous build wrote.
    #[test]
    fn an_unset_workload_security_is_not_written() {
        let p = SecurityPolicy {
            deny_privileged: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("workloadSecurity"), "{json}");
    }

    #[test]
    fn deny_privileged_without_a_profile_names_the_equivalents_it_admits() {
        let p = SecurityPolicy {
            deny_privileged: true,
            ..Default::default()
        };
        let ids: Vec<_> = p.lint().iter().map(|l| l.id).collect();
        assert!(
            ids.contains(&"POLICY-PRIVILEGE-EQUIVALENTS-OPEN"),
            "{ids:?}"
        );
        let closed = SecurityPolicy {
            workload_security: Some(WorkloadSecurity::Baseline),
            ..p
        };
        let ids: Vec<_> = closed.lint().iter().map(|l| l.id).collect();
        assert!(
            !ids.contains(&"POLICY-PRIVILEGE-EQUIVALENTS-OPEN"),
            "{ids:?}"
        );
    }
}

//! The node's admission gate — **one** evaluation, for every kind of workload.
//!
//! # The gap this closes
//!
//! Measured on `origin/main` at v0.69.0: `cmd/policy.rs::enforce` has exactly
//! one caller, `cmd/container.rs:2944`. `cmd/vm.rs` has none — its thirteen
//! matches for «policy» are all `restartPolicy`. So a node that set
//! `denyPrivileged: true` refused `container run --privileged` and accepted
//! `vm create --device 0000:01:00.0`, which hands the guest DMA to host
//! hardware: a strictly wider hole than the one it was refusing.
//!
//! That is the second cloud-native pillar — *whoever admits is ONE point* — and
//! it was failing. This module is the one point.
//!
//! # What it deliberately does not have
//!
//! No `RequireApproval` decision. Approval needs an approver, an approver needs
//! an identity, and identity and tenancy are outside the engine by ADR-0010
//! (Rejected, 2026-08-10) and ADR-0025 (Accepted, 2026-08-29). A variant this
//! repo cannot produce would be a promise in a type signature — the class of
//! dishonesty the engine refuses everywhere else. The layer that HAS approvers
//! wraps this one.

use crate::policy::{Mode, SecurityPolicy, WorkloadSecurity};
use crate::severity::{ActionRisk, Severity};

/// What is being admitted. The enum exists so a rule can say «container only»
/// and mean it, instead of a VM caller passing `false` for a field that has no
/// VM meaning and quietly satisfying a container rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workload {
    Container,
    VirtualMachine,
}

impl Workload {
    pub fn as_str(&self) -> &'static str {
        match self {
            Workload::Container => "container",
            Workload::VirtualMachine => "vm",
        }
    }
}

const NO_DEVICES: &[String] = &[];

/// The capabilities a container may ADD under `workloadSecurity: baseline` —
/// the Kubernetes Pod Security Standards baseline list, verbatim. Anything
/// else (`ALL`, `SYS_ADMIN`, `NET_ADMIN`, `SYS_PTRACE`, `BPF`, …) is a way out
/// of the container or into its neighbours.
pub const BASELINE_ADDABLE_CAPS: &[&str] = &[
    "AUDIT_WRITE",
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "KILL",
    "MKNOD",
    "NET_BIND_SERVICE",
    "SETFCAP",
    "SETGID",
    "SETPCAP",
    "SETUID",
    "SYS_CHROOT",
];

/// What a container asks for beyond `--privileged` that can take it out of
/// its own isolation. Read only by `workloadSecurity` rules; every field
/// defaults to «asks for nothing», so a caller that does not fill it in
/// cannot make a request look worse than it is — only a caller that does can
/// be refused for it.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContainerHardening<'a> {
    /// `--cap-add`, as written.
    pub cap_add: &'a [String],
    pub host_pid: bool,
    pub host_ipc: bool,
    pub seccomp_unconfined: bool,
    pub apparmor_unconfined: bool,
    /// `-v` specifications, as written; the ones whose source is a host path
    /// (`/…` or `./…`, the engine's own rule) are the ones that count.
    pub volumes: &'a [String],
    /// `--device`, as written.
    pub devices: &'a [String],
}

/// The capability names in `cap_add` the baseline does not allow, normalised
/// (`cap_sys_admin` → `SYS_ADMIN`), in the order given and without repeats.
pub fn caps_beyond_baseline(cap_add: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in cap_add {
        let up = c.trim().to_ascii_uppercase();
        let name = up.strip_prefix("CAP_").unwrap_or(&up).to_string();
        if !BASELINE_ADDABLE_CAPS.contains(&name.as_str()) && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// The source of each `-v` spec that is a host path rather than a named
/// volume — the same `/…`-or-`.…` rule `delonix-volume` uses to decide.
pub fn host_path_sources(volumes: &[String]) -> Vec<String> {
    volumes
        .iter()
        .filter_map(|v| v.split(':').next())
        .filter(|src| src.starts_with('/') || src.starts_with('.'))
        .map(str::to_string)
        .collect()
}

/// What is being asked of the node.
#[derive(Debug)]
pub struct Request<'a> {
    pub workload: Workload,
    /// The image reference, when the request names one.
    pub image: Option<&'a str>,
    /// Container only.
    pub privileged: bool,
    /// Container only.
    pub host_network: bool,
    /// VM only — `vm create --device`, VFIO PCI passthrough.
    pub devices: &'a [String],
    /// VM only — `vm create --url-img`, a qcow2 fetched over the network.
    pub image_url: Option<&'a str>,
    /// Container only — what `workloadSecurity` judges.
    pub hardening: ContainerHardening<'a>,
}

impl<'a> Request<'a> {
    /// A container run. Field-for-field the old `cmd/policy.rs::Request`.
    pub fn container(image: &'a str, privileged: bool, host_network: bool) -> Self {
        Request {
            workload: Workload::Container,
            image: Some(image),
            privileged,
            host_network,
            devices: NO_DEVICES,
            image_url: None,
            hardening: ContainerHardening::default(),
        }
    }

    /// The same request, carrying what `workloadSecurity` judges.
    pub fn with_hardening(mut self, h: ContainerHardening<'a>) -> Self {
        self.hardening = h;
        self
    }

    /// A VM create. `image` is the local disk image when one is named; a VM
    /// booted from `--url-img` names no local image, and passes `None` rather
    /// than an invented one.
    pub fn virtual_machine(
        image: Option<&'a str>,
        devices: &'a [String],
        image_url: Option<&'a str>,
    ) -> Self {
        Request {
            workload: Workload::VirtualMachine,
            image,
            privileged: false,
            host_network: false,
            devices,
            image_url,
            hardening: ContainerHardening::default(),
        }
    }
}

/// A stable identifier per rule. Safe to grep, to key an alert on, and to
/// translate against — the message wording may change, this may not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    Privileged,
    HostNetwork,
    LatestTag,
    Registry,
    DevicePassthrough,
    LatestVmImage,
    ImageUrlHost,
    BaselineCapabilities,
    BaselineHostNamespace,
    BaselineUnconfined,
    BaselineHostPath,
    BaselineDevice,
}

impl Rule {
    pub fn id(&self) -> &'static str {
        match self {
            Rule::Privileged => "ADM-PRIVILEGED",
            Rule::HostNetwork => "ADM-HOST-NETWORK",
            Rule::LatestTag => "ADM-LATEST-TAG",
            Rule::Registry => "ADM-REGISTRY",
            Rule::DevicePassthrough => "ADM-DEVICE-PASSTHROUGH",
            Rule::LatestVmImage => "ADM-LATEST-VM-IMAGE",
            Rule::ImageUrlHost => "ADM-IMAGE-URL-HOST",
            Rule::BaselineCapabilities => "ADM-BASELINE-CAPABILITIES",
            Rule::BaselineHostNamespace => "ADM-BASELINE-HOST-NAMESPACE",
            Rule::BaselineUnconfined => "ADM-BASELINE-UNCONFINED",
            Rule::BaselineHostPath => "ADM-BASELINE-HOST-PATH",
            Rule::BaselineDevice => "ADM-BASELINE-DEVICE",
        }
    }
}

/// One reason the node refuses, carrying exactly the data its message needs.
///
/// A typed variant per rule instead of a rendered `String`: the caller that
/// prints this is the `-bin`, which owns i18n, and a library that hands back
/// pre-translated English would make the Portuguese catalog unreachable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    Privileged,
    HostNetwork,
    LatestTag {
        image: String,
    },
    Registry {
        image: String,
        host: String,
        allowed: Vec<String>,
    },
    DevicePassthrough {
        devices: Vec<String>,
    },
    LatestVmImage {
        image: String,
    },
    ImageUrlHost {
        url: String,
        host: String,
        allowed: Vec<String>,
    },
    /// Capabilities beyond the baseline's addable set.
    BaselineCapabilities {
        caps: Vec<String>,
    },
    /// `pid`/`ipc` — the host namespaces asked for.
    BaselineHostNamespace {
        namespaces: Vec<String>,
    },
    /// `seccomp`/`apparmor` — the confinement switched off.
    BaselineUnconfined {
        profiles: Vec<String>,
    },
    /// Host paths bind-mounted into the container.
    BaselineHostPath {
        paths: Vec<String>,
    },
    /// Host devices handed to the container.
    BaselineDevice {
        devices: Vec<String>,
    },
}

impl Violation {
    pub fn rule(&self) -> Rule {
        match self {
            Violation::Privileged => Rule::Privileged,
            Violation::HostNetwork => Rule::HostNetwork,
            Violation::LatestTag { .. } => Rule::LatestTag,
            Violation::Registry { .. } => Rule::Registry,
            Violation::DevicePassthrough { .. } => Rule::DevicePassthrough,
            Violation::LatestVmImage { .. } => Rule::LatestVmImage,
            Violation::ImageUrlHost { .. } => Rule::ImageUrlHost,
            Violation::BaselineCapabilities { .. } => Rule::BaselineCapabilities,
            Violation::BaselineHostNamespace { .. } => Rule::BaselineHostNamespace,
            Violation::BaselineUnconfined { .. } => Rule::BaselineUnconfined,
            Violation::BaselineHostPath { .. } => Rule::BaselineHostPath,
            Violation::BaselineDevice { .. } => Rule::BaselineDevice,
        }
    }

    /// How much this matters if it is real. Admission is deterministic, so
    /// confidence is always [`crate::severity::Confidence::CERTAIN`] and only
    /// this axis varies.
    pub fn severity(&self) -> Severity {
        match self {
            // Both hand the workload a way out of its own isolation.
            Violation::Privileged | Violation::DevicePassthrough { .. } => Severity::Critical,
            // Each one alone is enough: `-v /:/host` as root is the host's
            // filesystem, `SYS_ADMIN` mounts it, a raw block device is the disk.
            Violation::BaselineCapabilities { .. }
            | Violation::BaselineHostPath { .. }
            | Violation::BaselineDevice { .. } => Severity::Critical,
            // Weaken the boundary rather than cross it: another process's
            // memory and signals, or the syscall surface the filter removes.
            Violation::BaselineHostNamespace { .. } | Violation::BaselineUnconfined { .. } => {
                Severity::High
            }
            // Reaching an unvetted host to fetch something you will then BOOT.
            Violation::ImageUrlHost { .. } => Severity::High,
            Violation::HostNetwork | Violation::Registry { .. } => Severity::High,
            // Reproducibility, not containment.
            Violation::LatestTag { .. } | Violation::LatestVmImage { .. } => Severity::Medium,
        }
    }

    /// The risk of the operation the violation describes.
    pub fn action_risk(&self) -> ActionRisk {
        match self {
            Violation::Privileged
            | Violation::DevicePassthrough { .. }
            | Violation::HostNetwork
            | Violation::BaselineCapabilities { .. }
            | Violation::BaselineHostNamespace { .. }
            | Violation::BaselineUnconfined { .. }
            | Violation::BaselineHostPath { .. }
            | Violation::BaselineDevice { .. } => ActionRisk::Privileged,
            _ => ActionRisk::SafeWrite,
        }
    }
}

/// What the node decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// `mode: warn` — the request proceeds, and these would have refused it.
    AllowWithWarnings(Vec<Violation>),
    Deny(Vec<Violation>),
}

impl Decision {
    pub fn is_denied(&self) -> bool {
        matches!(self, Decision::Deny(_))
    }

    pub fn violations(&self) -> &[Violation] {
        match self {
            Decision::Allow => &[],
            Decision::AllowWithWarnings(v) | Decision::Deny(v) => v,
        }
    }
}

/// Every reason this policy refuses the request — all of them, not the first.
///
/// Reporting one at a time turns fixing a manifest into one round trip per
/// violation, which is the drip-feed that makes people stop reading output.
/// Same reason `stack plan` names every cold field at once.
pub fn evaluate(policy: &SecurityPolicy, r: &Request<'_>) -> Decision {
    let mut v = Vec::new();

    // ---- Container path. Order and semantics frozen: these four shipped.
    if r.workload == Workload::Container {
        if policy.deny_privileged && r.privileged {
            v.push(Violation::Privileged);
        }
        if policy.deny_host_network && r.host_network {
            v.push(Violation::HostNetwork);
        }
        if policy.workload_security == Some(WorkloadSecurity::Baseline) {
            baseline(r, policy.deny_privileged, &mut v);
        }
        if let Some(image) = r.image {
            if policy.deny_latest_tag && is_latest(image) {
                v.push(Violation::LatestTag {
                    image: image.to_string(),
                });
            }
            if !policy.allowed_registries.is_empty() {
                let host = registry_of(image);
                if !policy.allowed_registries.iter().any(|a| a == &host) {
                    v.push(Violation::Registry {
                        image: image.to_string(),
                        host,
                        allowed: policy.allowed_registries.clone(),
                    });
                }
            }
        }
    }

    // ---- VM path. New; every rule here is off unless the operator turned it on.
    if r.workload == Workload::VirtualMachine {
        if policy.deny_device_passthrough && !r.devices.is_empty() {
            v.push(Violation::DevicePassthrough {
                devices: r.devices.to_vec(),
            });
        }
        if let Some(image) = r.image {
            if policy.deny_latest_vm_image && is_latest(image) {
                v.push(Violation::LatestVmImage {
                    image: image.to_string(),
                });
            }
        }
        if !policy.allowed_image_url_hosts.is_empty() {
            if let Some(url) = r.image_url {
                // An unparseable URL is refused, not ignored: an allowlist that
                // waves through what it could not read is not an allowlist.
                let host = url_host(url).unwrap_or_default();
                let ok = !host.is_empty()
                    && policy
                        .allowed_image_url_hosts
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(&host));
                if !ok {
                    v.push(Violation::ImageUrlHost {
                        url: url.to_string(),
                        host,
                        allowed: policy.allowed_image_url_hosts.clone(),
                    });
                }
            }
        }
    }

    if v.is_empty() {
        Decision::Allow
    } else if policy.mode == Mode::Warn {
        Decision::AllowWithWarnings(v)
    } else {
        Decision::Deny(v)
    }
}

/// The `workloadSecurity: baseline` rules, appended to `v` in a fixed order.
/// `privileged_already` keeps a `--privileged` from being reported twice when
/// `denyPrivileged` refused it already.
fn baseline(r: &Request<'_>, privileged_already: bool, v: &mut Vec<Violation>) {
    let h = &r.hardening;
    if r.privileged && !privileged_already {
        v.push(Violation::Privileged);
    }
    let caps = caps_beyond_baseline(h.cap_add);
    if !caps.is_empty() {
        v.push(Violation::BaselineCapabilities { caps });
    }
    let mut namespaces = Vec::new();
    if h.host_pid {
        namespaces.push("pid".to_string());
    }
    if h.host_ipc {
        namespaces.push("ipc".to_string());
    }
    if !namespaces.is_empty() {
        v.push(Violation::BaselineHostNamespace { namespaces });
    }
    let mut profiles = Vec::new();
    if h.seccomp_unconfined {
        profiles.push("seccomp".to_string());
    }
    if h.apparmor_unconfined {
        profiles.push("apparmor".to_string());
    }
    if !profiles.is_empty() {
        v.push(Violation::BaselineUnconfined { profiles });
    }
    let paths = host_path_sources(h.volumes);
    if !paths.is_empty() {
        v.push(Violation::BaselineHostPath { paths });
    }
    if !h.devices.is_empty() {
        v.push(Violation::BaselineDevice {
            devices: h.devices.to_vec(),
        });
    }
}

/// The registry host of a reference, using Docker's own rule.
///
/// The first segment is a registry only when it looks like a host — it contains
/// a `.` or a `:`, or it is `localhost`. Without that rule `library/alpine`
/// would have `library` as its registry, and `alpine` would have none.
///
/// Moved verbatim from `cmd/policy.rs`; the tests that pinned it moved with it.
pub fn registry_of(image: &str) -> String {
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
pub fn is_latest(image: &str) -> bool {
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

/// The host an absolute `http(s)` URL points at, lowercased and without port.
///
/// `None` for anything this cannot read with certainty — another scheme, no
/// authority, an empty host. The caller treats `None` as «refuse», never as
/// «allow»: a gate that fails open on input it did not understand is not a gate.
///
/// **The `@` trap.** `https://cloud.debian.org@evil.com/x.qcow2` has host
/// `evil.com` — everything before the LAST `@` in the authority is userinfo.
/// Reading the first segment instead is the classic allowlist bypass, and it is
/// the same family as the substring trap this repo already paid for on
/// `10.0.0.5` matching `10.0.0.50`.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;

    // Authority ends at the first `/`, `?` or `#`.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }

    // Userinfo is everything up to the LAST `@`.
    let hostport = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };

    // Strip the port. An IPv6 literal is bracketed, so the last `:` outside the
    // brackets is the port separator.
    let host = if let Some(end) = hostport.find(']') {
        &hostport[..=end]
    } else {
        match hostport.rsplit_once(':') {
            Some((h, _)) => h,
            None => hostport,
        }
    };

    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Mode;

    fn strict() -> SecurityPolicy {
        SecurityPolicy {
            deny_privileged: true,
            deny_host_network: true,
            deny_latest_tag: true,
            allowed_registries: vec!["ghcr.io".into()],
            deny_device_passthrough: true,
            deny_latest_vm_image: true,
            allowed_image_url_hosts: vec!["cloud.debian.org".into()],
            ..Default::default()
        }
    }

    // ---- The contract that must not move: the container path --------------

    #[test]
    fn an_empty_policy_refuses_nothing_on_either_path() {
        // The upgrade path: a node that never wrote a policy behaves exactly as
        // it did before — and that now holds for VMs too.
        let p = SecurityPolicy::default();
        assert_eq!(
            evaluate(&p, &Request::container("alpine:latest", true, true)),
            Decision::Allow
        );
        let devices = vec!["0000:01:00.0".to_string()];
        assert_eq!(
            evaluate(
                &p,
                &Request::virtual_machine(
                    Some("golden:latest"),
                    &devices,
                    Some("http://x/y.qcow2")
                )
            ),
            Decision::Allow
        );
    }

    #[test]
    fn a_container_is_refused_for_every_reason_at_once_not_the_first() {
        let d = evaluate(&strict(), &Request::container("docker.io/app", true, true));
        let rules: Vec<_> = d.violations().iter().map(|v| v.rule()).collect();
        assert!(d.is_denied());
        assert_eq!(
            rules,
            vec![
                Rule::Privileged,
                Rule::HostNetwork,
                Rule::LatestTag,
                Rule::Registry
            ]
        );
    }

    #[test]
    fn an_allowed_registry_is_matched_whole_never_as_a_substring() {
        // This repo has already paid for a substring match on an IP (`10.0.0.5`
        // matching `10.0.0.50`) in the overlay peer code.
        let p = SecurityPolicy {
            allowed_registries: vec!["ghcr.io".into()],
            ..Default::default()
        };
        assert_eq!(
            evaluate(&p, &Request::container("ghcr.io/org/app:1.0", false, false)),
            Decision::Allow
        );
        assert!(evaluate(
            &p,
            &Request::container("evil-ghcr.io/org/app:1.0", false, false)
        )
        .is_denied());
        assert!(evaluate(
            &p,
            &Request::container("ghcr.io.evil.com/app:1.0", false, false)
        )
        .is_denied());
    }

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

    #[test]
    fn untagged_counts_as_latest_and_a_digest_does_not() {
        assert!(is_latest("alpine"));
        assert!(is_latest("alpine:latest"));
        assert!(!is_latest("alpine:3.20"));
        assert!(!is_latest("alpine@sha256:aaaa"));
        // A port in the registry is not a tag separator.
        assert!(is_latest("localhost:5000/app"));
        assert!(!is_latest("localhost:5000/app:1.2"));
    }

    // ---- The measured gap, and the test that locks it shut ----------------

    #[test]
    fn a_vm_with_passthrough_stops_passing_once_the_node_refuses_it() {
        // The regression this test locks shut is the gap measured at v0.69.0:
        // `policy::enforce` had one caller (container.rs), so a node with
        // `denyPrivileged` refused a privileged container and accepted a VM
        // holding DMA to host hardware.
        let devices = vec!["0000:01:00.0".to_string(), "0000:02:00.0".to_string()];
        let d = evaluate(&strict(), &Request::virtual_machine(None, &devices, None));
        assert!(d.is_denied());
        assert_eq!(
            d.violations()[0],
            Violation::DevicePassthrough {
                devices: devices.clone()
            }
        );
        assert_eq!(d.violations()[0].severity(), Severity::Critical);
    }

    #[test]
    fn a_container_rule_never_fires_on_a_vm_request() {
        // The mirror image of the bug: silently widening `denyPrivileged` to
        // VMs would break existing nodes on upgrade. VMs get their own fields,
        // and only those.
        let p = SecurityPolicy {
            deny_privileged: true,
            deny_host_network: true,
            deny_latest_tag: true,
            allowed_registries: vec!["ghcr.io".into()],
            ..Default::default() // the VM fields stay off
        };
        let devices = vec!["0000:01:00.0".to_string()];
        assert_eq!(
            evaluate(
                &p,
                &Request::virtual_machine(
                    Some("docker.io/x:latest"),
                    &devices,
                    Some("http://evil/x")
                )
            ),
            Decision::Allow
        );
    }

    #[test]
    fn a_qcow2_from_a_host_off_the_list_is_refused() {
        let d = evaluate(
            &strict(),
            &Request::virtual_machine(None, NO_DEVICES, Some("https://evil.example/x.qcow2")),
        );
        assert!(d.is_denied());
        assert!(
            matches!(&d.violations()[0], Violation::ImageUrlHost { host, .. } if host == "evil.example")
        );
    }

    #[test]
    fn a_qcow2_from_an_allowed_host_passes_with_a_port_and_in_caps() {
        for url in [
            "https://cloud.debian.org/images/x.qcow2",
            "https://cloud.debian.org:443/images/x.qcow2",
            "https://CLOUD.Debian.ORG/images/x.qcow2",
        ] {
            assert_eq!(
                evaluate(
                    &strict(),
                    &Request::virtual_machine(None, NO_DEVICES, Some(url))
                ),
                Decision::Allow,
                "{url}"
            );
        }
    }

    // ---- As armadilhas de allowlist ---------------------------------------

    #[test]
    fn the_at_sign_trick_does_not_fool_the_host_allowlist() {
        // `https://allowed@malicious/` has host `malicious`. Reading the first
        // segment instead of the last is the classic allowlist bypass.
        assert_eq!(
            url_host("https://cloud.debian.org@evil.com/x"),
            Some("evil.com".into())
        );
        assert_eq!(url_host("https://a@b@evil.com/x"), Some("evil.com".into()));
        let d = evaluate(
            &strict(),
            &Request::virtual_machine(
                None,
                NO_DEVICES,
                Some("https://cloud.debian.org@evil.com/x.qcow2"),
            ),
        );
        assert!(d.is_denied(), "the @ trick got through: {d:?}");
    }

    #[test]
    fn a_url_that_cannot_be_read_is_refused_not_ignored() {
        // A gate that waves through what it could not read is not a gate.
        for url in [
            "ftp://x/y.qcow2",
            "https://",
            "not-a-url",
            "https:///x",
            "file:///etc/shadow",
        ] {
            let d = evaluate(
                &strict(),
                &Request::virtual_machine(None, NO_DEVICES, Some(url)),
            );
            assert!(d.is_denied(), "{url} should have been refused");
        }
    }

    #[test]
    fn url_host_reads_ipv6_and_a_port_without_getting_lost() {
        assert_eq!(
            url_host("https://[2001:db8::1]:8443/x"),
            Some("[2001:db8::1]".into())
        );
        assert_eq!(
            url_host("http://192.0.2.10:8080/x"),
            Some("192.0.2.10".into())
        );
        assert_eq!(url_host("https://host/x?a=b#c"), Some("host".into()));
    }

    // ---- Warn mode --------------------------------------------------------

    #[test]
    fn warn_mode_lets_it_through_but_says_what_would_have_refused() {
        let p = SecurityPolicy {
            mode: Mode::Warn,
            ..strict()
        };
        let d = evaluate(&p, &Request::container("docker.io/app", true, false));
        assert!(!d.is_denied());
        assert!(matches!(d, Decision::AllowWithWarnings(_)));
        assert_eq!(d.violations().len(), 3);
    }

    #[test]
    fn severity_puts_passthrough_and_privileged_on_the_same_top_step() {
        assert_eq!(Violation::Privileged.severity(), Severity::Critical);
        assert_eq!(
            Violation::DevicePassthrough { devices: vec![] }.severity(),
            Severity::Critical
        );
        assert_eq!(
            Violation::LatestTag { image: "x".into() }.severity(),
            Severity::Medium
        );
    }

    #[test]
    fn every_rule_has_a_stable_and_unique_id() {
        let all = [
            Rule::Privileged,
            Rule::HostNetwork,
            Rule::LatestTag,
            Rule::Registry,
            Rule::DevicePassthrough,
            Rule::LatestVmImage,
            Rule::ImageUrlHost,
            Rule::BaselineCapabilities,
            Rule::BaselineHostNamespace,
            Rule::BaselineUnconfined,
            Rule::BaselineHostPath,
            Rule::BaselineDevice,
        ];
        let mut ids: Vec<_> = all.iter().map(|r| r.id()).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate rule ids");
    }

    // ---- workloadSecurity: baseline ----------------------------------------

    fn baseline_only() -> SecurityPolicy {
        SecurityPolicy {
            workload_security: Some(WorkloadSecurity::Baseline),
            ..Default::default()
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// The bypass this rule exists for, measured before it did: a node with
    /// `denyPrivileged: true` admitted the same container under other names.
    #[test]
    fn deny_privileged_alone_admits_the_privilege_equivalents_and_baseline_refuses_them() {
        let caps = s(&["ALL"]);
        let vols = s(&["/:/host"]);
        let devs = s(&["/dev/sda"]);
        let h = ContainerHardening {
            cap_add: &caps,
            host_pid: true,
            host_ipc: true,
            seccomp_unconfined: true,
            apparmor_unconfined: true,
            volumes: &vols,
            devices: &devs,
        };
        let r = Request::container("alpine:3.20", false, false).with_hardening(h);

        let only_privileged = SecurityPolicy {
            deny_privileged: true,
            ..Default::default()
        };
        assert_eq!(evaluate(&only_privileged, &r), Decision::Allow);

        let d = evaluate(&baseline_only(), &r);
        let rules: Vec<Rule> = d.violations().iter().map(Violation::rule).collect();
        assert_eq!(
            rules,
            [
                Rule::BaselineCapabilities,
                Rule::BaselineHostNamespace,
                Rule::BaselineUnconfined,
                Rule::BaselineHostPath,
                Rule::BaselineDevice,
            ]
        );
        assert!(d.is_denied());
    }

    #[test]
    fn baseline_admits_an_ordinary_container_and_its_allowed_capabilities() {
        let caps = s(&["NET_BIND_SERVICE", "cap_chown", "Setuid"]);
        let vols = s(&["data:/var/lib/app", "cache:/cache:ro"]);
        let h = ContainerHardening {
            cap_add: &caps,
            volumes: &vols,
            ..Default::default()
        };
        let r = Request::container("alpine:3.20", false, false).with_hardening(h);
        assert_eq!(evaluate(&baseline_only(), &r), Decision::Allow);
    }

    #[test]
    fn baseline_names_each_capability_beyond_its_set_once() {
        let caps = s(&["SYS_ADMIN", "cap_sys_admin", "NET_ADMIN", "CHOWN"]);
        assert_eq!(caps_beyond_baseline(&caps), ["SYS_ADMIN", "NET_ADMIN"]);
    }

    #[test]
    fn a_relative_bind_is_a_host_path_and_a_named_volume_is_not() {
        let vols = s(&["./src:/app", "/etc:/etc:ro", "pgdata:/var/lib/postgresql"]);
        assert_eq!(host_path_sources(&vols), ["./src", "/etc"]);
    }

    #[test]
    fn baseline_refuses_privileged_once_whether_or_not_deny_privileged_is_set() {
        let r = Request::container("alpine:3.20", true, false);
        let d = evaluate(&baseline_only(), &r);
        assert_eq!(d.violations(), [Violation::Privileged]);
        let both = SecurityPolicy {
            deny_privileged: true,
            ..baseline_only()
        };
        assert_eq!(evaluate(&both, &r).violations(), [Violation::Privileged]);
    }

    #[test]
    fn baseline_never_judges_a_vm() {
        let p = baseline_only();
        let r = Request::virtual_machine(Some("golden:1.0"), NO_DEVICES, None);
        assert_eq!(evaluate(&p, &r), Decision::Allow);
    }

    #[test]
    fn baseline_under_warn_lets_the_request_through_and_says_why() {
        let p = SecurityPolicy {
            mode: Mode::Warn,
            ..baseline_only()
        };
        let caps = s(&["SYS_PTRACE"]);
        let h = ContainerHardening {
            cap_add: &caps,
            ..Default::default()
        };
        let d = evaluate(
            &p,
            &Request::container("a:1", false, false).with_hardening(h),
        );
        assert!(matches!(d, Decision::AllowWithWarnings(ref v) if v.len() == 1));
    }
}

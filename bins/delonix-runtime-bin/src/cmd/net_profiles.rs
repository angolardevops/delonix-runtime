//! Selectable pod-network profiles for a `kubeadm` cluster — the pure half.
//!
//! Until this module existed, `spec.cni` took exactly two values
//! (`cmd::cluster::CNI_CHOICES` was `["default", "none"]`): `default` was a
//! node-local `bridge` conflist that only serves ONE node and is refused above
//! it (DX-6701), and `none` installed nothing and left every node `NotReady`.
//! So a multi-node kubeadm cluster could not be brought up by this engine at
//! all — honestly refused, but refused.
//!
//! This module adds the profiles and answers every question that can be
//! answered WITHOUT touching a host: which profile, which dataplane, which
//! kube-proxy mode, which MTU, which CIDRs, which reference plugins the
//! profile's chain actually execs, and which artefact (pinned by version AND
//! by content digest) installs it. Everything here is pure and unit-tested;
//! the SSH/kubectl half lives in `cmd::cluster` and the image half in
//! `cmd::vmimage`, so a refusal can happen before a single machine exists.
//!
//! **Three rules this module exists to enforce.**
//!
//! 1. *A profile is not "supported" because its manifest applies.* Each one
//!    carries a [`Support`] level with the reason, and an `Experimental` one is
//!    refused unless the manifest opts in by name. Cilium is the profile this
//!    engine WANTS, and it starts `Experimental` on purpose: ADR-0074's own
//!    "what was not measured" section says it in writing — «A real cluster CNI
//!    (Calico, Cilium) declaring the capability differently, and more than one
//!    node, were not measured». Shipping it as supported would be the
//!    "reports success, does nothing" failure this repo hunts.
//! 2. *No silent fallback.* A combination this engine cannot deliver is a
//!    refusal naming the field, never a quiet downgrade to another CNI. A
//!    cluster that asked for Cilium and got Flannel is worse than a cluster
//!    that was never created.
//! 3. *Versions are pinned, and the pin is the content.* A profile names a
//!    version AND the SHA-256 of the exact artefact; `latest` appears nowhere.
//!    The digests below were obtained and verified against the publishers'
//!    own `.sha256` sidecars on 2026-10-15 (see `docs/adr/0078-*`).

use delonix_model::{Error, Result};

/// How much of this profile this engine has actually proven.
///
/// Not cosmetic: [`Support::Experimental`] makes [`validate`] refuse unless
/// `spec.network.experimental: true` is set, so an operator cannot reach an
/// unproven dataplane by accident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Support {
    /// Artefact pinned and verified, install path implemented, and the
    /// profile's own integration tests have been run. See
    /// `docs/compatibilidade-rede.md` for which run, on what, and when.
    Supported,
    /// Implemented and artefact-verified, but NOT yet validated end to end
    /// against this engine's CRI in a multi-node topology. The `&'static str`
    /// is the reason, and it reaches the operator verbatim.
    Experimental(&'static str),
}

/// A pod-network profile. `Default`/`None` are the two historical values and
/// keep their exact meaning — this enum did not change what they do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    /// The engine's own: kindnet in `mode: kind`, a node-local `bridge`
    /// conflist on a ONE-node kubeadm cluster (refused above one node).
    Default,
    /// Install nothing. Every node stays `NotReady` until the operator
    /// applies their own CNI. Still a legitimate answer, and the only one
    /// for a CNI this engine does not ship.
    None,
    /// eBPF dataplane. The profile this engine wants; `Experimental` until
    /// measured against the Delonix CRI on more than one node.
    Cilium,
    /// The explicitly-chosen alternative.
    Calico,
    /// The simple lab/diagnosis profile. No NetworkPolicy enforcement of its
    /// own — see [`Profile::enforces_network_policy`].
    Flannel,
}

/// Every value `spec.cni`/`--cni` may take. Ordered as the help text reads.
pub(crate) const CNI_CHOICES: &[&str] = &["default", "none", "cilium", "calico", "flannel"];

impl Profile {
    /// Parses `spec.cni`/`--cni`. `None` for anything else — the caller turns
    /// that into DX-1701 so the unknown value is named in the refusal.
    pub(crate) fn parse(s: &str) -> Option<Profile> {
        match s {
            "default" => Some(Profile::Default),
            "none" => Some(Profile::None),
            "cilium" => Some(Profile::Cilium),
            "calico" => Some(Profile::Calico),
            "flannel" => Some(Profile::Flannel),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Profile::Default => "default",
            Profile::None => "none",
            Profile::Cilium => "cilium",
            Profile::Calico => "calico",
            Profile::Flannel => "flannel",
        }
    }

    /// Is this one of the profiles this module installs (as opposed to the two
    /// historical answers, which `cmd::cluster` handles on its own paths)?
    pub(crate) fn is_addon(self) -> bool {
        matches!(self, Profile::Cilium | Profile::Calico | Profile::Flannel)
    }

    pub(crate) fn support(self) -> Support {
        match self {
            Profile::Default | Profile::None => Support::Supported,
            // The reason is the evidence, quoted from where it is recorded.
            Profile::Cilium => Support::Experimental(
                "the Cilium dataplane has not been measured against this engine's CRI on more \
                 than one node (ADR-0074's own «what was not measured»); its agent needs a \
                 lifted capability ceiling, bidirectional mount propagation and bpffs, all of \
                 which this engine implements but none of which has been proven together",
            ),
            Profile::Calico => Support::Experimental(
                "the Calico dataplane has not been measured against this engine's CRI on more \
                 than one node",
            ),
            Profile::Flannel => Support::Experimental(
                "the Flannel dataplane has not been measured against this engine's CRI on more \
                 than one node",
            ),
        }
    }

    /// The pinned upstream version. Never a channel, never `latest`.
    pub(crate) fn version(self) -> &'static str {
        match self {
            Profile::Cilium => CILIUM_VERSION,
            Profile::Calico => CALICO_VERSION,
            Profile::Flannel => FLANNEL_VERSION,
            Profile::Default | Profile::None => "",
        }
    }

    /// The artefact that installs the profile, pinned by content.
    pub(crate) fn artefact(self) -> Option<Artefact> {
        match self {
            Profile::Cilium => Some(Artefact {
                kind: ArtefactKind::HelmChart,
                url: CILIUM_CHART_URL,
                sha256: CILIUM_CHART_SHA256,
                file_name: "cilium-1.20.2.tgz",
            }),
            Profile::Calico => Some(Artefact {
                kind: ArtefactKind::Manifest,
                url: CALICO_MANIFEST_URL,
                sha256: CALICO_MANIFEST_SHA256,
                file_name: "calico-v3.33.0.yaml",
            }),
            Profile::Flannel => Some(Artefact {
                kind: ArtefactKind::Manifest,
                url: FLANNEL_MANIFEST_URL,
                sha256: FLANNEL_MANIFEST_SHA256,
                file_name: "kube-flannel-v0.28.10.yml",
            }),
            Profile::Default | Profile::None => None,
        }
    }

    /// The dataplane (encapsulation / forwarding) modes this profile accepts,
    /// first one being the default.
    ///
    /// **Not a copy of upstream's full option list** — only the modes this
    /// engine renders and would be able to verify. `native`/`bgp` routing
    /// needs the cluster's surrounding network to carry pod routes, which this
    /// engine does not configure and cannot assume, so they are absent rather
    /// than accepted-and-broken.
    pub(crate) fn dataplanes(self) -> &'static [&'static str] {
        match self {
            // VXLAN first and deliberately: it is the mode that works without
            // asking anything of the network between the nodes, which is the
            // only assumption this engine is entitled to make.
            Profile::Cilium => &["vxlan", "geneve"],
            Profile::Calico => &["vxlan", "ipip"],
            Profile::Flannel => &["vxlan"],
            Profile::Default | Profile::None => &[],
        }
    }

    pub(crate) fn default_dataplane(self) -> &'static str {
        self.dataplanes().first().copied().unwrap_or("")
    }

    /// The kube-proxy modes this profile is wired for, first one the default.
    ///
    /// `none` means «kube-proxy is not installed; the CNI replaces it», which
    /// changes the `kubeadm init` call itself (`skipPhases: addon/kube-proxy`)
    /// and is therefore a cluster-creation-time decision, not a toggle.
    pub(crate) fn kube_proxy_modes(self) -> &'static [&'static str] {
        match self {
            // `none` is Cilium's kube-proxy replacement. Listed LAST so the
            // default stays the boring, measurable one.
            Profile::Cilium => &["iptables", "ipvs", "none"],
            Profile::Calico | Profile::Flannel => &["iptables", "ipvs"],
            Profile::Default | Profile::None => &["iptables"],
        }
    }

    /// Does the profile enforce `NetworkPolicy` itself?
    ///
    /// Asked and answered here because the alternative is a reader assuming
    /// that installing a CNI bought them isolation. Flannel does not: a
    /// `NetworkPolicy` applied on a Flannel cluster is accepted by the
    /// apiserver and enforced by nobody.
    pub(crate) fn enforces_network_policy(self) -> bool {
        matches!(self, Profile::Cilium | Profile::Calico)
    }

    /// The reference CNI plugin binaries this profile's chain actually execs.
    ///
    /// This is the list the golden image has to carry for the profile to work,
    /// and the reason the image does not simply chain everything it ships: a
    /// binary being present is not a reason for it to be in a chain.
    pub(crate) fn required_plugins(self) -> &'static [&'static str] {
        match self {
            // Cilium ships its own `cilium-cni` binary and chains `portmap`
            // for hostPort; `loopback` is what gives a pod `127.0.0.1`.
            Profile::Cilium => &["loopback", "portmap"],
            // Calico ships `calico`/`calico-ipam`; it chains `bandwidth`
            // (its own manifest declares the capability) and `portmap`.
            Profile::Calico => &["loopback", "portmap", "bandwidth"],
            // Flannel's conflist IS `bridge` + `host-local` + `portmap`.
            Profile::Flannel => &["loopback", "bridge", "host-local", "portmap"],
            // The engine's own single-node conflist.
            Profile::Default => &["loopback", "bridge", "host-local", "portmap"],
            Profile::None => &[],
        }
    }

    /// Kernel modules the profile's dataplane needs loaded on every node, on
    /// top of the `overlay`/`br_netfilter` the kubelet already requires.
    pub(crate) fn required_modules(self, dataplane: &str) -> Vec<&'static str> {
        let mut m: Vec<&'static str> = Vec::new();
        // `default`/`none` have no dataplane of their own. Asked with one
        // anyway (a caller passing a plan's field through), they must answer
        // for THEMSELVES, not for the string — otherwise a `cni: none` node
        // would be prepared for an encapsulation nobody installs.
        if !self.is_addon() {
            return m;
        }
        match dataplane {
            "vxlan" => m.push("vxlan"),
            "geneve" => m.push("geneve"),
            "ipip" => m.push("ipip"),
            _ => {}
        }
        if matches!(self, Profile::Calico | Profile::Flannel) {
            // Both program iptables/ipset directly.
            m.push("ip_tables");
            m.push("nf_conntrack");
        }
        m
    }
}

// ---------------------------------------------------------------------------
//  Pinned artefacts
// ---------------------------------------------------------------------------
//
// Every digest below was fetched from the publisher and checked against the
// publisher's own sidecar (`.sha256`) or recomputed locally on 2026-10-15.
// They are the CONTENT pin: a URL that starts answering different bytes fails
// the build/bootstrap closed instead of installing something else.
//
// What these pins do NOT cover, said plainly: the container images REFERENCED
// inside Calico's and Flannel's manifests are tag-pinned by upstream, not
// digest-pinned, so `docker.io/calico/node:v3.33.0` is as reproducible as
// Docker Hub chooses to make it. Cilium's chart is the better case — its
// values carry per-image digests. Resolving the other two to digests needs a
// registry round-trip per image and is a follow-up, recorded as such.

/// Cilium, pinned.
pub(crate) const CILIUM_VERSION: &str = "1.20.2";
const CILIUM_CHART_URL: &str = "https://helm.cilium.io/cilium-1.20.2.tgz";
const CILIUM_CHART_SHA256: &str =
    "b2afd87b7f75f875f92a14559f14f59b7babbb479d968e3fd625a20bf30ec20e";

/// Calico, pinned.
pub(crate) const CALICO_VERSION: &str = "3.33.0";
const CALICO_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/projectcalico/calico/v3.33.0/manifests/calico.yaml";
const CALICO_MANIFEST_SHA256: &str =
    "2de8f47595fb9c41b3f47d7b767a1f8e72ecf84057af834738ff12689a234da5";

/// Flannel, pinned.
pub(crate) const FLANNEL_VERSION: &str = "0.28.10";
const FLANNEL_MANIFEST_URL: &str =
    "https://github.com/flannel-io/flannel/releases/download/v0.28.10/kube-flannel.yml";
const FLANNEL_MANIFEST_SHA256: &str =
    "393bdbce53eb50c8eb97dd513b218518b7d6e7985dc7b889be956e112a711084";

/// Multus, pinned. An EXTENSION, never a primary CNI (see [`validate`]).
pub(crate) const MULTUS_VERSION: &str = "4.3.1";
const MULTUS_MANIFEST_URL: &str = "https://raw.githubusercontent.com/k8snetworkplumbingwg/\
     multus-cni/v4.3.1/deployments/multus-daemonset.yml";
const MULTUS_MANIFEST_SHA256: &str =
    "bd618d23a957729071ec81dcef740ffc03a5916102137afa6114d31a5d187843";

/// How an artefact is turned into applicable YAML.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArtefactKind {
    /// Already a manifest: substitute and `kubectl apply`.
    Manifest,
    /// A Helm chart tarball: `helm template` on the HOST (never a cluster-side
    /// Tiller-shaped thing, and never `helm install`, so there is no release
    /// state to diverge) and `kubectl apply` the output.
    HelmChart,
}

/// An install artefact pinned by version AND content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Artefact {
    pub kind: ArtefactKind,
    pub url: &'static str,
    pub sha256: &'static str,
    /// Cache file name under `<root>/net-profiles/`.
    pub file_name: &'static str,
}

/// One line naming every profile this binary can install and the version it
/// would install — for the golden image's own `/etc/delonix-image-release`.
///
/// Recorded IN the image because the alternative is asking a node which
/// profiles it supports and getting the answer from whatever `delonix` happens
/// to be on the machine asking. An image that says `cilium:1.20.2` and a
/// binary that would install 1.21 is a mismatch worth seeing, and this is what
/// makes it visible.
pub(crate) fn provenance() -> String {
    [Profile::Cilium, Profile::Calico, Profile::Flannel]
        .iter()
        .map(|p| format!("{}:{}", p.name(), p.version()))
        .chain(std::iter::once(format!("multus:{MULTUS_VERSION}")))
        .collect::<Vec<_>>()
        .join(",")
}

/// The Multus artefact, when the extension is asked for.
pub(crate) fn multus_artefact() -> Artefact {
    Artefact {
        kind: ArtefactKind::Manifest,
        url: MULTUS_MANIFEST_URL,
        sha256: MULTUS_MANIFEST_SHA256,
        file_name: "multus-daemonset-v4.3.1.yml",
    }
}

// ---------------------------------------------------------------------------
//  MTU
// ---------------------------------------------------------------------------

/// Bytes the encapsulation takes off the node's MTU.
///
/// Measured values, not guesses: VXLAN is 50 (14 inner Ethernet + 8 VXLAN + 8
/// UDP + 20 IPv4), Geneve the same 50 with no options, IP-in-IP 20. The
/// engine's own `docs/guia-rede.md` already warns about exactly this number
/// for its container overlay — the class of bug is "unexplained slowness, not
/// an error", which is why an MTU that does not fit is a refusal here.
pub(crate) fn encap_overhead(dataplane: &str) -> u32 {
    match dataplane {
        "vxlan" | "geneve" => 50,
        "ipip" => 20,
        _ => 0,
    }
}

/// The MTU a profile should run at on a node whose own MTU is `node_mtu`.
pub(crate) fn auto_mtu(dataplane: &str, node_mtu: u32) -> u32 {
    node_mtu.saturating_sub(encap_overhead(dataplane))
}

/// The smallest MTU worth allowing. 1280 is IPv6's minimum link MTU and the
/// floor every CNI in this table documents; below it, pods break in ways that
/// look like application bugs.
pub(crate) const MIN_MTU: u32 = 1280;
/// The largest. 9000 is the conventional jumbo frame; above it nothing in this
/// path has been measured.
pub(crate) const MAX_MTU: u32 = 9000;
/// The node MTU assumed when the manifest does not say. 1500 is Ethernet.
pub(crate) const DEFAULT_NODE_MTU: u32 = 1500;

// ---------------------------------------------------------------------------
//  CIDR — a real parser, because the old one was a charset check
// ---------------------------------------------------------------------------

/// An IPv4 CIDR, parsed.
///
/// `cmd::cluster::valid_cidr` only ever checked the CHARACTER SET (digits, `.`
/// and `/`), for a sound security reason — the value is interpolated into a
/// remote `bash -c`. So `"9"`, `"1.2.3.4"`, `"///"` and `"0.0.0.0/99"` all
/// passed as a valid `podSubnet`, and the failure arrived later, on a host,
/// from `kubeadm`. This type keeps that defence (the charset check stays) and
/// adds the meaning: a real address, a real prefix, and the arithmetic needed
/// to answer "do these two overlap".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cidr4 {
    /// Network address, host bits already cleared.
    pub network: u32,
    pub prefix: u8,
}

impl Cidr4 {
    /// Strict parse of `a.b.c.d/len`. No shorthand, no leading zeros, no
    /// host bits set outside the prefix being tolerated silently — the host
    /// bits are MASKED OFF and `masked()` reports whether they were there, so
    /// a caller can refuse `10.244.0.1/16` instead of quietly treating it as
    /// `10.244.0.0/16`.
    pub(crate) fn parse(s: &str) -> Option<Cidr4> {
        let (addr, len) = s.split_once('/')?;
        let prefix: u8 = parse_u8_strict(len)?;
        if prefix > 32 {
            return None;
        }
        let mut octets = [0u32; 4];
        let mut seen = 0usize;
        for (i, part) in addr.split('.').enumerate() {
            if i >= 4 {
                return None;
            }
            octets[i] = u32::from(parse_u8_strict(part)?);
            seen = i + 1;
        }
        if seen != 4 {
            return None;
        }
        let raw = (octets[0] << 24) | (octets[1] << 16) | (octets[2] << 8) | octets[3];
        Some(Cidr4 {
            network: raw & mask_of(prefix),
            prefix,
        })
    }

    /// Does this CIDR overlap `other`? Two prefixes overlap when the shorter
    /// one contains the other's network address.
    pub(crate) fn overlaps(self, other: Cidr4) -> bool {
        let short = self.prefix.min(other.prefix);
        let m = mask_of(short);
        (self.network & m) == (other.network & m)
    }

    /// How many addresses the prefix holds.
    pub(crate) fn size(self) -> u64 {
        1u64 << (32 - u32::from(self.prefix))
    }
}

/// `255.255.0.0` for `/16`. `/0` is all-zero, which `<<` would have made
/// undefined — hence the explicit arm rather than `!0 << (32 - p)`.
fn mask_of(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

/// A decimal `u8` with no sign, no spaces, no `+`, and no leading zero on a
/// multi-digit number (`010` is refused rather than read as 10 or as octal).
fn parse_u8_strict(s: &str) -> Option<u8> {
    if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse::<u8>().ok()
}

// ---------------------------------------------------------------------------
//  The resolved, validated plan
// ---------------------------------------------------------------------------

/// What the manifest/flags asked for, before validation. Strings because this
/// is the parse boundary; [`validate`] turns it into a [`NetworkPlan`].
#[derive(Clone, Debug, Default)]
pub(crate) struct NetworkRequest {
    pub cni: String,
    pub dataplane: Option<String>,
    pub kube_proxy: Option<String>,
    pub mtu: Option<u32>,
    pub node_mtu: Option<u32>,
    pub ip_family: Option<String>,
    pub interface: Option<String>,
    pub multus: bool,
    pub hubble: bool,
    pub experimental: bool,
    pub offline: bool,
    pub pod_subnet: String,
    pub service_subnet: String,
    /// `kind`, `vm` or `ssh`.
    pub mode: String,
    pub nodes: u32,
}

/// A validated plan. Every field is decided — nothing downstream re-decides a
/// default, which is how the two independent defaults for the Kubernetes
/// version managed to drift four minor releases apart once already
/// (`k8s_recipes::resolve_k8s_version`'s own recorded bug).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetworkPlan {
    pub profile: Profile,
    pub dataplane: &'static str,
    pub kube_proxy: &'static str,
    pub mtu: u32,
    pub pod_subnet: Cidr4,
    pub service_subnet: Cidr4,
    pub interface: Option<String>,
    pub multus: bool,
    pub hubble: bool,
    pub offline: bool,
}

impl NetworkPlan {
    /// Is the `addon/kube-proxy` phase skipped (CNI replaces kube-proxy)?
    ///
    /// Whether a generated `kubeadm --config` is needed is NOT asked here:
    /// external etcd needs one too, and this type cannot see the etcd mode.
    /// `kubeadm_config::needed` sees both and is the only authority — two
    /// functions answering one question is how they drift.
    pub(crate) fn skips_kube_proxy(&self) -> bool {
        self.kube_proxy == "none"
    }
}

/// Resolves a static string out of a profile's option list, so [`NetworkPlan`]
/// can hold `&'static str` and no copy of the table exists downstream.
fn pick<'a>(options: &'a [&'static str], asked: Option<&str>) -> Option<&'static str> {
    match asked {
        None => options.first().copied(),
        Some(a) => options.iter().copied().find(|o| *o == a),
    }
}

/// Everything that can be refused before a machine exists.
///
/// Ordered deliberately: the profile first (so an unknown name is never
/// reported as a bad dataplane), then the options the profile constrains, then
/// the addressing, then the extensions. Each refusal names the field and the
/// accepted values, because "invalid configuration" sends the reader to read
/// the whole manifest.
pub(crate) fn validate(req: &NetworkRequest) -> Result<NetworkPlan> {
    // --- 1. the profile itself -------------------------------------------
    let profile = Profile::parse(&req.cni).ok_or_else(|| {
        Error::coded(
            1701,
            Error::Invalid(super::po::tf(
                "spec.cni '{cni}' is not a pod network this engine installs — use one of: {list}",
                &[("cni", &req.cni), ("list", &CNI_CHOICES.join(", "))],
            )),
        )
    })?;

    // The historical refusal, unchanged: `default` on a kubeadm cluster is a
    // node-local bridge and serves exactly one node.
    if req.mode != "kind" && profile == Profile::Default && req.nodes > 1 {
        return Err(Error::coded(
            6701,
            Error::Unavailable(super::po::tf(
                "a kubeadm cluster of {n} nodes cannot use the default CNI: it is a node-local \
                 bridge with nothing that routes pod traffic between nodes, so every node would \
                 stay NotReady. Nothing was created. Choose a cross-node profile (`--cni cilium`, \
                 `--cni calico`, `--cni flannel`), or `--cni none` and apply your own, or create \
                 a one-node cluster (`--workers 0`)",
                &[("n", &req.nodes.to_string())],
            )),
        ));
    }

    // An add-on profile is a kubeadm-path capability. In `mode: kind` the
    // node image's own kindnet is what `default` means, and installing a
    // second CNI over it would leave two owners of the pod network.
    if req.mode == "kind" && profile.is_addon() {
        return Err(Error::coded(
            1702,
            Error::Invalid(super::po::tf(
                "spec.cni '{cni}' is not available in `mode: kind`: a kind-mode node image ships \
                 its own CNI (kindnet) and installing a second one would leave two owners of the \
                 pod network. Use `mode: vm` or `mode: ssh` for a {cni} cluster, or `cni: \
                 default` here",
                &[("cni", profile.name())],
            )),
        ));
    }

    // --- 2. support level -------------------------------------------------
    if let Support::Experimental(why) = profile.support() {
        if profile.is_addon() && !req.experimental {
            return Err(Error::coded(
                1705,
                Error::Invalid(super::po::tf(
                    "spec.cni '{cni}' is EXPERIMENTAL in this release and is not enabled by \
                     default: {why}. Nothing was created. To use it anyway, say so explicitly \
                     with `spec.network.experimental: true` (or `--experimental-cni`), and read \
                     `docs/compatibilidade-rede.md` for what has and has not been measured",
                    &[("cni", profile.name()), ("why", why)],
                )),
            ));
        }
    }

    // --- 3. options the profile constrains --------------------------------
    // A knob set on a profile that has no dataplane at all would be dropped
    // in silence, which is the whole class of bug this module exists for.
    if !profile.is_addon() {
        for (field, set) in [
            ("dataplane", req.dataplane.is_some()),
            ("kubeProxy", req.kube_proxy.is_some()),
            ("mtu", req.mtu.is_some()),
            ("interface", req.interface.is_some()),
        ] {
            if set {
                return Err(Error::coded(
                    1702,
                    Error::Invalid(super::po::tf(
                        "spec.network.{field} was set with `cni: {cni}`, which installs no pod \
                         network add-on, so the value would be ignored. Remove the field, or \
                         choose a profile that reads it (cilium, calico, flannel)",
                        &[("field", field), ("cni", profile.name())],
                    )),
                ));
            }
        }
        if req.multus || req.hubble {
            return Err(Error::coded(
                1702,
                Error::Invalid(super::po::tf(
                    "spec.network.multus/hubble need a primary pod network to extend, and \
                     `cni: {cni}` installs none",
                    &[("cni", profile.name())],
                )),
            ));
        }
    }

    // `default`/`none` have no encapsulation of their own, and any value the
    // manifest set for one was already refused above — so the empty option
    // list resolves to the empty string rather than to "no valid mode".
    let dataplane = if profile.dataplanes().is_empty() {
        ""
    } else {
        pick(profile.dataplanes(), req.dataplane.as_deref()).ok_or_else(|| {
            Error::coded(
            1702,
            Error::Invalid(super::po::tf(
                "spec.network.dataplane '{asked}' is not a mode this engine renders for {cni} — \
                 use one of: {list}. Routed modes that need the surrounding network to carry pod \
                 routes (`native`, BGP) are deliberately absent: this engine does not configure \
                 that network and will not accept a setting it cannot deliver",
                &[
                    ("asked", req.dataplane.as_deref().unwrap_or("")),
                    ("cni", profile.name()),
                    ("list", &profile.dataplanes().join(", ")),
                ],
            )),
        )
        })?
    };

    let kube_proxy =
        pick(profile.kube_proxy_modes(), req.kube_proxy.as_deref()).ok_or_else(|| {
            Error::coded(
                1702,
                Error::Invalid(super::po::tf(
                    "spec.network.kubeProxy '{asked}' is not supported with {cni} — use one of: \
                 {list}. `none` means the CNI replaces kube-proxy entirely, which only Cilium \
                 does on this path and which changes the `kubeadm init` call itself, so it \
                 cannot be turned on after the cluster exists",
                    &[
                        ("asked", req.kube_proxy.as_deref().unwrap_or("")),
                        ("cni", profile.name()),
                        ("list", &profile.kube_proxy_modes().join(", ")),
                    ],
                )),
            )
        })?;

    // --- 4. address family ------------------------------------------------
    // Said as "not implemented", never as a limit of the engine: ADR-0065
    // (IPv6 dataplane) is Accepted and the work is real, it just has not
    // reached this path. A message that reads as permanent teaches the next
    // reader to stop looking.
    match req.ip_family.as_deref() {
        None | Some("ipv4") => {}
        Some(other) => {
            return Err(Error::coded(
                1706,
                Error::Invalid(super::po::tf(
                    "spec.network.ipFamily '{asked}' is not implemented on the cluster path yet: \
                     this bootstrap renders IPv4 pod and service CIDRs only, and the cluster \
                     CIDR validation below is IPv4 arithmetic. ADR-0065 carries the IPv6 \
                     dataplane; until it reaches here, a dual-stack cluster has to be bootstrapped \
                     with `cni: none` and your own CNI",
                    &[("asked", other)],
                )),
            ));
        }
    }

    // --- 5. addressing ----------------------------------------------------
    let pod_subnet = parse_cidr_field("spec.podSubnet", &req.pod_subnet)?;
    let service_subnet = parse_cidr_field("spec.serviceSubnet", &req.service_subnet)?;
    if pod_subnet.overlaps(service_subnet) {
        return Err(Error::coded(
            1704,
            Error::Invalid(super::po::tf(
                "spec.podSubnet {pods} and spec.serviceSubnet {svcs} overlap. kubeadm accepts \
                 both and the cluster then routes one of them nowhere: a Service address that \
                 falls inside the pod CIDR is answered by whatever pod holds it. Nothing was \
                 created",
                &[("pods", &req.pod_subnet), ("svcs", &req.service_subnet)],
            )),
        ));
    }
    // A pod CIDR has to hold more than the nodes that will carve it up. The
    // kube-controller-manager hands each node a /24 by default, so a /24 pod
    // CIDR on two nodes gives the second node nothing and its pods never get
    // an address — a failure that shows up as "pods Pending", far from here.
    let per_node = 256u64;
    if profile.is_addon() && pod_subnet.size() < per_node * u64::from(req.nodes.max(1)) {
        return Err(Error::coded(
            1704,
            Error::Invalid(super::po::tf(
                "spec.podSubnet {pods} holds {have} addresses, and a {n}-node cluster needs at \
                 least {need}: the controller-manager carves a /24 per node, so the later nodes \
                 would get no pod range at all and their pods would stay Pending. Widen the pod \
                 CIDR",
                &[
                    ("pods", &req.pod_subnet),
                    ("have", &pod_subnet.size().to_string()),
                    ("n", &req.nodes.max(1).to_string()),
                    (
                        "need",
                        &(per_node * u64::from(req.nodes.max(1))).to_string(),
                    ),
                ],
            )),
        ));
    }

    // --- 6. MTU -----------------------------------------------------------
    let node_mtu = req.node_mtu.unwrap_or(DEFAULT_NODE_MTU);
    let overhead = encap_overhead(dataplane);
    let mtu = match req.mtu {
        None => auto_mtu(dataplane, node_mtu),
        Some(asked) => {
            if asked < MIN_MTU || asked > MAX_MTU {
                return Err(Error::coded(
                    1703,
                    Error::Invalid(super::po::tf(
                        "spec.network.mtu {asked} is outside the range this path accepts \
                         ({min}-{max})",
                        &[
                            ("asked", &asked.to_string()),
                            ("min", &MIN_MTU.to_string()),
                            ("max", &MAX_MTU.to_string()),
                        ],
                    )),
                ));
            }
            if asked + overhead > node_mtu {
                return Err(Error::coded(
                    1703,
                    Error::Invalid(super::po::tf(
                        "spec.network.mtu {asked} does not fit {dp} encapsulation on a node MTU \
                         of {node}: {dp} takes {over} bytes, so the pod MTU cannot exceed {max}. \
                         An MTU that does not fit does not fail — it silently drops the large \
                         packets, which reads as unexplained slowness and not as an error, so it \
                         is refused here instead",
                        &[
                            ("asked", &asked.to_string()),
                            ("dp", dataplane),
                            ("node", &node_mtu.to_string()),
                            ("over", &overhead.to_string()),
                            ("max", &node_mtu.saturating_sub(overhead).to_string()),
                        ],
                    )),
                ));
            }
            asked
        }
    };
    if mtu < MIN_MTU {
        return Err(Error::coded(
            1703,
            Error::Invalid(super::po::tf(
                "a node MTU of {node} leaves {mtu} for pods after {dp} encapsulation ({over} \
                 bytes), below the {min} floor every CNI in this table documents. Raise the node \
                 MTU or set `spec.network.nodeMtu` to the real one",
                &[
                    ("node", &node_mtu.to_string()),
                    ("mtu", &mtu.to_string()),
                    ("dp", dataplane),
                    ("over", &overhead.to_string()),
                    ("min", &MIN_MTU.to_string()),
                ],
            )),
        ));
    }

    // --- 7. extensions ----------------------------------------------------
    if req.hubble && profile != Profile::Cilium {
        return Err(Error::coded(
            1702,
            Error::Invalid(super::po::tf(
                "spec.network.hubble is a Cilium component and `cni: {cni}` is not Cilium",
                &[("cni", profile.name())],
            )),
        ));
    }
    if req.multus && kube_proxy == "none" {
        // Not a theoretical conflict: Multus's own DaemonSet reaches the
        // apiserver through a Service, and with kube-proxy replaced the
        // ordering between the replacement coming up and Multus starting has
        // not been measured here. Refused rather than raced.
        return Err(Error::coded(
            1702,
            Error::Invalid(
                super::po::t(
                    "spec.network.multus together with `kubeProxy: none` has not been measured on \
                 this path: Multus reaches the apiserver through a Service, and the ordering \
                 against a kube-proxy replacement coming up is exactly what was not tested. \
                 Use `kubeProxy: iptables` with Multus, or drop Multus",
                )
                .to_string(),
            ),
        ));
    }

    // An interface name reaches a rendered manifest and, on some profiles, a
    // node-side command line. Same charset discipline as `valid_endpoint`.
    if let Some(dev) = &req.interface {
        if dev.is_empty()
            || dev.len() > 15
            || !dev
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err(Error::coded(
                1702,
                Error::Invalid(super::po::tf(
                    "spec.network.interface '{dev}' is not an interface name (up to 15 \
                     characters of letters, digits, '.', '-' and '_' — the kernel's own limit)",
                    &[("dev", dev)],
                )),
            ));
        }
    }

    Ok(NetworkPlan {
        profile,
        dataplane,
        kube_proxy,
        mtu,
        pod_subnet,
        service_subnet,
        interface: req.interface.clone(),
        multus: req.multus,
        hubble: req.hubble,
        offline: req.offline,
    })
}

/// Parses a CIDR field, keeping the historical charset refusal as the FIRST
/// gate (it is the shell-injection defence, and it must not become weaker
/// because the parse got stronger) and reporting host bits rather than
/// masking them away in silence.
fn parse_cidr_field(field: &str, value: &str) -> Result<Cidr4> {
    let charset_ok = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | '/'));
    let parsed = if charset_ok {
        Cidr4::parse(value)
    } else {
        None
    };
    let cidr = parsed.ok_or_else(|| {
        Error::coded(
            1704,
            Error::Invalid(super::po::tf(
                "{field} '{value}' is not an IPv4 CIDR (expected a.b.c.d/len, len 0-32)",
                &[("field", field), ("value", value)],
            )),
        )
    })?;
    // `10.244.0.1/16` is almost always a typo for `10.244.0.0/16`, and
    // accepting it after masking means the manifest and the cluster disagree
    // about what was asked.
    let reparsed = format!(
        "{}.{}.{}.{}/{}",
        cidr.network >> 24,
        (cidr.network >> 16) & 0xff,
        (cidr.network >> 8) & 0xff,
        cidr.network & 0xff,
        cidr.prefix
    );
    if reparsed != value {
        return Err(Error::coded(
            1704,
            Error::Invalid(super::po::tf(
                "{field} '{value}' has bits set outside its prefix — did you mean '{fixed}'? It \
                 is refused rather than masked, so the manifest and the cluster cannot disagree \
                 about what was asked",
                &[("field", field), ("value", value), ("fixed", &reparsed)],
            )),
        ));
    }
    Ok(cidr)
}

/// Refuses a pod/service CIDR that overlaps a network the environment already
/// uses, by name.
///
/// `known` is `(label, cidr)` — the engine's own declared networks and the
/// address of each node, which is everything this process can see without
/// asking the operator. An overlap here is the failure that looks like
/// "the cluster works but one host is unreachable from pods", and it is
/// cheap to refuse and expensive to debug.
pub(crate) fn check_env_overlap(plan: &NetworkPlan, known: &[(String, String)]) -> Result<()> {
    for (label, cidr) in known {
        let Some(other) = Cidr4::parse(cidr) else {
            continue;
        };
        for (field, ours) in [
            ("spec.podSubnet", plan.pod_subnet),
            ("spec.serviceSubnet", plan.service_subnet),
        ] {
            if ours.overlaps(other) {
                return Err(Error::coded(
                    1704,
                    Error::Invalid(super::po::tf(
                        "{field} overlaps {label} ({cidr}), a network this host already uses. \
                         Pods would be given addresses that also belong to something else, and \
                         the symptom is one host or one network becoming unreachable from inside \
                         the cluster — not an error at creation time. Nothing was created; pick \
                         a range outside it",
                        &[("field", field), ("label", label), ("cidr", cidr)],
                    )),
                ));
            }
        }
    }
    Ok(())
}

/// Refuses changing the pod network of a cluster that already has one.
///
/// There is no supported migration: two CNIs own the same pod CIDR, the same
/// `/etc/cni/net.d` and the same iptables/eBPF state, and the engine does not
/// drain, re-IP and re-attach every pod. `installed` is what the cluster
/// reports; `asked` is the new manifest.
pub(crate) fn check_profile_change(installed: &str, asked: Profile) -> Result<()> {
    if installed.is_empty() || installed == asked.name() {
        return Ok(());
    }
    Err(Error::coded(
        6703,
        Error::Unavailable(super::po::tf(
            "cluster already runs the '{installed}' pod network and the manifest now asks for \
             '{asked}'. Changing the CNI of a live cluster is not a supported procedure here: the \
             two own the same pod CIDR, the same /etc/cni/net.d and the same \
             iptables/eBPF state, and nothing in this engine drains, re-addresses and re-attaches \
             every pod. Nothing was changed — the cluster is untouched and still running \
             '{installed}'. To move, create a new cluster with '{asked}' and migrate the \
             workloads",
            &[("installed", installed), ("asked", asked.name())],
        )),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(cni: &str) -> NetworkRequest {
        NetworkRequest {
            cni: cni.to_string(),
            pod_subnet: "10.244.0.0/16".into(),
            service_subnet: "10.96.0.0/12".into(),
            mode: "ssh".into(),
            nodes: 3,
            experimental: true,
            ..Default::default()
        }
    }

    // --- the table itself -------------------------------------------------

    /// Every choice the CLI advertises parses, and every profile the enum has
    /// is advertised. A value accepted by one and not the other is how a flag
    /// starts refusing what the manifest accepts.
    #[test]
    fn the_choices_and_the_enum_are_the_same_set() {
        for c in CNI_CHOICES {
            assert!(Profile::parse(c).is_some(), "{c} is advertised but unknown");
        }
        for p in [
            Profile::Default,
            Profile::None,
            Profile::Cilium,
            Profile::Calico,
            Profile::Flannel,
        ] {
            assert!(
                CNI_CHOICES.contains(&p.name()),
                "{} exists but is not advertised",
                p.name()
            );
        }
        assert_eq!(CNI_CHOICES.len(), 5);
    }

    /// An add-on profile without a pinned artefact would install nothing and
    /// report success — the exact failure this module replaces.
    #[test]
    fn every_addon_profile_has_a_pinned_artefact_and_a_dataplane() {
        for p in [Profile::Cilium, Profile::Calico, Profile::Flannel] {
            let a = p
                .artefact()
                .unwrap_or_else(|| panic!("{} has no artefact", p.name()));
            assert_eq!(a.sha256.len(), 64, "{}: digest is not a sha256", p.name());
            assert!(
                a.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "{}: digest is not hex",
                p.name()
            );
            assert!(a.url.starts_with("https://"), "{}: {}", p.name(), a.url);
            // `latest` in a reproducible path is the bug, not a convenience.
            assert!(!a.url.contains("latest"), "{}: {}", p.name(), a.url);
            assert!(!p.version().is_empty());
            assert!(
                a.url.contains(p.version()),
                "{}: url {} does not carry the pinned version {}",
                p.name(),
                a.url,
                p.version()
            );
            assert!(!p.dataplanes().is_empty());
            assert!(p.dataplanes().contains(&p.default_dataplane()));
            // Every profile needs `loopback`: a pod expects 127.0.0.1.
            assert!(p.required_plugins().contains(&"loopback"), "{}", p.name());
        }
        let m = multus_artefact();
        assert_eq!(m.sha256.len(), 64);
        assert!(m.url.contains(MULTUS_VERSION));
    }

    /// Flannel must never be reported as giving NetworkPolicy enforcement.
    #[test]
    fn only_the_profiles_that_enforce_policy_say_they_do() {
        assert!(Profile::Cilium.enforces_network_policy());
        assert!(Profile::Calico.enforces_network_policy());
        assert!(!Profile::Flannel.enforces_network_policy());
        assert!(!Profile::Default.enforces_network_policy());
        assert!(!Profile::None.enforces_network_policy());
    }

    // --- support level ----------------------------------------------------

    /// An experimental profile is refused without the explicit opt-in, and
    /// the refusal carries the REASON, not just the word.
    #[test]
    fn an_experimental_profile_needs_an_explicit_opt_in() {
        let mut r = req("cilium");
        r.experimental = false;
        let e = validate(&r).unwrap_err();
        assert_eq!(e.number(), 1705, "{e}");
        assert!(e.to_string().contains("more than one node"), "{e}");
        assert!(e.to_string().contains("experimental: true"), "{e}");
        // With the opt-in it resolves.
        r.experimental = true;
        assert_eq!(validate(&r).unwrap().profile, Profile::Cilium);
        // `default`/`none` are not gated by it.
        let mut d = req("none");
        d.experimental = false;
        assert!(validate(&d).is_ok());
    }

    // --- the historical refusals still hold -------------------------------

    #[test]
    fn an_unknown_cni_is_refused_by_name_and_lists_the_real_ones() {
        let e = validate(&req("kindnet")).unwrap_err();
        assert_eq!(e.number(), 1701, "{e}");
        assert!(e.to_string().contains("kindnet"), "{e}");
        assert!(e.to_string().contains("cilium"), "{e}");
    }

    #[test]
    fn the_default_cni_still_serves_only_one_kubeadm_node() {
        let e = validate(&req("default")).unwrap_err();
        assert_eq!(e.number(), 6701, "{e}");
        // One node is fine.
        let mut one = req("default");
        one.nodes = 1;
        assert_eq!(validate(&one).unwrap().profile, Profile::Default);
        // And kind mode was never refused.
        let mut k = req("default");
        k.mode = "kind".into();
        assert!(validate(&k).is_ok());
    }

    /// Kind mode already has an owner of the pod network.
    #[test]
    fn an_addon_profile_is_refused_in_kind_mode() {
        let mut r = req("cilium");
        r.mode = "kind".into();
        let e = validate(&r).unwrap_err();
        assert_eq!(e.number(), 1702, "{e}");
        assert!(e.to_string().contains("kindnet"), "{e}");
    }

    // --- options ----------------------------------------------------------

    /// A knob that would be silently dropped is a refusal, for every knob.
    #[test]
    fn a_knob_set_on_a_profile_that_ignores_it_is_refused() {
        for (label, mutate) in [
            (
                "dataplane",
                (|r: &mut NetworkRequest| r.dataplane = Some("vxlan".into())) as fn(&mut _),
            ),
            ("kubeProxy", |r: &mut NetworkRequest| {
                r.kube_proxy = Some("ipvs".into())
            }),
            ("mtu", |r: &mut NetworkRequest| r.mtu = Some(1400)),
            ("interface", |r: &mut NetworkRequest| {
                r.interface = Some("eth0".into())
            }),
            ("multus", |r: &mut NetworkRequest| r.multus = true),
        ] {
            let mut r = req("none");
            mutate(&mut r);
            let e = validate(&r).unwrap_err();
            assert_eq!(e.number(), 1702, "{label}: {e}");
        }
    }

    #[test]
    fn a_dataplane_the_profile_does_not_render_is_refused_with_the_list() {
        let mut r = req("flannel");
        r.dataplane = Some("ipip".into());
        let e = validate(&r).unwrap_err();
        assert_eq!(e.number(), 1702, "{e}");
        assert!(e.to_string().contains("vxlan"), "{e}");
        // Calico does render ipip.
        let mut c = req("calico");
        c.dataplane = Some("ipip".into());
        assert_eq!(validate(&c).unwrap().dataplane, "ipip");
        // And a routed mode is absent on purpose, with the reason.
        let mut n = req("cilium");
        n.dataplane = Some("native".into());
        let e = validate(&n).unwrap_err();
        assert!(e.to_string().contains("does not configure"), "{e}");
    }

    /// kube-proxy replacement is Cilium-only, and it is the plan that says so
    /// — the `kubeadm --config` decision is `kubeadm_config::needed`'s, which
    /// also sees the etcd mode (and has its own tests).
    #[test]
    fn kube_proxy_replacement_is_cilium_only() {
        let mut c = req("cilium");
        c.kube_proxy = Some("none".into());
        assert!(validate(&c).unwrap().skips_kube_proxy());

        let mut f = req("flannel");
        f.kube_proxy = Some("none".into());
        let e = validate(&f).unwrap_err();
        assert_eq!(e.number(), 1702, "{e}");

        // The default stays the boring one.
        let plan = validate(&req("cilium")).unwrap();
        assert_eq!(plan.kube_proxy, "iptables");
        assert!(!plan.skips_kube_proxy());
        // `ipvs` keeps kube-proxy, it only changes its mode.
        let mut i = req("calico");
        i.kube_proxy = Some("ipvs".into());
        let p = validate(&i).unwrap();
        assert_eq!(p.kube_proxy, "ipvs");
        assert!(!p.skips_kube_proxy());
    }

    #[test]
    fn hubble_without_cilium_and_multus_without_a_primary_are_refused() {
        let mut r = req("calico");
        r.hubble = true;
        assert_eq!(validate(&r).unwrap_err().number(), 1702);
        let mut m = req("cilium");
        m.multus = true;
        m.kube_proxy = Some("none".into());
        let e = validate(&m).unwrap_err();
        assert!(e.to_string().contains("not been measured"), "{e}");
        // Multus on a normal profile is fine.
        let mut ok = req("cilium");
        ok.multus = true;
        assert!(validate(&ok).unwrap().multus);
    }

    #[test]
    fn an_interface_name_outside_the_kernels_own_limit_is_refused() {
        for bad in ["", "eth0;reboot", "a".repeat(16).as_str(), "eth 0", "$(id)"] {
            let mut r = req("cilium");
            r.interface = Some(bad.to_string());
            assert_eq!(validate(&r).unwrap_err().number(), 1702, "{bad:?}");
        }
        let mut ok = req("cilium");
        ok.interface = Some("ens3.100".into());
        assert_eq!(
            validate(&ok).unwrap().interface.as_deref(),
            Some("ens3.100")
        );
    }

    // --- address family ---------------------------------------------------

    /// The refusal says «not implemented» and names where the work lives —
    /// never «impossible» (the engine's own identity rule).
    #[test]
    fn a_non_ipv4_family_is_refused_as_not_implemented() {
        for f in ["ipv6", "dual"] {
            let mut r = req("cilium");
            r.ip_family = Some(f.to_string());
            let e = validate(&r).unwrap_err();
            assert_eq!(e.number(), 1706, "{e}");
            assert!(e.to_string().contains("not implemented"), "{e}");
            assert!(e.to_string().contains("ADR-0065"), "{e}");
        }
        let mut v4 = req("cilium");
        v4.ip_family = Some("ipv4".into());
        assert!(validate(&v4).is_ok());
    }

    // --- CIDR -------------------------------------------------------------

    /// The old charset check accepted all of these as a pod CIDR.
    #[test]
    fn the_cidr_parser_refuses_what_the_charset_check_accepted() {
        for bad in [
            "9",
            "1.2.3.4",
            "///",
            "0.0.0.0/99",
            "10.244.0.0/33",
            "10.244.0/16",
            "10.244.0.0.0/16",
            "256.0.0.0/8",
            "010.0.0.0/8",
            "10.0.0.0/016",
            "",
        ] {
            let mut r = req("cilium");
            r.pod_subnet = bad.to_string();
            assert!(validate(&r).is_err(), "{bad:?} was accepted");
        }
        assert_eq!(
            Cidr4::parse("10.244.0.0/16"),
            Some(Cidr4 {
                network: 0x0AF4_0000,
                prefix: 16
            })
        );
        assert_eq!(Cidr4::parse("0.0.0.0/0").unwrap().size(), 1 << 32);
    }

    /// Host bits are reported, not masked: the manifest and the cluster must
    /// not disagree about what was asked.
    #[test]
    fn host_bits_outside_the_prefix_are_refused_with_the_fix() {
        let mut r = req("cilium");
        r.pod_subnet = "10.244.0.1/16".into();
        let e = validate(&r).unwrap_err();
        assert!(e.to_string().contains("10.244.0.0/16"), "{e}");
        // The CODE matters as much as the text: DX-1704's own dictionary entry
        // documents host bits, so a generic DX-1000 here would send a reader
        // looking up a code whose meaning says nothing about this.
        assert_eq!(e.number(), 1704, "{e}");
    }

    /// Every CIDR refusal on this path carries DX-1704, for the same reason.
    #[test]
    fn a_malformed_cidr_also_carries_the_cidr_code() {
        for bad in ["9", "1.2.3.4", "10.244.0.0/33", ""] {
            let mut r = req("cilium");
            r.pod_subnet = bad.to_string();
            assert_eq!(validate(&r).unwrap_err().number(), 1704, "{bad:?}");
        }
    }

    #[test]
    fn pod_and_service_cidrs_that_overlap_are_refused() {
        let mut r = req("cilium");
        r.pod_subnet = "10.0.0.0/8".into();
        r.service_subnet = "10.96.0.0/12".into();
        let e = validate(&r).unwrap_err();
        assert_eq!(e.number(), 1704, "{e}");
        // Overlap is symmetric regardless of which is shorter.
        assert!(Cidr4::parse("10.96.0.0/12")
            .unwrap()
            .overlaps(Cidr4::parse("10.0.0.0/8").unwrap()));
        assert!(!Cidr4::parse("10.244.0.0/16")
            .unwrap()
            .overlaps(Cidr4::parse("10.96.0.0/12").unwrap()));
    }

    /// A pod CIDR too small for the node count fails as «pods Pending», far
    /// from here. Refused at the manifest instead.
    #[test]
    fn a_pod_cidr_too_small_for_the_nodes_is_refused() {
        let mut r = req("flannel");
        r.pod_subnet = "10.244.0.0/24".into();
        r.nodes = 3;
        let e = validate(&r).unwrap_err();
        assert_eq!(e.number(), 1704, "{e}");
        assert!(e.to_string().contains("/24 per node"), "{e}");
        // One node fits in a /24.
        r.nodes = 1;
        assert!(validate(&r).is_ok());
    }

    #[test]
    fn an_environment_overlap_is_named() {
        let plan = validate(&req("cilium")).unwrap();
        let known = vec![
            (
                "network \"lab-net\"".to_string(),
                "10.244.0.0/16".to_string(),
            ),
            ("unparseable".to_string(), "not-a-cidr".to_string()),
        ];
        let e = check_env_overlap(&plan, &known).unwrap_err();
        assert_eq!(e.number(), 1704, "{e}");
        assert!(e.to_string().contains("lab-net"), "{e}");
        // No overlap, no refusal — and an unparseable entry is skipped, not
        // treated as a match.
        let fine = vec![("other".to_string(), "192.168.50.0/24".to_string())];
        assert!(check_env_overlap(&plan, &fine).is_ok());
    }

    // --- MTU --------------------------------------------------------------

    #[test]
    fn the_automatic_mtu_leaves_room_for_the_encapsulation() {
        assert_eq!(encap_overhead("vxlan"), 50);
        assert_eq!(encap_overhead("geneve"), 50);
        assert_eq!(encap_overhead("ipip"), 20);
        assert_eq!(encap_overhead("whatever"), 0);
        assert_eq!(validate(&req("cilium")).unwrap().mtu, 1450);
        let mut c = req("calico");
        c.dataplane = Some("ipip".into());
        assert_eq!(validate(&c).unwrap().mtu, 1480);
        // A jumbo node MTU is carried through.
        let mut j = req("cilium");
        j.node_mtu = Some(9000);
        assert_eq!(validate(&j).unwrap().mtu, 8950);
    }

    /// An MTU that does not fit the encapsulation drops large packets in
    /// silence. It is refused, and the refusal says the ceiling.
    #[test]
    fn an_mtu_that_does_not_fit_the_encapsulation_is_refused() {
        let mut r = req("cilium");
        r.mtu = Some(1500);
        let e = validate(&r).unwrap_err();
        assert_eq!(e.number(), 1703, "{e}");
        assert!(e.to_string().contains("1450"), "{e}");
        // Exactly at the ceiling is accepted.
        r.mtu = Some(1450);
        assert_eq!(validate(&r).unwrap().mtu, 1450);
        // Out of range either way.
        for bad in [1279, 9001] {
            let mut b = req("cilium");
            b.node_mtu = Some(65_000);
            b.mtu = Some(bad);
            assert_eq!(validate(&b).unwrap_err().number(), 1703, "{bad}");
        }
    }

    /// A node MTU so small that nothing legal is left is a refusal that names
    /// the node MTU — not a pod network that comes up and loses packets.
    #[test]
    fn a_node_mtu_too_small_for_any_pod_mtu_is_refused() {
        let mut r = req("cilium");
        r.node_mtu = Some(1300);
        let e = validate(&r).unwrap_err();
        assert_eq!(e.number(), 1703, "{e}");
        assert!(e.to_string().contains("1300"), "{e}");
    }

    // --- profile change ---------------------------------------------------

    #[test]
    fn changing_the_cni_of_a_live_cluster_is_refused_and_changes_nothing() {
        let e = check_profile_change("calico", Profile::Cilium).unwrap_err();
        assert_eq!(e.number(), 6703, "{e}");
        assert!(e.to_string().contains("untouched"), "{e}");
        // Same profile, or a cluster with none recorded, is a no-op.
        assert!(check_profile_change("cilium", Profile::Cilium).is_ok());
        assert!(check_profile_change("", Profile::Cilium).is_ok());
    }

    // --- modules ----------------------------------------------------------

    #[test]
    fn the_required_modules_follow_the_dataplane() {
        assert!(Profile::Cilium.required_modules("vxlan").contains(&"vxlan"));
        assert!(Profile::Cilium
            .required_modules("geneve")
            .contains(&"geneve"));
        assert!(Profile::Calico.required_modules("ipip").contains(&"ipip"));
        assert!(Profile::Calico
            .required_modules("vxlan")
            .contains(&"nf_conntrack"));
        // Cilium's eBPF dataplane does not need the iptables modules.
        assert!(!Profile::Cilium
            .required_modules("vxlan")
            .contains(&"ip_tables"));
        assert!(Profile::None.required_modules("vxlan").is_empty());
    }
}

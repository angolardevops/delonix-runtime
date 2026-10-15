//! Selectable pod-network profiles — the half that touches things.
//!
//! [`super::net_profiles`] decides; this module obtains the artefact, renders
//! it, applies it to the cluster, waits for it to be genuinely up, records
//! what was installed and answers what is installed now. The split is not
//! cosmetic: everything that can refuse a bad cluster refuses it over there,
//! with unit tests and no host, and everything here runs only after that
//! refusal could not happen.
//!
//! **What "installed" means here.** Not that `kubectl apply` exited 0 — that
//! only means the apiserver accepted the objects. A profile is installed when
//! its own agent is `Ready` on every node AND every node reports `Ready`,
//! observed by polling, with a bounded deadline and the observed state
//! recorded. `kubectl apply` returning 0 while nothing schedules is the exact
//! shape of the gap this work closes (`docs/discovery/kaas-matrix-2026-10-08.md`
//! gap #1), so it is not allowed to be the success condition.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use delonix_model::{Error, Result};
use serde::{Deserialize, Serialize};

use super::net_profiles::{Artefact, ArtefactKind, NetworkPlan, Profile};
use super::remote::{self, SshTarget};
use super::util::state_root;
use super::vmimage::{hex_sha256_file, stream_download};

/// Where verified artefacts are cached, one file per pinned artefact.
///
/// Under the state root rather than a temp dir so an `--offline` run on a
/// machine with no reach can be served from a cache someone else populated,
/// and so a second `cluster apply` does not re-download what it verified.
pub(crate) fn cache_dir() -> PathBuf {
    state_root().join("net-profiles")
}

/// Obtains an artefact and returns its path, having PROVEN its content.
///
/// Three outcomes, no fourth: the cached file whose digest matches, a fresh
/// download whose digest matches, or an error. A cached file whose digest does
/// NOT match is deleted and re-fetched (it is a truncated download, not a
/// decision); a fresh download whose digest does not match is deleted and
/// reported, because at that point this release's pin and the published
/// artefact genuinely disagree and guessing which is right is not this code's
/// job.
pub(crate) fn fetch_artefact(a: &Artefact, offline: bool) -> Result<PathBuf> {
    let dir = cache_dir();
    let dest = dir.join(a.file_name);
    if dest.exists() {
        match hex_sha256_file(&dest) {
            Ok(got) if got == a.sha256 => return Ok(dest),
            // A cached file that does not match is not evidence of anything —
            // most often an interrupted download. Drop it and try again,
            // unless we are not allowed to.
            _ => {
                if offline {
                    let _ = std::fs::remove_file(&dest);
                    return Err(artefact_unavailable(a, "the cached copy did not match its pinned digest and offline mode cannot re-fetch it"));
                }
                let _ = std::fs::remove_file(&dest);
            }
        }
    }
    if offline {
        return Err(artefact_unavailable(
            a,
            "offline mode was asked for and the artefact is not in the local cache",
        ));
    }
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{}.part", a.file_name));
    stream_download(a.url, &tmp).map_err(|e| artefact_unavailable(a, &e.to_string()))?;
    let got = hex_sha256_file(&tmp)?;
    if got != a.sha256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::coded(
            6704,
            Error::Unavailable(super::po::tf(
                "the artefact at {url} does not match the digest this release pins: expected \
                 {want}, got {got}. The download was discarded and NOTHING was installed. This \
                 is not a transient failure — report it rather than working around it",
                &[("url", a.url), ("want", a.sha256), ("got", &got)],
            )),
        ));
    }
    // Rename only after the digest matched, so the cache never holds a file
    // under its real name that was not verified under it.
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

fn artefact_unavailable(a: &Artefact, why: &str) -> Error {
    Error::coded(
        6704,
        Error::Unavailable(super::po::tf(
            "could not obtain the pod-network artefact {file} ({url}): {why}. Nothing was \
             installed. To pre-populate the cache on a machine with reach, run `delonix cluster \
             network fetch` and copy `<root>/net-profiles/` across",
            &[("file", a.file_name), ("url", a.url), ("why", why)],
        )),
    )
}

// ---------------------------------------------------------------------------
//  Rendering
// ---------------------------------------------------------------------------

/// Turns a verified artefact into the YAML to apply.
///
/// A manifest is read and substituted; a Helm chart is rendered with the
/// system `helm`, on the HOST, with `template` and never `install` — so there
/// is no release state in the cluster for a later run to diverge from, and the
/// output is an ordinary manifest that `kubectl apply` makes idempotent.
pub(crate) fn render(plan: &NetworkPlan, artefact: &Path) -> Result<String> {
    let a = plan
        .profile
        .artefact()
        .ok_or_else(|| Error::Invalid("profile has no artefact to render".into()))?;
    match a.kind {
        ArtefactKind::Manifest => {
            let text = std::fs::read_to_string(artefact)
                .map_err(|e| Error::Invalid(format!("reading {}: {e}", artefact.display())))?;
            Ok(substitute_manifest(plan, &text))
        }
        ArtefactKind::HelmChart => render_helm(plan, artefact),
    }
}

/// Substitutes the values a rendered manifest has to carry.
///
/// Pure, and deliberately narrow: it rewrites the KNOWN keys of the known
/// upstream manifests, never arbitrary text. A blind `sed` over a 700 KiB
/// manifest is how a substitution silently lands in a comment or in an
/// unrelated field.
pub(crate) fn substitute_manifest(plan: &NetworkPlan, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // The one piece of state the pass needs: an env var's `value:` is on the
    // LINE AFTER its `- name:`, so setting the value means remembering what
    // the previous line named.
    //
    // The first version of this rewrote the `- name:` line into name+value
    // and let the original `value:` line through as well — a manifest with
    // two `value:` keys in one env entry, which YAML accepts as the last one
    // winning. It passed a naive eyeball and failed its own test, which is
    // the whole reason the test asserts the RESULTING pair and not the
    // substitution.
    let mut pending_env: Option<&'static str> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if let (Profile::Calico, Some(want)) = (plan.profile, pending_env) {
            if trimmed.starts_with("value:") {
                out.push_str(indent);
                out.push_str("value: \"");
                out.push_str(want);
                out.push_str("\"\n");
                pending_env = None;
                continue;
            }
            // The env entry had no `value:` of its own — leave the line alone
            // and stop waiting, rather than rewriting something unrelated
            // further down.
            pending_env = None;
        }
        let replaced = match plan.profile {
            // Flannel's ConfigMap carries the pod CIDR and the MTU as JSON
            // inside a string value.
            Profile::Flannel => flannel_line(plan, trimmed, indent),
            // Calico's manifest carries the encapsulation in
            // CALICO_IPV4POOL_IPIP / _VXLAN and the MTU in the
            // `calico-config` ConfigMap's `veth_mtu`.
            Profile::Calico => {
                pending_env = calico_pending_env(plan, trimmed);
                calico_line(plan, trimmed, indent)
            }
            _ => None,
        };
        out.push_str(replaced.as_deref().unwrap_or(line));
        out.push('\n');
    }
    out
}

/// The Flannel `net-conf.json` lines that have to change.
///
/// BUG FOUND BY `the_substitution_lands_on_the_real_upstream_manifests`, and
/// the reason that test exists: this function used to REPLACE a `"MTU":` line.
/// The real `kube-flannel.yml` v0.28.10 has no `MTU` key at all —
/// `net-conf.json` is `Network`, `EnableNFTables` and `Backend` and nothing
/// else. So the MTU substitution matched nothing, Flannel autodetected the
/// NODE MTU, and every pod paid the 50-byte VXLAN header twice: large packets
/// silently dropped, which reads as unexplained slowness. Exactly the failure
/// `encap_overhead` was written to prevent, defeated by a substitution that
/// assumed a key the file does not have. The excerpt tests passed throughout,
/// because the excerpt was written from the assumption.
///
/// So the MTU is INSERTED, anchored on the `"Network":` line that does exist,
/// and the backend type is set from the plan.
fn flannel_line(plan: &NetworkPlan, trimmed: &str, indent: &str) -> Option<String> {
    if trimmed.starts_with("\"Network\":") {
        // One output line per input line is not a constraint here — the pass
        // writes whatever this returns — so the insert rides along with the
        // anchor and keeps its indentation.
        return Some(format!(
            "{indent}\"Network\": \"{}\",\n{indent}\"MTU\": {},",
            cidr_text(plan.pod_subnet),
            plan.mtu
        ));
    }
    // An MTU key DOES exist in some older releases and in a manifest an
    // operator has already edited. Replace it then, so the insert above and a
    // pre-existing key cannot both end up in the document.
    if trimmed.starts_with("\"MTU\":") {
        return Some(String::new());
    }
    // `Backend.Type` is the dataplane. Flannel ships `vxlan`, which is also
    // the only mode this engine renders for it — but reading it from the plan
    // rather than trusting the default is what makes a future second mode a
    // one-line change instead of a silent mismatch.
    if trimmed.starts_with("\"Type\":") {
        return Some(format!("{indent}\"Type\": \"{}\"", plan.dataplane));
    }
    None
}

/// Which value the NEXT line should carry, when this line names one of
/// Calico's two encapsulation switches.
///
/// They are a matched pair: exactly one is `Always`. Setting both, or
/// neither, is a cluster that encapsulates twice or not at all.
fn calico_pending_env(plan: &NetworkPlan, trimmed: &str) -> Option<&'static str> {
    let on = |yes: bool| Some(if yes { "Always" } else { "Never" });
    match trimmed {
        "- name: CALICO_IPV4POOL_IPIP" => on(plan.dataplane == "ipip"),
        "- name: CALICO_IPV4POOL_VXLAN" => on(plan.dataplane == "vxlan"),
        _ => None,
    }
}

/// The Calico lines this profile rewrites in place.
fn calico_line(plan: &NetworkPlan, trimmed: &str, indent: &str) -> Option<String> {
    // `veth_mtu: "0"` in the shipped ConfigMap means "autodetect", which
    // detects the NODE MTU and knows nothing about our encapsulation.
    if trimmed.starts_with("veth_mtu:") {
        return Some(format!("{indent}veth_mtu: \"{}\"", plan.mtu));
    }
    None
}

/// `10.244.0.0/16` from a parsed CIDR.
pub(crate) fn cidr_text(c: super::net_profiles::Cidr4) -> String {
    format!(
        "{}.{}.{}.{}/{}",
        c.network >> 24,
        (c.network >> 16) & 0xff,
        (c.network >> 8) & 0xff,
        c.network & 0xff,
        c.prefix
    )
}

/// The `helm template` argument list for a Cilium install.
///
/// Pure and returned as a `Vec` so the flags are unit-tested without running
/// helm: the values here ARE the configuration, and a typo in one of them is a
/// cluster that comes up with the wrong dataplane and says nothing.
pub(crate) fn cilium_helm_values(plan: &NetworkPlan) -> Vec<String> {
    let mut v = vec![
        // The pod CIDR has to be the one kubeadm was told, or the IPAM hands
        // out addresses the cluster does not route.
        format!(
            "ipam.operator.clusterPoolIPv4PodCIDRList={}",
            cidr_text(plan.pod_subnet)
        ),
        format!("tunnelProtocol={}", plan.dataplane),
        "routingMode=tunnel".to_string(),
        format!("MTU={}", plan.mtu),
        // This engine's CRI is not containerd and does not own containerd's
        // paths; Cilium writes its CNI conflist into the standard directories,
        // which is what `delonix-cri`'s root path reads.
        "cni.binPath=/opt/cni/bin".to_string(),
        "cni.confPath=/etc/cni/net.d".to_string(),
        // Hubble is opt-in: it is a second set of pods and a second thing that
        // can be NotReady, and it buys nothing until the dataplane works.
        format!("hubble.enabled={}", plan.hubble),
        format!("hubble.relay.enabled={}", plan.hubble),
    ];
    match plan.kube_proxy {
        // The replacement needs to reach the apiserver WITHOUT kube-proxy,
        // which means it has to be told where the apiserver is. The caller
        // fills these two in (they are cluster facts, not profile facts).
        "none" => v.push("kubeProxyReplacement=true".to_string()),
        _ => v.push("kubeProxyReplacement=false".to_string()),
    }
    if let Some(dev) = &plan.interface {
        v.push(format!("devices={dev}"));
    }
    v
}

/// Renders the Cilium chart with the system `helm`.
///
/// `helm` is a host tool here, in the same family as `ssh`, `scp`, `qemu-img`,
/// `virt-customize` and `gpgv` — which this engine already shells out to. It
/// is NOT a new runtime dependency of a node, and nothing resident is
/// installed in the cluster. Its absence is a named refusal, never a silent
/// fallback to a different CNI.
fn render_helm(plan: &NetworkPlan, chart: &Path) -> Result<String> {
    if which("helm").is_none() {
        return Err(Error::coded(
            6704,
            Error::Unavailable(super::po::t(
                "`helm` was not found on this host, and the Cilium profile is published only as \
                 a Helm chart — this engine renders it here, with `helm template`, and applies \
                 the resulting manifest (it never runs `helm install`, so no release state is \
                 left in the cluster). Install helm on the machine running `delonix`, or choose \
                 a profile published as a manifest (`--cni calico`, `--cni flannel`). Nothing \
                 was installed",
            )
            .to_string()),
        ));
    }
    let mut cmd = std::process::Command::new("helm");
    cmd.arg("template")
        .arg("cilium")
        .arg(chart)
        .arg("--namespace")
        .arg("kube-system");
    for v in cilium_helm_values(plan) {
        cmd.arg("--set").arg(v);
    }
    let out = cmd
        .output()
        .map_err(|e| Error::Invalid(format!("helm template: {e}")))?;
    if !out.status.success() {
        return Err(Error::Invalid(super::po::tf(
            "helm template failed for the cilium chart: {err}",
            &[("err", String::from_utf8_lossy(&out.stderr).trim())],
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Teaches a rendered Cilium manifest where the apiserver is.
///
/// Only needed with `kubeProxyReplacement=true`: the agent cannot reach the
/// apiserver through the `kubernetes` Service while it is the thing
/// implementing Services, so it needs the real host and port. Those are
/// CLUSTER facts (the `controlPlaneEndpoint`), which is why they are added
/// here instead of inside the host-agnostic value list.
///
/// Implemented as a ConfigMap patch appended as its own document rather than a
/// rewrite of the chart's output: the chart's ConfigMap is one object among
/// hundreds, and a textual edit inside it is the kind of substitution that
/// lands in the wrong place. `kubectl apply --server-side` merges the second
/// document over the first.
pub(crate) fn with_api_server(yaml: &str, endpoint: &str) -> String {
    let (host, port) = match endpoint.rsplit_once(':') {
        // An IPv6 literal has colons of its own; only a trailing `:<digits>`
        // is a port.
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h, p),
        _ => (endpoint, "6443"),
    };
    format!(
        "{yaml}\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: cilium-config\n  \
         namespace: kube-system\ndata:\n  k8s-service-host: {host:?}\n  k8s-service-port: \
         {port:?}\n"
    )
}

/// First match for `name` on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(name))
            .find(|c| c.is_file())
    })
}

// ---------------------------------------------------------------------------
//  Install
// ---------------------------------------------------------------------------

/// What this engine recorded about a cluster's pod network, next to its
/// kubeconfig. The DESIRED state; [`observed_profile`] reads the actual one.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ProfileRecord {
    pub cni: String,
    pub version: String,
    pub dataplane: String,
    pub kube_proxy: String,
    pub mtu: u32,
    pub pod_subnet: String,
    pub service_subnet: String,
    pub multus: bool,
    pub hubble: bool,
    /// SHA-256 of the artefact that was actually applied — the pin, recorded,
    /// so a later run can say "the same bytes" rather than "the same version".
    pub artefact_sha256: String,
}

fn record_path(cluster: &str) -> PathBuf {
    state_root()
        .join("clusters")
        .join(cluster)
        .join("network-profile.json")
}

pub(crate) fn write_record(cluster: &str, rec: &ProfileRecord) -> Result<()> {
    let p = record_path(cluster);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let json = serde_json::to_string_pretty(rec)
        .map_err(|e| Error::Invalid(format!("serializing the network profile record: {e}")))?;
    std::fs::write(&p, json)?;
    Ok(())
}

pub(crate) fn read_record(cluster: &str) -> Option<ProfileRecord> {
    let text = std::fs::read_to_string(record_path(cluster)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Which pod network the LIVE cluster is actually running.
///
/// Probed from the cluster, never from our own record: the record is what we
/// meant, and the whole point of a drift check is that the two can differ.
/// Each profile is recognised by a DaemonSet it owns in `kube-system`, which
/// is what survives an operator having applied it by hand.
pub(crate) fn observed_profile(cp1: &SshTarget) -> String {
    let out = remote::ssh_run(
        cp1,
        "kubectl --kubeconfig=/etc/kubernetes/admin.conf -n kube-system get daemonset \
         -o name 2>/dev/null",
    )
    .unwrap_or_default();
    for (needle, name) in [
        ("cilium", "cilium"),
        ("calico-node", "calico"),
        ("kube-flannel", "flannel"),
    ] {
        if out.contains(needle) {
            return name.to_string();
        }
    }
    // kube-flannel also ships in its own namespace in recent releases.
    let ns = remote::ssh_run(
        cp1,
        "kubectl --kubeconfig=/etc/kubernetes/admin.conf get ns -o name 2>/dev/null",
    )
    .unwrap_or_default();
    if ns.contains("kube-flannel") {
        return "flannel".to_string();
    }
    String::new()
}

/// Applies the rendered manifest to the cluster, idempotently.
///
/// `kubectl apply` is the idempotent verb by construction — a second run of
/// the same bytes is a no-op — so this needs no `check` of its own, unlike the
/// host recipes. What it DOES need is to not be mistaken for success: the
/// caller must wait (see [`wait_until_ready`]).
pub(crate) fn apply_manifest(cp1: &SshTarget, label: &str, yaml: &str) -> Result<()> {
    let tmp = delonix_state::write_private_temp(&format!("delonix-{label}.yaml"), yaml.as_bytes())
        .map_err(|e| Error::Invalid(format!("writing the {label} manifest: {e}")))?;
    let remote_path = format!("/tmp/delonix-{label}.yaml");
    let sent = remote::scp_to(cp1, &tmp, &remote_path);
    let _ = std::fs::remove_file(&tmp);
    sent.map_err(|e| Error::Invalid(format!("{label} manifest: {e}")))?;
    remote::ssh_run(
        cp1,
        &format!(
            "KUBECONFIG=/etc/kubernetes/admin.conf kubectl apply --server-side --force-conflicts \
             -f {remote_path} && rm -f {remote_path}"
        ),
    )
    .map_err(|e| {
        Error::Invalid(super::po::tf(
            "applying the {label} manifest failed: {err}. The cluster is bootstrapped but has no \
         working pod network; nothing was rolled back, and re-running the same apply is safe",
            &[("label", label), ("err", &e.to_string())],
        ))
    })?;
    Ok(())
}

/// What a readiness poll observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Readiness {
    pub agents_ready: u32,
    pub agents_desired: u32,
    pub nodes_ready: u32,
    pub nodes_total: u32,
}

impl Readiness {
    pub(crate) fn converged(&self, expected_nodes: u32) -> bool {
        self.agents_desired > 0
            && self.agents_ready >= self.agents_desired
            && self.nodes_ready >= expected_nodes
            && expected_nodes > 0
    }
}

/// The DaemonSet that IS the profile's agent on every node.
fn agent_daemonset(p: Profile) -> (&'static str, &'static str) {
    match p {
        Profile::Cilium => ("kube-system", "cilium"),
        Profile::Calico => ("kube-system", "calico-node"),
        Profile::Flannel => ("kube-flannel", "kube-flannel-ds"),
        _ => ("", ""),
    }
}

/// Reads the agent DaemonSet's ready/desired and the nodes' Ready count.
///
/// One SSH round-trip for both, because the pair is what makes a verdict: an
/// agent `Ready` on 3 of 3 nodes while only 1 node is `Ready` means the agent
/// is up and the kubelet still disagrees, which is a different problem from
/// an agent that never scheduled.
pub(crate) fn poll_readiness(cp1: &SshTarget, p: Profile) -> Readiness {
    let (ns, ds) = agent_daemonset(p);
    let out = remote::ssh_run(
        cp1,
        &format!(
            "export KUBECONFIG=/etc/kubernetes/admin.conf; \
             kubectl -n {ns} get daemonset {ds} \
               -o jsonpath='{{.status.numberReady}} {{.status.desiredNumberScheduled}}' \
               2>/dev/null; echo; \
             kubectl get nodes --no-headers 2>/dev/null"
        ),
    )
    .unwrap_or_default();
    parse_readiness(&out)
}

/// Pure half of [`poll_readiness`] — the parsing, so the verdict is testable
/// without a cluster.
pub(crate) fn parse_readiness(out: &str) -> Readiness {
    let mut lines = out.lines();
    let (mut agents_ready, mut agents_desired) = (0u32, 0u32);
    if let Some(first) = lines.next() {
        let mut it = first.split_whitespace();
        agents_ready = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        agents_desired = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    }
    let mut nodes_ready = 0u32;
    let mut nodes_total = 0u32;
    for l in lines {
        if l.trim().is_empty() {
            continue;
        }
        nodes_total += 1;
        // `Ready` exactly — `NotReady` and `Ready,SchedulingDisabled` are not
        // the same thing and the second column is where they differ.
        if l.split_whitespace().nth(1) == Some("Ready") {
            nodes_ready += 1;
        }
    }
    Readiness {
        agents_ready,
        agents_desired,
        nodes_ready,
        nodes_total,
    }
}

/// Waits until the profile is genuinely up, or fails with what it saw.
///
/// The deadline is bounded and the failure is diagnostic: it names the agent
/// counts, the node counts and the command to look further. It does NOT
/// delete, re-apply or fall back — a half-installed CNI is evidence, and
/// destroying it to make a command exit 0 would throw away the only state that
/// explains the failure.
pub(crate) fn wait_until_ready(
    cp1: &SshTarget,
    cluster: &str,
    p: Profile,
    expected_nodes: u32,
    timeout: Duration,
) -> Result<Readiness> {
    let deadline = Instant::now() + timeout;
    loop {
        let last = poll_readiness(cp1, p);
        if last.converged(expected_nodes) {
            return Ok(last);
        }
        if Instant::now() >= deadline {
            let (ns, ds) = agent_daemonset(p);
            return Err(Error::coded(
                6704,
                Error::Unavailable(super::po::tf(
                    "the {cni} pod network did not converge within {secs}s: its agent is Ready on \
                     {ar}/{ad} node(s) and {nr}/{exp} node(s) report Ready. The cluster is \
                     bootstrapped and the manifest WAS applied — nothing was deleted or rolled \
                     back, so the evidence is still there. Look with `delonix cluster {name} \
                     -n {ns} describe daemonset {ds}` and `delonix cluster {name} -n {ns} logs \
                     -l k8s-app={ds}`; re-running the same apply is safe and resumes from here",
                    &[
                        ("cni", p.name()),
                        ("secs", &timeout.as_secs().to_string()),
                        ("ar", &last.agents_ready.to_string()),
                        ("ad", &last.agents_desired.to_string()),
                        ("nr", &last.nodes_ready.to_string()),
                        ("exp", &expected_nodes.to_string()),
                        ("name", cluster),
                        ("ns", ns),
                        ("ds", ds),
                    ],
                )),
            ));
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::super::net_profiles::{self, NetworkRequest};
    use super::*;

    fn plan(cni: &str, dataplane: Option<&str>) -> NetworkPlan {
        net_profiles::validate(&NetworkRequest {
            cni: cni.into(),
            dataplane: dataplane.map(String::from),
            pod_subnet: "10.244.0.0/16".into(),
            service_subnet: "10.96.0.0/12".into(),
            mode: "ssh".into(),
            nodes: 3,
            experimental: true,
            ..Default::default()
        })
        .expect("a valid plan")
    }

    #[test]
    fn a_cidr_round_trips_through_the_text_form() {
        for s in ["10.244.0.0/16", "192.168.0.0/24", "0.0.0.0/0"] {
            let c = net_profiles::Cidr4::parse(s).unwrap();
            assert_eq!(cidr_text(c), s);
        }
    }

    /// The pod CIDR and the MTU must reach Flannel's ConfigMap. The MTU is
    /// INSERTED, because the shipped `net-conf.json` has no `MTU` key — see
    /// `flannel_line`'s own comment for the bug that taught this.
    #[test]
    fn the_flannel_substitution_inserts_the_mtu_and_sets_the_cidr() {
        let mut p = plan("flannel", None);
        p.pod_subnet = net_profiles::Cidr4::parse("10.99.0.0/16").unwrap();
        // The REAL shape: Network, EnableNFTables, Backend.Type. No MTU.
        let src = concat!(
            "  net-conf.json: |\n",
            "    {\n",
            "      \"Network\": \"10.244.0.0/16\",\n",
            "      \"EnableNFTables\": false,\n",
            "      \"Backend\": {\n",
            "        \"Type\": \"vxlan\"\n",
            "      }\n",
            "    }\n",
        );
        let out = substitute_manifest(&p, src);
        assert!(out.contains("\"Network\": \"10.99.0.0/16\","), "{out}");
        assert!(
            out.contains("\"MTU\": 1450,"),
            "the MTU was not inserted: {out}"
        );
        // Indentation is preserved on BOTH — this is JSON inside a YAML block
        // scalar, and losing the indent loses the document.
        assert!(out.contains("      \"Network\""), "{out}");
        assert!(out.contains("      \"MTU\""), "{out}");
        // Untouched lines survive byte for byte.
        assert!(out.contains("\"EnableNFTables\": false,"), "{out}");
        // And it stays valid JSON once the block scalar is unwrapped.
        let json: String = out
            .lines()
            .skip_while(|l| !l.trim_start().starts_with('{'))
            .map(|l| l.trim().to_string())
            .collect::<Vec<_>>()
            .join("");
        let v: serde_json::Value =
            serde_json::from_str(&json).unwrap_or_else(|e| panic!("not JSON: {e}\n{json}"));
        assert_eq!(v["MTU"], 1450);
        assert_eq!(v["Network"], "10.99.0.0/16");
        assert_eq!(v["Backend"]["Type"], "vxlan");
    }

    /// A manifest that ALREADY has an MTU key must not end up with two.
    #[test]
    fn a_preexisting_flannel_mtu_is_not_duplicated() {
        let p = plan("flannel", None);
        let src = concat!(
            "    {\n",
            "      \"Network\": \"10.244.0.0/16\",\n",
            "      \"MTU\": 1500,\n",
            "      \"Backend\": {\n",
            "        \"Type\": \"vxlan\"\n",
            "      }\n",
            "    }\n",
        );
        let out = substitute_manifest(&p, src);
        assert_eq!(out.matches("\"MTU\"").count(), 1, "{out}");
        assert!(out.contains("\"MTU\": 1450,"), "{out}");
        assert!(!out.contains("1500"), "{out}");
    }

    /// Calico's two encapsulation switches are a matched pair: exactly one is
    /// `Always`. Setting both, or neither, is a cluster that routes nothing.
    #[test]
    fn the_calico_substitution_turns_on_exactly_one_encapsulation() {
        for (dp, ipip, vxlan) in [("vxlan", "Never", "Always"), ("ipip", "Always", "Never")] {
            let p = plan("calico", Some(dp));
            let src = concat!(
                "            - name: CALICO_IPV4POOL_IPIP\n",
                "              value: \"Always\"\n",
                "            - name: CALICO_IPV4POOL_VXLAN\n",
                "              value: \"Never\"\n",
                "  veth_mtu: \"0\"\n",
            );
            let out = substitute_manifest(&p, src);
            // The value of an env var is the FIRST `value:` after its name —
            // asserted as a pair, not as a substring anywhere in the file,
            // because "the file contains Always somewhere" is exactly what a
            // broken substitution also satisfies.
            let value_after = |name: &str| -> String {
                out.split(name)
                    .nth(1)
                    .unwrap()
                    .lines()
                    .find(|l| l.trim_start().starts_with("value:"))
                    .unwrap()
                    .trim()
                    .to_string()
            };
            assert_eq!(
                value_after("CALICO_IPV4POOL_IPIP"),
                format!("value: \"{ipip}\""),
                "{dp}: {out}"
            );
            assert_eq!(
                value_after("CALICO_IPV4POOL_VXLAN"),
                format!("value: \"{vxlan}\""),
                "{dp}: {out}"
            );
            // Exactly ONE `value:` per env entry — the first version of this
            // emitted two and YAML silently took the last.
            assert_eq!(
                out.matches("value:").count(),
                2,
                "an env entry grew a second value: {out}"
            );
            // And the MTU stops being "autodetect".
            let expect_mtu = if dp == "ipip" { "1480" } else { "1450" };
            assert!(
                out.contains(&format!("veth_mtu: \"{expect_mtu}\"")),
                "{dp}: {out}"
            );
        }
    }

    /// The Cilium values ARE the configuration. A typo here is a cluster that
    /// comes up with the wrong dataplane and says nothing, so each one is
    /// asserted rather than trusted.
    #[test]
    fn the_cilium_values_carry_the_plan() {
        let p = plan("cilium", None);
        let v = cilium_helm_values(&p);
        let has = |k: &str| v.iter().any(|s| s == k);
        assert!(
            has("ipam.operator.clusterPoolIPv4PodCIDRList=10.244.0.0/16"),
            "{v:?}"
        );
        assert!(has("tunnelProtocol=vxlan"), "{v:?}");
        assert!(has("routingMode=tunnel"), "{v:?}");
        assert!(has("MTU=1450"), "{v:?}");
        // The conflist and binaries have to land where this engine's CRI
        // reads them, which is NOT containerd's path.
        assert!(has("cni.binPath=/opt/cni/bin"), "{v:?}");
        assert!(has("cni.confPath=/etc/cni/net.d"), "{v:?}");
        // Hubble stays off until asked.
        assert!(has("hubble.enabled=false"), "{v:?}");
        assert!(has("kubeProxyReplacement=false"), "{v:?}");

        let mut g = plan("cilium", Some("geneve"));
        g.hubble = true;
        g.kube_proxy = "none";
        g.interface = Some("ens3".into());
        let v = cilium_helm_values(&g);
        let has = |k: &str| v.iter().any(|s| s == k);
        assert!(has("tunnelProtocol=geneve"), "{v:?}");
        assert!(
            has("hubble.enabled=true") && has("hubble.relay.enabled=true"),
            "{v:?}"
        );
        assert!(has("kubeProxyReplacement=true"), "{v:?}");
        assert!(has("devices=ens3"), "{v:?}");
    }

    /// A verdict needs BOTH halves. The counts are parsed from one round-trip,
    /// and the states that are not `Ready` must not be counted as it.
    #[test]
    fn readiness_is_parsed_from_both_halves() {
        let out = "3 3\n\
                   lab-cp1   Ready      control-plane   5m   v1.36.0\n\
                   lab-w1    Ready      <none>          4m   v1.36.0\n\
                   lab-w2    NotReady   <none>          4m   v1.36.0\n";
        let r = parse_readiness(out);
        assert_eq!(r.agents_ready, 3);
        assert_eq!(r.agents_desired, 3);
        assert_eq!(r.nodes_ready, 2);
        assert_eq!(r.nodes_total, 3);
        assert!(!r.converged(3), "2 of 3 nodes Ready is not converged");
        assert!(r.converged(2));

        // `Ready,SchedulingDisabled` is a cordoned node, not a Ready one.
        let cordoned = "1 1\nlab-cp1  Ready,SchedulingDisabled  control-plane  5m  v1.36.0\n";
        assert_eq!(parse_readiness(cordoned).nodes_ready, 0);

        // Nothing applied at all: the DaemonSet does not exist, so the first
        // line is empty and `desired` is 0 — which must NEVER read as
        // converged, however many nodes happen to be Ready.
        let nothing = "\nlab-cp1  Ready  control-plane  5m  v1.36.0\n";
        let r = parse_readiness(nothing);
        assert_eq!(r.agents_desired, 0);
        assert!(!r.converged(1), "a missing DaemonSet is not a ready one");

        // And the empty output of an unreachable control-plane.
        assert!(!parse_readiness("").converged(1));
    }

    /// Every add-on profile must name a DaemonSet, or `wait_until_ready`
    /// would poll a resource that does not exist and time out saying nothing.
    #[test]
    fn every_addon_profile_names_its_agent_daemonset() {
        for p in [Profile::Cilium, Profile::Calico, Profile::Flannel] {
            let (ns, ds) = agent_daemonset(p);
            assert!(!ns.is_empty() && !ds.is_empty(), "{}", p.name());
        }
    }

    /// The substitution, run over the REAL upstream manifests.
    ///
    /// The tests above use hand-written excerpts, which prove the algorithm
    /// and nothing about the actual files: upstream can rename a key, move the
    /// MTU, or stop shipping an env var, and a passing excerpt test would say
    /// everything is fine while the substitution quietly matched nothing. This
    /// one reads the pinned artefact from the local cache and asserts the
    /// result — the only version of this check that can catch that.
    ///
    /// SKIPS with a reason when the cache is absent (CI, a fresh checkout)
    /// rather than downloading inside a unit test. Populate it with
    /// `delonix cluster network fetch`.
    #[test]
    fn the_substitution_lands_on_the_real_upstream_manifests() {
        let dir = cache_dir();
        for (cni, file) in [
            ("flannel", "kube-flannel-v0.28.10.yml"),
            ("calico", "calico-v3.33.0.yaml"),
        ] {
            let path = dir.join(file);
            let Ok(text) = std::fs::read_to_string(&path) else {
                eprintln!(
                    "SKIP {cni}: {} not cached — run `delonix cluster network fetch`",
                    path.display()
                );
                continue;
            };
            let p = plan(cni, None);
            let out = substitute_manifest(&p, &text);
            // The substitution has to have CHANGED something. A no-op over a
            // 700 KiB manifest is the failure mode this test exists for.
            assert_ne!(out, text, "{cni}: the substitution matched nothing");
            // The MTU is the value both profiles would otherwise autodetect as
            // the NODE MTU, paying the VXLAN header twice.
            assert!(
                out.contains("1450"),
                "{cni}: the computed MTU (1450) is not in the output"
            );
            if cni == "flannel" {
                // The decisive check: parse the REAL document as YAML, pull
                // `net-conf.json` out of the ConfigMap, and parse THAT as
                // JSON. Substring assertions cannot tell a valid document
                // from one whose block-scalar indentation was broken by the
                // insert — and a broken one is rejected by the apiserver with
                // a message that points nowhere near this function.
                let conf = yaml_docs(&out)
                    .into_iter()
                    .find_map(|d| {
                        d.get("data")?
                            .get("net-conf.json")?
                            .as_str()
                            .map(str::to_owned)
                    })
                    .expect("the flannel manifest has a net-conf.json");
                let v: serde_json::Value = serde_json::from_str(&conf).unwrap_or_else(|e| {
                    panic!("net-conf.json is not JSON after substitution: {e}\n{conf}")
                });
                assert_eq!(v["Network"], "10.244.0.0/16");
                assert_eq!(v["MTU"], 1450, "the MTU was not inserted: {conf}");
                assert_eq!(v["Backend"]["Type"], "vxlan");
            } else {
                assert!(
                    out.contains("veth_mtu: \"1450\""),
                    "calico veth_mtu not set"
                );
                // Exactly one of the two encapsulation switches is `Always`.
                let value_after = |name: &str| -> Option<String> {
                    out.split(name).nth(1)?.lines().find_map(|l| {
                        let t = l.trim();
                        t.strip_prefix("value:").map(|v| v.trim().to_string())
                    })
                };
                assert_eq!(
                    value_after("CALICO_IPV4POOL_VXLAN").as_deref(),
                    Some("\"Always\"")
                );
                assert_eq!(
                    value_after("CALICO_IPV4POOL_IPIP").as_deref(),
                    Some("\"Never\"")
                );
            }
            // Every document still parses, and there are as many as before: a
            // substitution that breaks one object's indentation can merge it
            // into its neighbour, and the count is what notices.
            let before = yaml_docs(&text).len();
            let after = yaml_docs(&out);
            assert_eq!(after.len(), before, "{cni}: the document count changed");
            assert!(before > 1, "{cni}: expected a multi-document manifest");
        }
    }

    /// Parses a multi-document manifest, skipping the empty documents a
    /// trailing `---` produces. A parse failure PANICS rather than yielding
    /// fewer documents: "it did not parse" and "it had fewer objects" are
    /// different findings and must not look the same.
    fn yaml_docs(text: &str) -> Vec<serde_yaml::Value> {
        serde_yaml::Deserializer::from_str(text)
            .map(|d| {
                serde_yaml::Value::deserialize(d)
                    .unwrap_or_else(|e| panic!("a document did not parse after substitution: {e}"))
            })
            .filter(|v| !v.is_null())
            .collect()
    }

    /// Offline with an empty cache is a named refusal, not a silent download.
    #[test]
    fn offline_without_a_cached_artefact_is_refused_by_name() {
        let a = Artefact {
            kind: ArtefactKind::Manifest,
            url: "https://example.invalid/never-fetched.yaml",
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            file_name: "delonix-test-never-fetched.yaml",
        };
        // The cache dir is the real one, but this file name is never produced
        // by a profile, so the test neither reads nor writes anybody's cache.
        let e = fetch_artefact(&a, true).unwrap_err();
        assert_eq!(e.number(), 6704, "{e}");
        assert!(e.to_string().contains("offline mode"), "{e}");
    }
}

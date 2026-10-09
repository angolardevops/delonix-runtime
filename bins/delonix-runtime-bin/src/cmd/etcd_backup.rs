//! `cluster backup`/`cluster restore` — `etcdctl snapshot save`/`restore`
//! for a VM-provisioned, STACKED etcd control-plane (KaaS capability audit,
//! gap #10: `cluster destroy` got a teardown half earlier; this is the
//! other half — the engine had no `etcdctl snapshot` wiring at all, so an
//! operator's only recourse for a disaster-recovery backup was logging
//! into a node by hand).
//!
//! Scope, stated up front because it is the honest boundary, not an
//! afterthought: only `etcd.mode: stacked` (the kubeadm default,
//! co-located on the control-plane) — `etcd.mode: external` has its own PKI
//! layout (`/etc/etcd/pki/...`, see `etcd.rs`) and is refused by name on
//! both verbs, never silently attempted. [`backup`] connects to whichever
//! `<name>-cp1` it finds regardless of how many OTHER control-planes
//! exist — any single member's snapshot reflects the same keyspace.
//! [`restore`] refuses outright above one `-cpN` node: restoring one member
//! of a multi-member stacked cluster correctly needs a working
//! `--initial-cluster` for every OTHER member too, which this does not
//! attempt.
//!
//! **Not validated against a real cluster in the session that wrote
//! this** — see each function's own doc comment for exactly what the
//! boundary is. `restore`'s command sequence mirrors Kubernetes' own
//! documented etcd disaster-recovery procedure (stop the static pod,
//! preserve the old data directory, `etcdctl snapshot restore` under the
//! EXISTING member's own identity read back from its manifest, restart the
//! static pod) — test it against a disposable cluster before trusting it
//! against one that matters.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use delonix_model::{Error, Result};

use super::kubeadm_config::{
    KUBEADM_ETCD_CA_PATH, KUBEADM_ETCD_CLIENT_CERT_PATH, KUBEADM_ETCD_CLIENT_KEY_PATH,
};
use super::remote::{self, SshTarget};
use super::util::state_root;

/// What a stacked kubeadm etcd member's own static pod manifest says about
/// itself, read back from the LIVE node and parsed LOCALLY — never
/// reconstructed from a naming convention — so a restore brings the member
/// back with EXACTLY the identity the rest of the cluster already expects.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EtcdMemberIdentity {
    name: String,
    initial_cluster: String,
    peer_urls: String,
    data_dir: String,
    version: String,
}

/// Pulls `--name=`/`--initial-cluster=`/`--initial-advertise-peer-urls=`/
/// `--data-dir=` out of a kubeadm-rendered etcd static pod manifest's
/// `command:` block, plus the etcd version from its `image:` tag.
///
/// PURE and unit-tested with a real fixture on purpose, instead of a
/// remote `grep -oP` round trip: PCRE support in the node's `grep` is not
/// guaranteed, and a wrong value here does not fail loudly — it silently
/// restores a member under a different name/cluster than the one actually
/// running, which `etcdctl` accepts without complaint.
fn parse_etcd_member_identity(manifest_yaml: &str) -> Result<EtcdMemberIdentity> {
    fn flag_value(text: &str, flag: &str) -> Option<String> {
        let needle = format!("{flag}=");
        text.split_whitespace()
            .map(|tok| tok.trim_start_matches('-'))
            .find_map(|tok| tok.strip_prefix(&needle).map(str::to_string))
    }
    fn required(text: &str, flag: &str) -> Result<String> {
        flag_value(text, flag).ok_or_else(|| {
            Error::Invalid(super::po::tf(
                "could not find --{flag}= in the etcd static pod manifest — refusing to \
                 restore blind",
                &[("flag", flag)],
            ))
        })
    }

    let name = required(manifest_yaml, "name")?;
    let initial_cluster = required(manifest_yaml, "initial-cluster")?;
    let peer_urls = required(manifest_yaml, "initial-advertise-peer-urls")?;
    let data_dir = required(manifest_yaml, "data-dir")?;
    let version = manifest_yaml
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("image:") && l.contains("etcd"))
        .and_then(|l| l.trim_start_matches("image:").trim().rsplit_once(':'))
        .map(|(_, v)| v.to_string())
        .ok_or_else(|| {
            Error::Invalid(
                super::po::t("could not find the etcd image tag in the static pod manifest")
                    .to_string(),
            )
        })?;

    Ok(EtcdMemberIdentity {
        name,
        initial_cluster,
        peer_urls,
        data_dir,
        version,
    })
}

/// Resolves cluster `name`'s `<name>-cp1` as an [`SshTarget`], plus how
/// many `-cpN` control-planes it actually has (so [`restore`] can refuse
/// above one). Confirms the node is reachable and runs STACKED etcd
/// (`/etc/kubernetes/manifests/etcd.yaml` exists) before returning — the
/// same two checks either caller would otherwise have to repeat.
fn resolve_cp1(name: &str) -> Result<(SshTarget, usize)> {
    let vms = super::cluster::cluster_vm_names(name)?;
    if vms.is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "cluster '{name}' has no VM-provisioned nodes known to this engine — `cluster \
             backup`/`cluster restore` only support clusters created by `cluster kubeadm`",
            &[("name", name)],
        )));
    }
    let cp_count = vms
        .iter()
        .filter(|v| {
            v.strip_prefix(&format!("{name}-cp"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .count();
    let cp1_name = format!("{name}-cp1");
    if !vms.iter().any(|v| v == &cp1_name) {
        return Err(Error::Invalid(super::po::tf(
            "cluster '{name}' has no control-plane node '{cp1}' — nothing to connect to",
            &[("name", name), ("cp1", &cp1_name)],
        )));
    }

    let base = state_root();
    let vm = delonix_vm::status(&base, &cp1_name)?;
    let ip = vm.ip.ok_or_else(|| {
        Error::Invalid(super::po::tf(
            "'{cp1}' has no IP assigned — is it running? (`delonix vm ls`)",
            &[("cp1", &cp1_name)],
        ))
    })?;
    let (key_path, _public) = super::cluster::generate_or_load_ssh_key(name, None)?;
    let target = SshTarget {
        host: ip,
        user: "delonix".to_string(),
        key: Some(key_path),
        port: None,
    };

    if !remote::ssh_check(&target, "true") {
        return Err(Error::Invalid(super::po::tf(
            "could not reach '{cp1}' over SSH ({host}) — is the VM up?",
            &[("cp1", &cp1_name), ("host", &target.host)],
        )));
    }
    if !remote::ssh_check(&target, "test -f /etc/kubernetes/manifests/etcd.yaml") {
        return Err(Error::Invalid(
            super::po::t(
                "this control-plane has no stacked etcd static pod manifest — `etcd.mode: \
                 external` clusters are not supported by `cluster backup`/`cluster restore` \
                 yet",
            )
            .to_string(),
        ));
    }

    Ok((target, cp_count))
}

/// `cluster backup <name> [--to <path>]` — `etcdctl snapshot save`,
/// streamed back over SSH and written locally without ever loosening the
/// remote file's permissions: the whole sequence (save, base64-encode,
/// delete the remote temp file) runs as ONE privileged command
/// ([`remote::ssh_run`]'s own `sudo -n bash -c`), so the snapshot — which
/// is the cluster's entire keyspace, Secrets included — exists on the
/// node's disk for exactly the lifetime of that one command, never read
/// back by a second, separate connection the way an earlier bug in
/// `fetch_kubeconfig` once did for `admin.conf` (see that function's own
/// doc comment).
///
/// Read-only against etcd (`snapshot save` never mutates the keyspace), but
/// **not validated against a real cluster** in the session that wrote
/// this — see the module doc comment.
pub(crate) fn backup(name: &str, to: Option<PathBuf>) -> Result<()> {
    let (target, _cp_count) = resolve_cp1(name)?;

    let manifest = remote::ssh_run(&target, "cat /etc/kubernetes/manifests/etcd.yaml")?;
    let identity = parse_etcd_member_identity(&manifest)?;

    let (_etcd_bin, etcdctl_bin) = super::etcd::download_and_cache_etcd(&identity.version)?;
    let id = delonix_node::generate_id();
    let remote_etcdctl = format!("/tmp/delonix-etcdctl-{id}");
    let remote_snap = format!("/tmp/delonix-etcd-snapshot-{id}.db");
    remote::scp_to(&target, &etcdctl_bin, &remote_etcdctl)?;

    let save_and_encode = format!(
        "chmod +x {remote_etcdctl} && \
         ETCDCTL_API=3 {remote_etcdctl} \
           --endpoints=https://127.0.0.1:2379 \
           --cacert={KUBEADM_ETCD_CA_PATH} \
           --cert={KUBEADM_ETCD_CLIENT_CERT_PATH} \
           --key={KUBEADM_ETCD_CLIENT_KEY_PATH} \
           snapshot save {remote_snap} >/dev/null && \
         base64 -w0 {remote_snap}"
    );
    let result = remote::ssh_run(&target, &save_and_encode);
    let _ = remote::ssh_run(&target, &format!("rm -f {remote_snap} {remote_etcdctl}"));
    let b64 = result?;

    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = STANDARD.decode(&cleaned).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::t("decoding the etcd snapshot")
        ))
    })?;

    let dest = to.unwrap_or_else(|| {
        state_root()
            .join("clusters")
            .join(name)
            .join(format!("etcd-snapshot-{}.db", delonix_node::now_unix()))
    });
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    delonix_state::write_atomic_mode(&dest, &bytes, Some(0o600))?;

    println!(
        "{}",
        super::po::tf(
            "etcd snapshot of '{name}' written to {path} ({bytes} bytes)",
            &[
                ("name", name),
                ("path", &dest.display().to_string()),
                ("bytes", &bytes.len().to_string()),
            ],
        )
    );
    Ok(())
}

/// `cluster restore <name> --from <path>` — **DESTRUCTIVE**. Stops the
/// control-plane's etcd static pod, moves its data directory aside (never
/// deletes it — the operator confirms health before cleaning it up by
/// hand, the same caution this codebase already applies to `volumes rm`),
/// restores the snapshot into a fresh data directory under the member's
/// OWN identity (read back from its own manifest, never guessed — see
/// [`parse_etcd_member_identity`]), and brings the static pod back.
///
/// Refuses above a single `-cpN` control-plane (see the module doc
/// comment).
///
/// **Not validated against a real cluster** in the session that wrote
/// this. The sequence mirrors Kubernetes' own documented etcd
/// disaster-recovery procedure but has not been exercised end to end here —
/// test it against a disposable cluster first.
pub(crate) fn restore(name: &str, from: &Path) -> Result<()> {
    let (target, cp_count) = resolve_cp1(name)?;
    if cp_count > 1 {
        return Err(Error::Invalid(super::po::tf(
            "cluster '{name}' has {n} control-plane nodes — `cluster restore` only supports a \
             single stacked control-plane today; restoring one member of a multi-member \
             cluster needs a correct --initial-cluster for every OTHER member too, which this \
             does not attempt",
            &[("name", name), ("n", &cp_count.to_string())],
        )));
    }

    let snapshot = std::fs::read(from).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf("reading {path}", &[("path", &from.display().to_string())]),
        ))
    })?;
    if snapshot.is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "{path} is empty — refusing to restore from it",
            &[("path", &from.display().to_string())],
        )));
    }

    let manifest = remote::ssh_run(&target, "cat /etc/kubernetes/manifests/etcd.yaml")?;
    let identity = parse_etcd_member_identity(&manifest)?;

    let (_etcd_bin, etcdctl_bin) = super::etcd::download_and_cache_etcd(&identity.version)?;
    let id = delonix_node::generate_id();
    let remote_etcdctl = format!("/tmp/delonix-etcdctl-{id}");
    let remote_snap = format!("/tmp/delonix-etcd-restore-{id}.db");

    remote::scp_to(&target, &etcdctl_bin, &remote_etcdctl)?;
    // Pushed as the SSH-connecting user (never root), to a file CREATED
    // with a tight mode BEFORE scp ever touches it — OpenSSH's scp opens
    // and writes an EXISTING destination rather than unlinking and
    // recreating it, so the file is never briefly world-readable the way a
    // destination scp itself creates (subject to the remote umask) would
    // be. Only root (via the sudo'd script below) and the SSH user who
    // just wrote it can ever read the cluster's entire keyspace this way —
    // same property `backup` gets for free by never writing a remote file
    // outside one root-owned command.
    remote::ssh_run_as_user(&target, &format!("umask 077 && : > {remote_snap}"))?;
    remote::scp_to(&target, from, &remote_snap)?;

    let script = format!(
        "set -e; \
         chmod +x {remote_etcdctl}; \
         mv /etc/kubernetes/manifests/etcd.yaml /etc/kubernetes/etcd.yaml.bak-{id}; \
         sleep 20; \
         if [ -d {data_dir} ]; then mv {data_dir} {data_dir}.bak-{id}; fi; \
         ETCDCTL_API=3 {remote_etcdctl} snapshot restore {remote_snap} \
           --data-dir={data_dir} \
           --name={member_name} \
           --initial-cluster={initial_cluster} \
           --initial-advertise-peer-urls={peer_urls}; \
         mv /etc/kubernetes/etcd.yaml.bak-{id} /etc/kubernetes/manifests/etcd.yaml",
        data_dir = identity.data_dir,
        member_name = identity.name,
        initial_cluster = identity.initial_cluster,
        peer_urls = identity.peer_urls,
    );
    remote::ssh_run(&target, &script)?;

    let health_cmd = format!(
        "ETCDCTL_API=3 {remote_etcdctl} \
           --endpoints=https://127.0.0.1:2379 \
           --cacert={KUBEADM_ETCD_CA_PATH} \
           --cert={KUBEADM_ETCD_CLIENT_CERT_PATH} \
           --key={KUBEADM_ETCD_CLIENT_KEY_PATH} \
           endpoint health"
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    let healthy = loop {
        if remote::ssh_check(&target, &health_cmd) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_secs(3));
    };
    let _ = remote::ssh_run(&target, &format!("rm -f {remote_snap} {remote_etcdctl}"));

    if !healthy {
        return Err(Error::Invalid(super::po::tf(
            "restored etcd on '{name}' but it did not report healthy within 90s — the OLD data \
             directory was preserved at {data_dir}.bak-{id} on the node; investigate before \
             deciding what to do next",
            &[
                ("name", name),
                ("data_dir", &identity.data_dir),
                ("id", &id),
            ],
        )));
    }

    println!(
        "{}",
        super::po::tf(
            "etcd on '{name}' restored from {path} and reports healthy. The previous data \
             directory was preserved on the node at {data_dir}.bak-{id} — remove it by hand \
             once you've confirmed everything else looks right.",
            &[
                ("name", name),
                ("path", &from.display().to_string()),
                ("data_dir", &identity.data_dir),
                ("id", &id),
            ],
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: etcd-cp1
  namespace: kube-system
spec:
  containers:
  - command:
    - etcd
    - --advertise-client-urls=https://10.0.1.5:2379
    - --cert-file=/etc/kubernetes/pki/etcd/server.crt
    - --client-cert-auth=true
    - --data-dir=/var/lib/etcd
    - --initial-advertise-peer-urls=https://10.0.1.5:2380
    - --initial-cluster=cp1=https://10.0.1.5:2380
    - --key-file=/etc/kubernetes/pki/etcd/server.key
    - --listen-client-urls=https://127.0.0.1:2379,https://10.0.1.5:2379
    - --name=cp1
    - --peer-cert-file=/etc/kubernetes/pki/etcd/peer.crt
    - --peer-trusted-ca-file=/etc/kubernetes/pki/etcd/ca.crt
    - --trusted-ca-file=/etc/kubernetes/pki/etcd/ca.crt
    image: registry.k8s.io/etcd:3.5.16-0
    name: etcd
"#;

    #[test]
    fn parses_a_real_kubeadm_etcd_manifest() {
        let id = parse_etcd_member_identity(FIXTURE).expect("should parse");
        assert_eq!(id.name, "cp1");
        assert_eq!(id.initial_cluster, "cp1=https://10.0.1.5:2380");
        assert_eq!(id.peer_urls, "https://10.0.1.5:2380");
        assert_eq!(id.data_dir, "/var/lib/etcd");
        assert_eq!(id.version, "3.5.16-0");
    }

    /// `--peer-trusted-ca-file=`/`--trusted-ca-file=` share no prefix with
    /// `--data-dir=`/`--name=`/etc — guards against `flag_value`'s
    /// `strip_prefix` matching a longer flag's tail by accident.
    #[test]
    fn does_not_confuse_similarly_named_flags() {
        let id = parse_etcd_member_identity(FIXTURE).expect("should parse");
        assert_eq!(id.data_dir, "/var/lib/etcd");
    }

    #[test]
    fn refuses_a_manifest_missing_a_required_flag() {
        let broken = FIXTURE.replace("- --name=cp1\n", "");
        let err = parse_etcd_member_identity(&broken).expect_err("should refuse");
        assert!(err.to_string().contains("--name="), "{err}");
    }

    #[test]
    fn refuses_blind_rather_than_guessing_an_empty_manifest() {
        assert!(parse_etcd_member_identity("").is_err());
    }

    #[test]
    fn refuses_a_manifest_without_an_etcd_image_tag() {
        let broken = FIXTURE.replace("image: registry.k8s.io/etcd:3.5.16-0\n", "");
        let err = parse_etcd_member_identity(&broken).expect_err("should refuse");
        assert!(err.to_string().contains("image tag"), "{err}");
    }
}

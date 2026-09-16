//! Bootstraps a dedicated etcd cluster (`etcd.mode: external`) — delonix's
//! own CA + one leaf per member (reused for both peer and client/server TLS)
//! plus one `apiserver-etcd-client` leaf for kube-apiserver, installs+starts
//! `etcd` on every member in parallel, waits for quorum. Mirrors `cmd::lb`'s
//! shape (a small module owning one piece of per-cluster dynamic state,
//! delivered to remote hosts via the established local-tmpfile, `scp_to`,
//! privileged `ssh_run` idiom — see `prepare_host`/`lb::ensure_haproxy`).
//!
//! v1 scope (see AGENTS.md): fresh-cluster bootstrap only — no member
//! add/remove after this runs, no cert rotation. `bootstrap_etcd_cluster` is
//! only ever called once per `cluster apply`/`cluster kubeadm` run.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use delonix_runtime_core::{Error, Result};

use super::cluster::valid_endpoint;
use super::pki::{self, LeafMaterial};
use super::remote::{self, SshTarget};
use super::util::state_root;
use super::vmimage::{cpu_has_x86_64_v3, hex_sha256_file, http_get_text, stream_download};

/// Pinned etcd release — kubeadm's own recommended etcd line for the k8s
/// versions this project's golden images target (1.34/1.35) is 3.5.x.
/// **Flagged, not assumed**: confirmed live against `etcd-io/etcd`'s GitHub
/// releases that this exact tag/asset exists (2026-07-26); a future k8s
/// version needing a different etcd version is a manual follow-up, same
/// accepted v1 simplification `k8s_recipes::k8s_repo_version`'s hardcoded
/// `stable:/v1.31` fallback already uses.
const DEFAULT_ETCD_VERSION: &str = "3.5.17";

pub(crate) struct EtcdBootstrapResult {
    pub endpoints: Vec<String>,
    pub pki_dir: PathBuf,
}

/// `<root>/clusters/<name>/etcd/` — extends the existing per-cluster
/// subdirectory convention (`<root>/clusters/<name>/id_ed25519`).
fn pki_dir(cluster_name: &str) -> PathBuf {
    state_root()
        .join("clusters")
        .join(cluster_name)
        .join("etcd")
}

/// Writes `contents` to `<dir>/<name>`, `0600`. `dir` must already exist
/// (`0700`, set by the caller once per bootstrap).
fn write_pki_file(dir: &Path, name: &str, contents: &str) -> Result<()> {
    let path = dir.join(name);
    std::fs::write(&path, contents).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf("writing {name}", &[("name", name)])
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Writes `contents` to a unique local tmpfile (cleaned up by the caller) —
/// same idiom `lb::ensure_haproxy` already uses for content that only needs
/// to exist long enough for one `scp_to`.
fn write_local_tmp(tag: &str, contents: &str) -> Result<PathBuf> {
    let tmp = std::env::temp_dir().join(format!(
        "delonix-etcd-{}-{}-{tag}",
        std::process::id(),
        std::process::id()
    ));
    std::fs::write(&tmp, contents).map_err(|e| {
        Error::Invalid(format!(
            "{}: {e}",
            super::po::tf("writing local temporary {tag}", &[("tag", tag)])
        ))
    })?;
    Ok(tmp)
}

/// Renders the systemd unit for one etcd member. Pure (testable). `ExecStart=`
/// is a direct argv list (systemd's own line-continuation syntax, no `sh -c`)
/// — shell metacharacters have no special meaning here at all, sidestepping
/// that class of injection entirely. Every value still comes pre-checked by
/// `valid_endpoint` (defense in depth, matching `lb::ensure_haproxy`'s own
/// treatment of `backend_ips`).
pub(crate) fn build_etcd_unit(
    member_name: &str,
    listen_ip: &str,
    initial_cluster: &str,
    token: &str,
) -> String {
    format!(
        "[Unit]\n\
         Description=etcd (delonix-managed external cluster)\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=notify\n\
         ExecStart=/usr/local/bin/etcd \\\n\
         \x20 --name={member_name} \\\n\
         \x20 --data-dir=/var/lib/etcd \\\n\
         \x20 --listen-peer-urls=https://{listen_ip}:2380 \\\n\
         \x20 --listen-client-urls=https://{listen_ip}:2379,https://127.0.0.1:2379 \\\n\
         \x20 --advertise-client-urls=https://{listen_ip}:2379 \\\n\
         \x20 --initial-advertise-peer-urls=https://{listen_ip}:2380 \\\n\
         \x20 --initial-cluster={initial_cluster} \\\n\
         \x20 --initial-cluster-token={token} \\\n\
         \x20 --initial-cluster-state=new \\\n\
         \x20 --cert-file=/etc/etcd/pki/server.crt \\\n\
         \x20 --key-file=/etc/etcd/pki/server.key \\\n\
         \x20 --peer-cert-file=/etc/etcd/pki/server.crt \\\n\
         \x20 --peer-key-file=/etc/etcd/pki/server.key \\\n\
         \x20 --trusted-ca-file=/etc/etcd/pki/ca.crt \\\n\
         \x20 --peer-trusted-ca-file=/etc/etcd/pki/ca.crt \\\n\
         \x20 --client-cert-auth=true \\\n\
         \x20 --peer-client-cert-auth=true\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         LimitNOFILE=65536\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// Pure: the tarball + `SHA256SUMS` URLs for a given etcd version. Confirmed
/// live against real `etcd-io/etcd` GitHub releases before writing this —
/// same "confirmed live, not assumed" discipline the Debian/Rocky golden-image
/// work already used (their checksum formats turned out to differ).
fn etcd_release_asset_url(version: &str) -> (String, String) {
    let base = format!("https://github.com/etcd-io/etcd/releases/download/v{version}");
    (
        format!("{base}/etcd-v{version}-linux-amd64.tar.gz"),
        format!("{base}/SHA256SUMS"),
    )
}

fn etcd_asset_name(version: &str) -> String {
    format!("etcd-v{version}-linux-amd64.tar.gz")
}

fn etcd_cache_dir(version: &str) -> PathBuf {
    state_root().join("bin").join(format!("etcd-{version}"))
}

/// Downloads (and caches) the official `etcd`/`etcdctl` binaries, verified
/// against the release's own `SHA256SUMS` — same non-negotiable as every
/// other download in this codebase (never installs an unverified binary),
/// mirroring `vmimage::download_cri_bin`'s exact pattern. Cached under
/// `<root>/bin/etcd-<version>/`, so this only downloads once per version.
fn download_and_cache_etcd(version: &str) -> Result<(PathBuf, PathBuf)> {
    let cache_dir = etcd_cache_dir(version);
    let etcd_bin = cache_dir.join("etcd");
    let etcdctl_bin = cache_dir.join("etcdctl");
    if etcd_bin.exists() && etcdctl_bin.exists() {
        return Ok((etcd_bin, etcdctl_bin));
    }
    std::fs::create_dir_all(&cache_dir)?;
    let (tarball_url, sums_url) = etcd_release_asset_url(version);
    let asset = etcd_asset_name(version);
    // Not CPU-variant-selected like `delonix`/`delonix-cri` — etcd upstream
    // only publishes one linux/amd64 asset (no `-v3` build), confirmed live.
    let _ = cpu_has_x86_64_v3();
    let tmp = cache_dir.join(&asset);
    eprintln!(
        "{}",
        super::po::tf("downloading {asset}...", &[("asset", &asset)])
    );
    stream_download(&tarball_url, &tmp)?;
    let sums = http_get_text(&sums_url)?;
    let expected = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&asset))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| {
            Error::Invalid(super::po::tf(
                "SHA256SUMS has no entry for {asset}",
                &[("asset", &asset)],
            ))
        })?
        .to_string();
    let got = hex_sha256_file(&tmp)?;
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for {asset}: expected {expected}, got {got} — download discarded",
            &[("asset", &asset), ("expected", &expected), ("got", &got)],
        )));
    }
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tmp)
        .arg("-C")
        .arg(&cache_dir)
        .status()
        .map_err(|e| {
            Error::Invalid(format!(
                "{}: {e}",
                super::po::tf("extracting {asset}", &[("asset", &asset)])
            ))
        })?;
    if !status.success() {
        return Err(Error::Invalid(super::po::tf(
            "tar -xzf {asset} failed",
            &[("asset", &asset)],
        )));
    }
    let extracted_dir = cache_dir.join(format!("etcd-v{version}-linux-amd64"));
    std::fs::rename(extracted_dir.join("etcd"), &etcd_bin)?;
    std::fs::rename(extracted_dir.join("etcdctl"), &etcdctl_bin)?;
    let _ = std::fs::remove_dir_all(&extracted_dir);
    let _ = std::fs::remove_file(&tmp);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&etcd_bin, std::fs::Permissions::from_mode(0o755))?;
        std::fs::set_permissions(&etcdctl_bin, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok((etcd_bin, etcdctl_bin))
}

/// Installs+starts etcd on one member: binaries + PKI + systemd unit, same
/// local-tmpfile -> `scp_to` (unprivileged) -> ONE privileged `ssh_run`
/// (mv + chmod + daemon-reload + enable --now) idiom as `prepare_host`/
/// `lb::ensure_haproxy`. Local tmpfiles are always cleaned up, even on
/// failure partway through.
#[allow(clippy::too_many_arguments)]
fn ensure_etcd_running(
    target: &SshTarget,
    label: &str,
    member_name: &str,
    listen_ip: &str,
    initial_cluster: &str,
    token: &str,
    ca_cert_pem: &str,
    leaf: &LeafMaterial,
    etcd_bin: &Path,
    etcdctl_bin: &Path,
) -> Result<()> {
    let unit = build_etcd_unit(member_name, listen_ip, initial_cluster, token);
    let ca_tmp = write_local_tmp("ca.crt", ca_cert_pem)?;
    let cert_tmp = write_local_tmp("server.crt", &leaf.cert_pem)?;
    let key_tmp = write_local_tmp("server.key", &leaf.key_pem)?;
    let unit_tmp = write_local_tmp("etcd.service", &unit)?;

    let result = (|| -> Result<()> {
        remote::scp_to(target, etcd_bin, "/tmp/delonix-etcd")?;
        remote::scp_to(target, etcdctl_bin, "/tmp/delonix-etcdctl")?;
        remote::scp_to(target, &ca_tmp, "/tmp/delonix-etcd-ca.crt")?;
        remote::scp_to(target, &cert_tmp, "/tmp/delonix-etcd-server.crt")?;
        remote::scp_to(target, &key_tmp, "/tmp/delonix-etcd-server.key")?;
        remote::scp_to(target, &unit_tmp, "/tmp/delonix-etcd.service")?;
        remote::ssh_run(
            target,
            "mkdir -p /var/lib/etcd /etc/etcd/pki && \
             mv /tmp/delonix-etcd /usr/local/bin/etcd && chmod +x /usr/local/bin/etcd && \
             mv /tmp/delonix-etcdctl /usr/local/bin/etcdctl && chmod +x /usr/local/bin/etcdctl && \
             mv /tmp/delonix-etcd-ca.crt /etc/etcd/pki/ca.crt && \
             mv /tmp/delonix-etcd-server.crt /etc/etcd/pki/server.crt && \
             mv /tmp/delonix-etcd-server.key /etc/etcd/pki/server.key && \
             chmod 0644 /etc/etcd/pki/ca.crt /etc/etcd/pki/server.crt && \
             chmod 0600 /etc/etcd/pki/server.key && \
             mv /tmp/delonix-etcd.service /etc/systemd/system/etcd.service && \
             systemctl daemon-reload && systemctl enable --now etcd",
        )?;
        Ok(())
    })();

    for p in [&ca_tmp, &cert_tmp, &key_tmp, &unit_tmp] {
        let _ = std::fs::remove_file(p);
    }
    result.map_err(|e| Error::Invalid(format!("[{label}] etcd: {e}")))
}

/// Polls `etcdctl endpoint health --cluster` from one member until the whole
/// cluster reports healthy (a leader has been elected — static bootstrap
/// quorum reached) or `timeout` elapses.
fn wait_for_etcd_healthy(
    target: &SshTarget,
    endpoints: &[String],
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let endpoints_csv = endpoints.join(",");
    let cmd = format!(
        "ETCDCTL_API=3 /usr/local/bin/etcdctl --endpoints={endpoints_csv} \
         --cacert=/etc/etcd/pki/ca.crt --cert=/etc/etcd/pki/server.crt --key=/etc/etcd/pki/server.key \
         endpoint health --cluster"
    );
    loop {
        if remote::ssh_check(target, &cmd) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Invalid(
                super::po::t("etcd: the cluster did not become healthy within the timeout").into(),
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Generates the CA, issues one leaf per member (reused for both peer and
/// client/server TLS) + one `apiserver-etcd-client` leaf, installs+starts
/// etcd on every member IN PARALLEL (static bootstrap needs all initial
/// members reachable together, not just faster — same `std::thread::scope`
/// idiom `apply_ssh` already uses for host prep, and the correct choice here
/// too, not just a perf win), then waits for quorum.
pub(crate) fn bootstrap_etcd_cluster(
    cluster_name: &str,
    members: &[(String, SshTarget)],
    _k8s_version: Option<&str>,
) -> Result<EtcdBootstrapResult> {
    for (label, target) in members {
        if !valid_endpoint(label) {
            return Err(Error::Invalid(super::po::tf(
                "etcd: invalid member name '{label}'",
                &[("label", label)],
            )));
        }
        if !valid_endpoint(&target.host) {
            return Err(Error::Invalid(super::po::tf(
                "etcd: invalid member ip '{ip}'",
                &[("ip", &target.host)],
            )));
        }
    }

    let dir = pki_dir(cluster_name);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }

    let ca = pki::generate_ca(&format!("{cluster_name}-etcd-ca"))?;
    write_pki_file(&dir, "ca.crt", &ca.cert_pem)?;
    write_pki_file(&dir, "ca.key", &ca.key_pem)?;

    let initial_cluster = members
        .iter()
        .map(|(label, target)| format!("{label}=https://{}:2380", target.host))
        .collect::<Vec<_>>()
        .join(",");
    let token = format!("delonix-{cluster_name}-etcd");
    let endpoints: Vec<String> = members
        .iter()
        .map(|(_, t)| format!("https://{}:2379", t.host))
        .collect();

    let (etcd_bin, etcdctl_bin) = download_and_cache_etcd(DEFAULT_ETCD_VERSION)?;

    let errors: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = members
            .iter()
            .map(|(label, target)| {
                let ca = &ca;
                let ca_cert_pem = ca.cert_pem.clone();
                let initial_cluster = initial_cluster.clone();
                let token = token.clone();
                let etcd_bin = &etcd_bin;
                let etcdctl_bin = &etcdctl_bin;
                let sans = vec![
                    target.host.clone(),
                    label.clone(),
                    "localhost".to_string(),
                    "127.0.0.1".to_string(),
                ];
                scope.spawn(move || -> Result<()> {
                    let leaf = pki::issue_leaf(ca, label, &sans)?;
                    ensure_etcd_running(
                        target,
                        label,
                        label,
                        &target.host,
                        &initial_cluster,
                        &token,
                        &ca_cert_pem,
                        &leaf,
                        etcd_bin,
                        etcdctl_bin,
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| match h.join() {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(_) => Some("etcd member thread panicked".to_string()),
            })
            .collect()
    });
    if !errors.is_empty() {
        return Err(Error::Invalid(errors.join("; ")));
    }

    let client_leaf = pki::issue_leaf(
        &ca,
        "apiserver-etcd-client",
        &["apiserver-etcd-client".to_string()],
    )?;
    write_pki_file(&dir, "apiserver-etcd-client.crt", &client_leaf.cert_pem)?;
    write_pki_file(&dir, "apiserver-etcd-client.key", &client_leaf.key_pem)?;

    wait_for_etcd_healthy(&members[0].1, &endpoints, Duration::from_secs(60))?;

    Ok(EtcdBootstrapResult {
        endpoints,
        pki_dir: dir,
    })
}

/// Re-delivers `ca.crt`/`apiserver-etcd-client.{crt,key}` to a control-plane
/// host — called before that host's `kubeadm init`/`kubeadm join
/// --control-plane` runs. Proactive rather than relying on kubeadm's own
/// `--upload-certs` redistribution for external etcd (unvalidated — see
/// AGENTS.md): correctness this way is independent of kubeadm's own
/// behavior, which is a pure follow-up question, not a blocker.
pub(crate) fn push_etcd_client_pki(target: &SshTarget, pki_dir: &Path) -> Result<()> {
    let ca = pki_dir.join("ca.crt");
    let cert = pki_dir.join("apiserver-etcd-client.crt");
    let key = pki_dir.join("apiserver-etcd-client.key");
    remote::ssh_run(target, "mkdir -p /etc/kubernetes/pki/etcd")?;
    remote::scp_to(target, &ca, "/tmp/delonix-etcd-ca.crt")?;
    remote::scp_to(target, &cert, "/tmp/delonix-apiserver-etcd-client.crt")?;
    remote::scp_to(target, &key, "/tmp/delonix-apiserver-etcd-client.key")?;
    remote::ssh_run(
        target,
        "mv /tmp/delonix-etcd-ca.crt /etc/kubernetes/pki/etcd/ca.crt && \
         mv /tmp/delonix-apiserver-etcd-client.crt /etc/kubernetes/pki/apiserver-etcd-client.crt && \
         mv /tmp/delonix-apiserver-etcd-client.key /etc/kubernetes/pki/apiserver-etcd-client.key && \
         chmod 0644 /etc/kubernetes/pki/etcd/ca.crt /etc/kubernetes/pki/apiserver-etcd-client.crt && \
         chmod 0600 /etc/kubernetes/pki/apiserver-etcd-client.key",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_etcd_unit_inclui_os_campos_essenciais() {
        let unit = build_etcd_unit(
            "etcd-1",
            "10.0.1.1",
            "etcd-1=https://10.0.1.1:2380,etcd-2=https://10.0.1.2:2380",
            "tok123",
        );
        assert!(unit.contains("--name=etcd-1"));
        assert!(unit.contains("--listen-peer-urls=https://10.0.1.1:2380"));
        assert!(unit.contains("--listen-client-urls=https://10.0.1.1:2379,https://127.0.0.1:2379"));
        assert!(unit.contains(
            "--initial-cluster=etcd-1=https://10.0.1.1:2380,etcd-2=https://10.0.1.2:2380"
        ));
        assert!(unit.contains("--initial-cluster-token=tok123"));
        assert!(unit.contains("--initial-cluster-state=new"));
        assert!(unit.contains("--cert-file=/etc/etcd/pki/server.crt"));
        assert!(unit.contains("--peer-cert-file=/etc/etcd/pki/server.crt"));
        assert!(unit.contains("--trusted-ca-file=/etc/etcd/pki/ca.crt"));
        assert!(unit.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn etcd_release_asset_url_aponta_para_o_release_real() {
        let (tarball, sums) = etcd_release_asset_url("3.5.17");
        assert_eq!(
            tarball,
            "https://github.com/etcd-io/etcd/releases/download/v3.5.17/etcd-v3.5.17-linux-amd64.tar.gz"
        );
        assert_eq!(
            sums,
            "https://github.com/etcd-io/etcd/releases/download/v3.5.17/SHA256SUMS"
        );
    }

    #[test]
    fn etcd_asset_name_bate_com_o_nome_no_url() {
        assert_eq!(etcd_asset_name("3.5.17"), "etcd-v3.5.17-linux-amd64.tar.gz");
    }
}

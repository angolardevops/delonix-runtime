//! Single-VM TCP (L4) HAProxy load balancer, auto-provisioned in front of
//! the control-plane VMs' port 6443 whenever `delonix cluster kubeadm` is
//! asked for more than 1 control-plane. TCP mode, not HTTP: apiserver TLS
//! terminates at the real apiserver, per standard kubeadm HA guidance —
//! this LB is a dumb TCP forwarder, it never sees plaintext.

use delonix_runtime_core::{Error, Result};

use super::cluster::valid_endpoint;
use super::remote::{self, SshTarget};

/// Pure, no I/O. `backend_ips` must already be `valid_endpoint`-checked by
/// the caller (defense in depth — these end up in a file pushed to a
/// remote root-owned path).
pub(crate) fn build_haproxy_cfg(backend_ips: &[String]) -> String {
    let mut cfg = String::from(
        "global\n  log /dev/log local0\n  maxconn 2000\n\n\
         defaults\n  mode tcp\n  timeout connect 5s\n  timeout client 50s\n  timeout server 50s\n\n\
         frontend kube-apiserver\n  bind *:6443\n  mode tcp\n  option tcplog\n  default_backend kube-apiserver-backend\n\n\
         backend kube-apiserver-backend\n  mode tcp\n  balance roundrobin\n  option tcp-check\n",
    );
    for (i, ip) in backend_ips.iter().enumerate() {
        cfg.push_str(&format!("  server cp{} {ip}:6443 check\n", i + 1));
    }
    cfg
}

/// Installs haproxy via apt if missing, (re)writes `/etc/haproxy/haproxy.cfg`,
/// (re)starts the service. Always rewrites + always restarts — same
/// "sequential, simple, correct" v1 tradeoff already accepted for
/// `cluster apply`'s host prep (CLAUDE.md: parallelizing it is a follow-up
/// of performance, not correctness). Safe on every re-run: haproxy is
/// stateless, `systemctl restart` is sub-second, `create_and_wait`'s VM is
/// already idempotent by name so a retry reuses the same `<name>-lb` VM.
pub(crate) fn ensure_haproxy(target: &SshTarget, backend_ips: &[String]) -> Result<()> {
    for ip in backend_ips {
        if !valid_endpoint(ip) {
            return Err(Error::Invalid(format!(
                "lb: control-plane ip inválido '{ip}' — recusado antes de entrar no haproxy.cfg"
            )));
        }
    }
    if !remote::ssh_check(target, "command -v haproxy >/dev/null 2>&1") {
        remote::ssh_run(target, "apt-get update && apt-get install -y haproxy")?;
    }
    // No existing helper writes a String's content directly to a remote
    // file — mirror `prepare_host`'s exact idiom (delonix-cri): local
    // tmpfile -> scp_to (unprivileged) -> privileged mv over ssh.
    let cfg = build_haproxy_cfg(backend_ips);
    let tmp = std::env::temp_dir().join(format!("delonix-haproxy-{}.cfg", std::process::id()));
    std::fs::write(&tmp, &cfg)
        .map_err(|e| Error::Invalid(format!("a escrever haproxy.cfg local temporário: {e}")))?;
    let scp_result = remote::scp_to(target, &tmp, "/tmp/delonix-haproxy.cfg");
    let _ = std::fs::remove_file(&tmp);
    scp_result?;
    remote::ssh_run(
        target,
        "mv /tmp/delonix-haproxy.cfg /etc/haproxy/haproxy.cfg && \
         systemctl enable --now haproxy && systemctl restart haproxy",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_haproxy_cfg_gera_frontend_tcp_na_6443() {
        let cfg = build_haproxy_cfg(&["10.0.0.1".to_string(), "10.0.0.2".to_string()]);
        assert!(cfg.contains("bind *:6443"));
        assert!(cfg.contains("mode tcp"));
        assert!(cfg.contains("balance roundrobin"));
        assert!(cfg.contains("option tcp-check"));
    }

    #[test]
    fn build_haproxy_cfg_gera_uma_linha_server_por_ip_control_plane() {
        let cfg = build_haproxy_cfg(&["10.0.0.1".into(), "10.0.0.2".into(), "10.0.0.3".into()]);
        assert!(cfg.contains("server cp1 10.0.0.1:6443 check"));
        assert!(cfg.contains("server cp2 10.0.0.2:6443 check"));
        assert!(cfg.contains("server cp3 10.0.0.3:6443 check"));
    }

    #[test]
    fn build_haproxy_cfg_com_2_backends_nao_inventa_um_terceiro() {
        let cfg = build_haproxy_cfg(&["10.0.0.1".into(), "10.0.0.2".into()]);
        assert!(!cfg.contains("server cp3"));
    }
}

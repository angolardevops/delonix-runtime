//! Execução remota via SSH/SCP — shell-out ao `ssh`/`scp` do sistema (mesmo
//! padrão já usado neste repo para `ip`/`nft`/`qemu-img`/`virt-customize`:
//! nunca reimplementar protocolos em Rust, reaproveitar o cliente do host).
//! Usado por `cmd::cluster` para o bootstrap `kubeadm` idempotente.

use std::path::{Path, PathBuf};
use std::process::Command;

use delonix_runtime_core::{Error, Result};

#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    pub user: String,
    pub key: Option<PathBuf>,
    /// SSH port. `None` = the client's default (22).
    ///
    /// `ssh.port` was in the `kind: Cluster` schema, was parsed, and reached
    /// nothing — every connection went to 22 regardless. A bastion on a
    /// non-standard port failed with a timeout that named no cause; worse, if
    /// something else answered on 22 (another service, another machine behind
    /// NAT) the bootstrap would run against the wrong host. Accepted-and-ignored
    /// is the failure mode this project refuses by policy — same family as
    /// `--security-opt seccomp=`, `-v :z` and `--network-alias`.
    pub port: Option<u16>,
}

impl SshTarget {
    /// Connection arguments shared by `ssh` and `scp`.
    ///
    /// `port_flag` is the port option **of the tool this argv is for**: `-p` for
    /// `ssh`, `-P` for `scp`. They are genuinely different, and this is the trap
    /// worth naming: `-p` handed to `scp` is not a port at all, it is "preserve
    /// modification times" — the copy would still go to 22, and the only symptom
    /// would be that it worked everywhere except where the port matters. Asking
    /// the caller makes the difference visible at each call site instead of
    /// hidden in here.
    fn conn_args(&self, port_flag: &str) -> Vec<String> {
        let mut a = vec![
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
            "-o".to_string(),
            "ConnectTimeout=10".to_string(),
        ];
        if let Some(p) = self.port {
            a.push(port_flag.to_string());
            a.push(p.to_string());
        }
        if let Some(k) = &self.key {
            a.push("-i".to_string());
            a.push(k.to_string_lossy().into_owned());
        }
        a
    }

    fn user_host(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Corre `cmd` no host, como root (`sudo -n` — o utilizador SSH tem de já
/// estar em sudoers sem password; `BatchMode=yes` recusa qualquer prompt
/// interactivo, incluindo de password). Devolve `(sucesso, stdout+stderr)`.
fn ssh_run_raw(t: &SshTarget, cmd: &str) -> Result<(bool, String)> {
    let mut args = t.conn_args("-p");
    // `--` separa opções de argumentos posicionais — defesa em profundidade
    // contra um `host` que comece por `-` ser interpretado como flag do
    // `ssh` (ex.: `-oProxyCommand=...`). `valid_endpoint` (cmd::cluster) já
    // recusa esses valores na origem; isto é a segunda camada. Achado de
    // auditoria de segurança, ver AGENTS.md.
    args.push("--".to_string());
    args.push(t.user_host());
    args.push(format!("sudo -n bash -c {}", shell_quote(cmd)));
    let out = Command::new("ssh")
        .args(&args)
        .output()
        .map_err(|e| Error::Invalid(format!("a correr ssh para {}: {e}", t.host)))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), combined))
}

/// `true` se `check_cmd` terminar com sucesso no host (condição já satisfeita).
pub fn ssh_check(t: &SshTarget, check_cmd: &str) -> bool {
    ssh_run_raw(t, check_cmd).map(|(ok, _)| ok).unwrap_or(false)
}

/// Corre `cmd`; erro claro (com o host e o output capturado) se falhar.
pub fn ssh_run(t: &SshTarget, cmd: &str) -> Result<String> {
    let (ok, out) = ssh_run_raw(t, cmd)?;
    if ok {
        Ok(out)
    } else {
        Err(Error::Invalid(format!(
            "[{}] comando falhou: {cmd}\n{out}",
            t.host
        )))
    }
}

/// Copia um ficheiro local para o host — SEM privilégio elevado (o `scp` em
/// si corre como o utilizador SSH normal; para destinos que exigem root,
/// copia para `/tmp` e move com um `ssh_run` a seguir, como faz
/// `cmd::cluster::prepare_host` para o `delonix-cri`).
pub fn scp_to(t: &SshTarget, local: &Path, remote_path: &str) -> Result<()> {
    let mut args = t.conn_args("-P");
    args.push("--".to_string());
    args.push(local.to_string_lossy().into_owned());
    args.push(format!("{}:{}", t.user_host(), remote_path));
    let status = Command::new("scp")
        .args(&args)
        .status()
        .map_err(|e| Error::Invalid(format!("a correr scp para {}: {e}", t.host)))?;
    if !status.success() {
        return Err(Error::Invalid(format!(
            "scp para {}:{remote_path} falhou",
            t.host
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{shell_quote, SshTarget};

    #[test]
    fn shell_quote_escapa_plicas() {
        assert_eq!(shell_quote("echo hi"), "'echo hi'");
        assert_eq!(shell_quote("echo 'hi'"), "'echo '\\''hi'\\'''");
    }

    fn alvo(port: Option<u16>) -> SshTarget {
        SshTarget {
            host: "10.0.0.5".into(),
            user: "delonix".into(),
            key: None,
            port,
        }
    }

    /// `ssh.port` was in the schema and reached nothing. Without a port the argv
    /// has to stay byte-for-byte what it always was — every cluster already out
    /// there omits it, and a change there would be a change for everybody.
    #[test]
    fn sem_porta_o_argv_fica_exactamente_como_estava() {
        let a = alvo(None);
        assert_eq!(
            a.conn_args("-p"),
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=10"
            ]
        );
        assert_eq!(a.conn_args("-p"), a.conn_args("-P"));
    }

    /// The one that matters: `ssh` takes `-p`, `scp` takes `-P`, and `-p` given
    /// to `scp` is "preserve modification times" — it would connect to 22 and
    /// report success, so the symptom would appear only where the port is not
    /// the default. This test is what stops the two call sites from being
    /// "simplified" back into one.
    #[test]
    fn a_flag_da_porta_do_scp_nao_e_a_do_ssh() {
        let a = alvo(Some(2222));
        assert!(a.conn_args("-p").windows(2).any(|w| w == ["-p", "2222"]));
        assert!(a.conn_args("-P").windows(2).any(|w| w == ["-P", "2222"]));
        assert!(!a.conn_args("-P").iter().any(|x| x == "-p"));
    }
}

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
}

impl SshTarget {
    fn conn_args(&self) -> Vec<String> {
        let mut a = vec![
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
            "-o".to_string(),
            "ConnectTimeout=10".to_string(),
        ];
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
    let mut args = t.conn_args();
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
        Err(Error::Invalid(format!("[{}] comando falhou: {cmd}\n{out}", t.host)))
    }
}

/// Copia um ficheiro local para o host — SEM privilégio elevado (o `scp` em
/// si corre como o utilizador SSH normal; para destinos que exigem root,
/// copia para `/tmp` e move com um `ssh_run` a seguir, como faz
/// `cmd::cluster::prepare_host` para o `delonix-cri`).
pub fn scp_to(t: &SshTarget, local: &Path, remote_path: &str) -> Result<()> {
    let mut args = t.conn_args();
    args.push(local.to_string_lossy().into_owned());
    args.push(format!("{}:{}", t.user_host(), remote_path));
    let status = Command::new("scp")
        .args(&args)
        .status()
        .map_err(|e| Error::Invalid(format!("a correr scp para {}: {e}", t.host)))?;
    if !status.success() {
        return Err(Error::Invalid(format!("scp para {}:{remote_path} falhou", t.host)));
    }
    Ok(())
}

/// Copia um ficheiro do host para o local — o `remote_path` tem de ser
/// legível pelo utilizador SSH (ex.: via `sudo chmod`/`cat` antes, se for
/// root-only; `cmd::cluster::fetch_kubeconfig` trata disso).
pub fn scp_from(t: &SshTarget, remote_path: &str, local: &Path) -> Result<()> {
    let mut args = t.conn_args();
    args.push(format!("{}:{}", t.user_host(), remote_path));
    args.push(local.to_string_lossy().into_owned());
    let status = Command::new("scp")
        .args(&args)
        .status()
        .map_err(|e| Error::Invalid(format!("a correr scp de {}: {e}", t.host)))?;
    if !status.success() {
        return Err(Error::Invalid(format!("scp de {}:{remote_path} falhou", t.host)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quote_escapa_plicas() {
        assert_eq!(shell_quote("echo hi"), "'echo hi'");
        assert_eq!(shell_quote("echo 'hi'"), "'echo '\\''hi'\\'''");
    }
}

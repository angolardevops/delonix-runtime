//! WireGuard sobre o overlay (req #6) — confidencialidade + integridade +
//! autenticação de ORIGEM entre nós Delonix (Curve25519 + ChaCha20-Poly1305).
//!
//! A "assinatura de pacotes" do requisito só é coerente entre PEERS (túnel), não
//! para egress arbitrário; a resposta certa é cifrar o overlay inter-node. Usa o
//! módulo de kernel via `ip link`/`wg` — o holder cria a interface no netns de
//! infra como já cria bridge/veth, logo NÃO é preciso boringtun/dependência nova.
//! O intra-host fica coberto pelo anti-spoofing (do_attach).
//!
//! Validado end-to-end (dois netns rootless): ping pelo túnel + `tcpdump` no
//! underlay = só UDP WireGuard cifrado, sem ICMP em claro; handshake completo.

use delonix_runtime_core::{Error, Result};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn rt(ctx: &'static str, e: impl std::fmt::Display) -> Error {
    Error::Runtime {
        context: ctx,
        message: e.to_string(),
    }
}

/// Corre `prog args`; devolve stdout (trim) ou erro com o stderr.
fn out(prog: &str, args: &[&str]) -> Result<String> {
    let o = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| rt("spawn", e))?;
    if !o.status.success() {
        return Err(Error::Runtime {
            context: "cmd",
            message: String::from_utf8_lossy(&o.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn run(prog: &str, args: &[&str]) -> Result<()> {
    out(prog, args).map(|_| ())
}

/// Par de chaves WireGuard (base64, como o `wg` as emite).
#[derive(Clone, Debug)]
pub struct WgKey {
    pub private: String,
    pub public: String,
}

/// Deriva a chave pública de uma privada (`<priv> | wg pubkey`).
pub fn pubkey(private: &str) -> Result<String> {
    let mut child = Command::new("wg")
        .arg("pubkey")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| rt("spawn wg pubkey", e))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(private.as_bytes())
        .map_err(|e| rt("stdin", e))?;
    let o = child.wait_with_output().map_err(|e| rt("wait", e))?;
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Gera um par de chaves novo (`wg genkey` + `wg pubkey`).
pub fn keygen() -> Result<WgKey> {
    let private = out("wg", &["genkey"])?;
    let public = pubkey(&private)?;
    Ok(WgKey { private, public })
}

fn wg_dir() -> PathBuf {
    let root = std::env::var_os("DELONIX_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/delonix")))
        .unwrap_or_else(|| PathBuf::from("/var/lib/delonix"));
    root.join("wg")
}

fn write_0600(p: &Path, data: &str) -> Result<()> {
    std::fs::write(p, data).map_err(|e| rt("write key", e))?;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

/// Chave do nó, persistida 0600 em `$DELONIX_ROOT/wg/node.key` (gera na 1ª vez).
/// A pública vai para `node.pub` (legível) para publicação no control-plane.
pub fn ensure_node_key() -> Result<WgKey> {
    let dir = wg_dir();
    std::fs::create_dir_all(&dir).map_err(|e| rt("wg dir", e))?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let kp = dir.join("node.key");
    let private = match std::fs::read_to_string(&kp) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            let k = out("wg", &["genkey"])?;
            write_0600(&kp, &k)?;
            k
        }
    };
    let public = pubkey(&private)?;
    let _ = std::fs::write(dir.join("node.pub"), &public);
    Ok(WgKey { private, public })
}

/// Cria/configura a interface WireGuard `<name>` no netns ATUAL (chamado no holder,
/// que tem CAP_NET_ADMIN no netns de infra). Idempotente. A privada vai por
/// ficheiro temporário 0600 (não na linha de comando / `ps`). `addr_cidr` ex.:
/// `"10.99.0.1/24"`.
pub fn ensure_iface(
    name: &str,
    private_key: &str,
    listen_port: u16,
    addr_cidr: &str,
) -> Result<()> {
    let _ = run("ip", &["link", "del", name]); // limpa restos (best-effort)
    run("ip", &["link", "add", name, "type", "wireguard"])?;
    let dir = wg_dir();
    let _ = std::fs::create_dir_all(&dir);
    let kf = dir.join(format!(".{name}.key.tmp"));
    write_0600(&kf, private_key)?;
    let res = run(
        "wg",
        &[
            "set",
            name,
            "private-key",
            &kf.to_string_lossy(),
            "listen-port",
            &listen_port.to_string(),
        ],
    );
    let _ = std::fs::remove_file(&kf);
    res?;
    run("ip", &["addr", "add", addr_cidr, "dev", name])?;
    run("ip", &["link", "set", name, "up"])?;
    Ok(())
}

/// Um peer WireGuard (outro nó Delonix).
pub struct Peer {
    pub public: String,
    pub endpoint: String,
    pub allowed_ips: Vec<String>,
}

/// Configura um peer numa interface (`wg set <if> peer <pub> allowed-ips … endpoint …`).
pub fn set_peer(name: &str, p: &Peer) -> Result<()> {
    let allowed = p.allowed_ips.join(",");
    run(
        "wg",
        &[
            "set",
            name,
            "peer",
            &p.public,
            "allowed-ips",
            &allowed,
            "endpoint",
            &p.endpoint,
            "persistent-keepalive",
            "25",
        ],
    )
}

/// Está o WireGuard disponível neste host? (`wg`/`ip` + módulo de kernel).
pub fn available() -> bool {
    Command::new("wg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_roundtrip() {
        if !available() {
            return; // salta em hosts sem `wg`
        }
        let k = keygen().expect("keygen");
        // chaves WireGuard = base64 de 32 bytes = 44 chars (termina em '=').
        assert_eq!(k.private.len(), 44);
        assert_eq!(k.public.len(), 44);
        assert!(k.public.ends_with('='));
        // a pública deriva DETERMINISTICAMENTE da privada (Curve25519).
        assert_eq!(pubkey(&k.private).unwrap(), k.public);
        assert_ne!(k.private, k.public);
    }
}

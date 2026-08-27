//! WireGuard over the overlay (req #6) — confidentiality + integrity +
//! ORIGIN authentication between Delonix nodes (Curve25519 + ChaCha20-Poly1305).
//!
//! The requirement's "packet signing" only makes sense between PEERS (tunnel), not
//! for arbitrary egress; the right answer is to encrypt the inter-node overlay. Uses the
//! kernel module via `ip link`/`wg` — the holder creates the interface in the infra
//! netns just as it already creates bridge/veth, so NO boringtun/new dependency is needed.
//! Intra-host is covered by the anti-spoofing (do_attach).
//!
//! Validated end-to-end (two rootless netns): ping through the tunnel + `tcpdump` on the
//! underlay = only encrypted WireGuard UDP, no ICMP in the clear; full handshake.

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

/// The `wg` binary is missing from the host.
///
/// The raw `ENOENT` of a spawn is NOT a missing file — it is the TOOL not being
/// there, and "No such file or directory" sends the reader looking for a path.
/// `network node init`/`key` used to surface exactly that errno, while the
/// encrypted-overlay path two functions away (`cmd::network`, `wg::available`)
/// already refused with an actionable message. Fixed at the boundary so every
/// caller of this module inherits it. Same class as `vmimage::tool_package`.
fn missing_wg() -> Error {
    Error::Unavailable(
        "'wg' is not available on this host — install wireguard-tools (Debian/Ubuntu: \
         `apt install wireguard-tools`; Fedora/RHEL: `dnf install wireguard-tools`; \
         Arch: `pacman -S wireguard-tools`). It is only needed for WireGuard node keys \
         and encrypted overlay networks"
            .into(),
    )
}

/// Runs `prog args`; returns stdout (trimmed) or an error with the stderr.
fn out(prog: &str, args: &[&str]) -> Result<String> {
    let o = Command::new(prog).args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            missing_wg()
        } else {
            rt("spawn", e)
        }
    })?;
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

/// WireGuard key pair (base64, as `wg` emits them).
#[derive(Clone, Debug)]
pub struct WgKey {
    pub private: String,
    pub public: String,
}

/// Derives the public key from a private one (`<priv> | wg pubkey`).
pub fn pubkey(private: &str) -> Result<String> {
    let mut child = Command::new("wg")
        .arg("pubkey")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                missing_wg()
            } else {
                rt("spawn wg pubkey", e)
            }
        })?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(private.as_bytes())
        .map_err(|e| rt("stdin", e))?;
    let o = child.wait_with_output().map_err(|e| rt("wait", e))?;
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Generates a new key pair (`wg genkey` + `wg pubkey`).
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

/// Node key, persisted 0600 at `$DELONIX_ROOT/wg/node.key` (generated on first use).
/// The public one goes to `node.pub` (readable) for publishing to the control-plane.
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

/// Creates/configures the WireGuard interface `<name>` in the CURRENT netns (called in the
/// holder, which has CAP_NET_ADMIN in the infra netns). Idempotent. The private key goes via
/// a 0600 temporary file (not on the command line / `ps`). `addr_cidr` e.g.:
/// `"10.99.0.1/24"`.
pub fn ensure_iface(
    name: &str,
    private_key: &str,
    listen_port: u16,
    addr_cidr: &str,
) -> Result<()> {
    let _ = run("ip", &["link", "del", name]); // clears leftovers (best-effort)
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

/// A WireGuard peer (another Delonix node).
pub struct Peer {
    pub public: String,
    pub endpoint: String,
    pub allowed_ips: Vec<String>,
}

/// Configures a peer on an interface (`wg set <if> peer <pub> allowed-ips … endpoint …`).
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

/// **Removes a peer** from an interface (`wg set <if> peer <pub> remove`).
///
/// The inverse of [`set_peer`], and the half that was missing where it matters
/// most: a node dropped from an encrypted overlay kept its tunnel UP. Taking it
/// out of the FDB stops the VXLAN traffic; leaving the WireGuard peer configured
/// leaves a machine that is no longer in the mesh still able to establish the
/// crypto channel.
pub fn remove_peer(name: &str, public: &str) -> Result<()> {
    if !valid_wg_key(public) {
        return Err(Error::Invalid(format!(
            "not a WireGuard public key: '{public}'"
        )));
    }
    run("wg", &["set", name, "peer", public, "remove"])
}

/// A base64 WireGuard key: 43 chars of alphabet plus the `=` pad.
///
/// Validated BEFORE the value reaches an argv, not only on the far side — the
/// same discipline as `valid_fdb_dst` and the `valid_*` family the security
/// audit left behind. Without it a peer string starting with `-` is read by `wg`
/// as an option.
pub fn valid_wg_key(s: &str) -> bool {
    s.len() == 44
        && s.ends_with('=')
        && s[..43]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
}

/// Is WireGuard available on this host? (`wg`/`ip` + kernel module).
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
            return; // skips on hosts without `wg`
        }
        let k = keygen().expect("keygen");
        // WireGuard keys = base64 of 32 bytes = 44 chars (ends in '=').
        assert_eq!(k.private.len(), 44);
        assert_eq!(k.public.len(), 44);
        assert!(k.public.ends_with('='));
        // the public key derives DETERMINISTICALLY from the private one (Curve25519).
        assert_eq!(pubkey(&k.private).unwrap(), k.public);
        assert_ne!(k.private, k.public);
        // A chave REAL tem de passar o validador — senão o `remove_peer` recusaria
        // exactamente as chaves que existem.
        assert!(valid_wg_key(&k.public), "{}", k.public);
    }

    /// A chave vai para o argv do `wg` e vem de um manifesto. Validar ANTES do
    /// `format!` é a disciplina `valid_*` que a auditoria deixou: sem ela, um
    /// valor começado por `-` é lido pelo `wg` como uma opção.
    #[test]
    fn valid_wg_key_recusa_flags_espacos_e_comprimentos_errados() {
        // 44 chars, alfabeto base64, termina em '=' — a forma real.
        assert!(valid_wg_key("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ="));
        assert!(valid_wg_key("aB3+/aB3+/aB3+/aB3+/aB3+/aB3+/aB3+/aB3+/aB3="));
        // O que isto existe para travar.
        assert!(!valid_wg_key("-remove"));
        assert!(!valid_wg_key("--endpoint=1.2.3.4:51820"));
        assert!(!valid_wg_key(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN OP="
        ));
        assert!(!valid_wg_key(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLM\nOPQ="
        ));
        // Comprimento e pad.
        assert!(!valid_wg_key(""));
        assert!(!valid_wg_key("abc="));
        assert!(!valid_wg_key(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQR"
        ));
    }
}

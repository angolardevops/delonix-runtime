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

/// Runs `prog args`; returns stdout (trimmed) or an error with the stderr.
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
    }
}

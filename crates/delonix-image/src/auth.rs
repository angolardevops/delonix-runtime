//! Credenciais de registos (`delonix login`/`logout`), formato compatível com
//! o `~/.docker/config.json` / `auth.json` do Podman: `{ "auths": { "<host>":
//! { "auth": "base64(user:password)" } } }`. Guardado em `<root>/auth.json`.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use delonix_core::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default)]
struct AuthFile {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
}

#[derive(Serialize, Deserialize, Default)]
struct AuthEntry {
    #[serde(default)]
    auth: String,
}

fn auth_path(root: &Path) -> PathBuf {
    root.join("auth.json")
}

/// Normaliza o nome do registo para a chave canónica (host da API V2). `docker.io`,
/// `index.docker.io` e vazio → `registry-1.docker.io`.
pub fn canonical_host(registry: &str) -> String {
    match registry {
        "" | "docker.io" | "index.docker.io" | "registry-1.docker.io" => {
            "registry-1.docker.io".to_string()
        }
        other => other.to_string(),
    }
}

fn read(root: &Path) -> AuthFile {
    std::fs::read(auth_path(root))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Guarda as credenciais de `registry` (codificadas em base64). 0600.
pub fn login(root: &Path, registry: &str, user: &str, password: &str) -> Result<()> {
    let host = canonical_host(registry);
    let mut file = read(root);
    let token = B64.encode(format!("{user}:{password}"));
    file.auths.insert(host.clone(), AuthEntry { auth: token });
    write(root, &file)?;
    Ok(())
}

/// Remove as credenciais de `registry`. Devolve `true` se existiam.
pub fn logout(root: &Path, registry: &str) -> Result<bool> {
    let host = canonical_host(registry);
    let mut file = read(root);
    let existed = file.auths.remove(&host).is_some();
    if existed {
        write(root, &file)?;
    }
    Ok(existed)
}

fn write(root: &Path, file: &AuthFile) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(file)?;
    let path = auth_path(root);
    std::fs::write(&path, bytes)?;
    // permissões 0600 — credenciais não devem ser legíveis por outros.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Devolve `(user, password)` guardados para `host`, se existirem.
pub fn lookup(root: &Path, host: &str) -> Option<(String, String)> {
    let file = read(root);
    let entry = file.auths.get(&canonical_host(host))?;
    let decoded = B64.decode(entry.auth.as_bytes()).ok()?;
    let pair = String::from_utf8(decoded).ok()?;
    let (u, p) = pair.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

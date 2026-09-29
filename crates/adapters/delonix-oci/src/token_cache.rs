//! Registry bearer tokens kept on disk between commands — only the ones issued
//! WITHOUT credentials (ADR-0060).
//!
//! Every read command used to start from nothing: a `401`, a request to the
//! token service, and only then the request it wanted. Measured against Docker
//! Hub, the first two are about 1 s of a ~1.9 s warm pull. A token the service
//! hands out anonymously names nobody (`sub` is empty) and grants `pull` on one
//! public repository for a few minutes, so keeping it on disk exposes nothing a
//! stranger cannot fetch with the same request. A token obtained WITH
//! credentials can read private repositories, and never comes here: the client
//! only holds a cache root when it has no credentials for the host.
//!
//! Everything in this module can fail without failing the command: a missing,
//! unreadable, corrupt or expired entry is a miss, and a write that fails is
//! logged at `debug`. The cache can save a request; it can never cost one.

use std::path::{Path, PathBuf};

/// How much earlier than its stated expiry an entry stops being used. It also
/// covers the difference between the token service's clock and this host's,
/// and the time the response took to arrive (the expiry is counted from when
/// it was received, not from the service's `issued_at`).
const MARGIN_SECS: u64 = 30;

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    token: String,
    expires_unix: u64,
}

/// The one scope this cache keys on: a read of `repo`. A token is only stored
/// when the registry's challenge asked for exactly this scope, so every stored
/// entry is one a later read can find.
pub(crate) fn pull_scope(repo: &str) -> String {
    format!("repository:{repo}:pull")
}

fn dir(root: &Path, host: &str) -> PathBuf {
    let host: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':') {
                c
            } else {
                '_'
            }
        })
        .collect();
    root.join("auth").join("tokens").join(host)
}

fn entry_path(root: &Path, host: &str, scope: &str) -> PathBuf {
    dir(root, host).join(format!("{}.json", crate::cas::sha256_hex(scope.as_bytes())))
}

/// The cached token for `(host, scope)`, if one is there and still valid at
/// `now`. Anything else — no file, a file that does not parse, an expired
/// entry — is `None`.
pub(crate) fn load(root: &Path, host: &str, scope: &str, now: u64) -> Option<String> {
    let bytes = std::fs::read(entry_path(root, host, scope)).ok()?;
    let entry: Entry = serde_json::from_slice(&bytes).ok()?;
    (now < entry.expires_unix && !entry.token.is_empty()).then_some(entry.token)
}

/// Stores `token` for `(host, scope)` until `expires_unix`, mode `0600`, and
/// removes this host's entries that have already expired. Best effort.
pub(crate) fn store(
    root: &Path,
    host: &str,
    scope: &str,
    token: &str,
    expires_unix: u64,
    now: u64,
) {
    let d = dir(root, host);
    if let Err(e) = create_private_dir(&d) {
        tracing::debug!("token cache: cannot create {}: {e}", d.display());
        return;
    }
    prune_expired(&d, now);
    let entry = Entry {
        token: token.to_string(),
        expires_unix,
    };
    let bytes = match serde_json::to_vec(&entry) {
        Ok(b) => b,
        Err(_) => return,
    };
    let path = entry_path(root, host, scope);
    if let Err(e) = delonix_node::write_atomic_mode(&path, &bytes, Some(0o600)) {
        tracing::debug!("token cache: cannot write {}: {e}", path.display());
    }
}

/// Drops the entry for `(host, scope)` — a cached token the registry has just
/// refused must not be offered again.
pub(crate) fn forget(root: &Path, host: &str, scope: &str) {
    let _ = std::fs::remove_file(entry_path(root, host, scope));
}

/// When a token received at `now` stops being usable: the earlier of the JWT
/// `exp` claim and `now + expires_in`, minus [`MARGIN_SECS`]. `None` when the
/// token says neither, or when what is left after the margin is nothing — a
/// token whose lifetime cannot be read is not cached.
pub(crate) fn expiry(token: &str, expires_in: Option<u64>, now: u64) -> Option<u64> {
    let from_jwt = jwt_exp(token);
    let from_response = expires_in.map(|e| now.saturating_add(e));
    let until = match (from_jwt, from_response) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => return None,
    };
    until.checked_sub(MARGIN_SECS).filter(|&t| t > now)
}

/// The `exp` claim of a JWT, read without verifying the signature — only the
/// registry needs to trust it; here it decides how long to keep it.
fn jwt_exp(token: &str) -> Option<u64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    claims.get("exp")?.as_u64()
}

fn create_private_dir(d: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(d)?;
    let tokens = d.parent().unwrap_or(d);
    for p in [tokens, d] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn prune_expired(d: &Path, now: u64) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let expired = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Entry>(&b).ok())
            .is_none_or(|entry| entry.expires_unix <= now);
        if expired {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(exp: u64) -> String {
        use base64::Engine as _;
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            enc.encode(br#"{"alg":"RS256"}"#),
            enc.encode(format!(r#"{{"sub":"","exp":{exp}}}"#))
        )
    }

    #[test]
    fn expiry_takes_the_earlier_bound_minus_the_margin() {
        // JWT says 1300, the response says now+300 = 1300: 1270.
        assert_eq!(expiry(&jwt(1300), Some(300), 1000), Some(1270));
        // The JWT is earlier than the response.
        assert_eq!(expiry(&jwt(1100), Some(300), 1000), Some(1070));
        // An opaque token (not a JWT) is kept for what the response says.
        assert_eq!(expiry("opaque", Some(300), 1000), Some(1270));
        // Only the JWT.
        assert_eq!(expiry(&jwt(1300), None, 1000), Some(1270));
    }

    #[test]
    fn a_token_whose_lifetime_cannot_be_read_is_not_cached() {
        assert_eq!(expiry("opaque", None, 1000), None);
        // Nothing left once the margin is taken.
        assert_eq!(expiry(&jwt(1020), None, 1000), None);
        assert_eq!(expiry("opaque", Some(10), 1000), None);
    }

    #[test]
    fn a_stored_token_comes_back_until_it_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = pull_scope("library/alpine");
        store(
            tmp.path(),
            "registry-1.docker.io",
            &scope,
            "tok",
            1270,
            1000,
        );
        assert_eq!(
            load(tmp.path(), "registry-1.docker.io", &scope, 1269).as_deref(),
            Some("tok")
        );
        assert_eq!(load(tmp.path(), "registry-1.docker.io", &scope, 1270), None);
        // Another repository, and another host, are other entries.
        assert_eq!(
            load(
                tmp.path(),
                "registry-1.docker.io",
                &pull_scope("library/node"),
                1100
            ),
            None
        );
        assert_eq!(load(tmp.path(), "ghcr.io", &scope, 1100), None);
    }

    #[test]
    fn a_corrupt_entry_is_a_miss_and_forget_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = pull_scope("x");
        let path = entry_path(tmp.path(), "h", &scope);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load(tmp.path(), "h", &scope, 0), None);
        store(tmp.path(), "h", &scope, "tok", 500, 100);
        assert!(load(tmp.path(), "h", &scope, 200).is_some());
        forget(tmp.path(), "h", &scope);
        assert!(!path.exists());
    }

    #[test]
    fn entries_are_private_and_expired_ones_are_pruned_on_write() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        store(tmp.path(), "h", &pull_scope("old"), "a", 150, 100);
        store(tmp.path(), "h", &pull_scope("new"), "b", 900, 200);
        let old = entry_path(tmp.path(), "h", &pull_scope("old"));
        assert!(
            !old.exists(),
            "an entry expired at 150 must be gone after a write at 200"
        );
        let new = entry_path(tmp.path(), "h", &pull_scope("new"));
        let mode = std::fs::metadata(&new).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = std::fs::metadata(new.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700);
    }
}

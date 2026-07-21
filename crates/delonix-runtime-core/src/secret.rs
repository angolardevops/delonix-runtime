//! Secret Manager: named secrets (key→value pairs) injected as environment
//! variables into containers. Persisted one file per secret under
//! `<root>/secrets/<name>.json` with **0600** permissions and **encrypted at-rest**
//! (XChaCha20-Poly1305, via [`crate::cred_vault::CredVault`], host master key).
//! Plaintext files from previous versions remain readable (backward-compatible);
//! on re-write they become encrypted. Values must never go to logs/inspect in cleartext.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cred_vault::CredVault;
use crate::{Error, Result};

/// A named secret: a set of `KEY=value` pairs (env).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Secret {
    /// Secret name (reference used by `run --secret <name>`).
    pub name: String,
    /// key→value pairs. The keys are environment variable names.
    #[serde(default)]
    pub data: BTreeMap<String, String>,
    /// Creation/update instant (Unix seconds).
    #[serde(default)]
    pub updated_unix: u64,
}

impl Secret {
    /// The pairs in `KEY=value` format for injection as env.
    pub fn env_pairs(&self) -> Vec<String> {
        self.data.iter().map(|(k, v)| format!("{k}={v}")).collect()
    }
}

/// Is an environment variable name valid? (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn valid_env_key(k: &str) -> bool {
    let mut it = k.chars();
    matches!(it.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Valid secret name? (`[a-z0-9._-]`, non-empty, ≤ 64).
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Header of the encrypted files (distinguishes from the legacy plaintext JSON format).
const SEALED_MAGIC: &[u8] = b"DLXSEC1\n";

/// Secret store under `<root>/secrets`, with 0600 files **encrypted at-rest**
/// (XChaCha20-Poly1305 via [`CredVault`], host master key). Old plaintext
/// files remain readable (backward-compatible).
pub struct SecretStore {
    root: PathBuf,
    vault: CredVault,
}

impl SecretStore {
    /// Opens (creating) the store. `base` = `$DELONIX_ROOT` (the `secrets` dir is created there).
    /// Reuses the host master key (the same as [`CredVault`]) to encrypt the values.
    pub fn open(base: impl Into<PathBuf>) -> Result<Self> {
        let base = base.into();
        let root = base.join("secrets");
        fs::create_dir_all(&root)?;
        // the directory itself is 0700 (not listable by others).
        let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        let vault = CredVault::open(&base)?;
        Ok(Self { root, vault })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.json"))
    }

    /// Decodes the bytes of a file: encrypted (with header) or legacy (plaintext JSON).
    fn decode(&self, bytes: &[u8]) -> Result<Secret> {
        if let Some(sealed) = bytes.strip_prefix(SEALED_MAGIC) {
            Ok(serde_json::from_slice(&self.vault.unseal(sealed)?)?)
        } else {
            Ok(serde_json::from_slice(bytes)?)
        }
    }

    /// Persists a secret (atomic write + chmod 0600).
    pub fn save(&self, s: &Secret) -> Result<()> {
        if !valid_name(&s.name) {
            return Err(Error::Invalid(format!("invalid secret name: {:?}", s.name)));
        }
        for k in s.data.keys() {
            if !valid_env_key(k) {
                return Err(Error::Invalid(format!("invalid env key: {k:?}")));
            }
        }
        // value encrypted at-rest: header || nonce || ciphertext.
        let mut blob = Vec::from(SEALED_MAGIC);
        blob.extend_from_slice(&self.vault.seal(&serde_json::to_vec(s)?)?);
        let tmp = self.root.join(format!(".{}.tmp", s.name));
        fs::write(&tmp, &blob)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, self.path(&s.name))?;
        Ok(())
    }

    /// Loads a secret by name.
    pub fn load(&self, name: &str) -> Result<Secret> {
        let p = self.path(name);
        if !p.exists() {
            return Err(Error::NotFound(format!("secret {name}")));
        }
        self.decode(&fs::read(p)?)
    }

    /// Lists the names of the existing secrets.
    pub fn list(&self) -> Vec<Secret> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("json") {
                    if let Ok(s) = self.decode(&fs::read(&p).unwrap_or_default()) {
                        out.push(s);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Removes a secret.
    pub fn remove(&self, name: &str) -> Result<()> {
        let p = self.path(name);
        if !p.exists() {
            return Err(Error::NotFound(format!("secret {name}")));
        }
        fs::remove_file(p)?;
        Ok(())
    }

    /// Resolves a list of secret names into their `KEY=value` pairs (env),
    /// in the given order (later ones override earlier ones on the same key).
    /// Nonexistent names are ignored (best-effort at startup).
    pub fn resolve_env(&self, names: &[String]) -> Vec<String> {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        for n in names {
            if let Ok(s) = self.load(n) {
                for (k, v) in s.data {
                    env.insert(k, v);
                }
            }
        }
        env.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
    }

    /// Materializes the secrets as **files** in `dir` (one per key, 0600;
    /// `dir` 0700) — for injection via bind-mount into `/run/secrets` (Pillar 5 Block B),
    /// instead of environment variables (which leak in `environ`/`inspect`). Idempotent.
    /// Nonexistent names → error (the caller validated beforehand).
    pub fn materialize(&self, names: &[String], dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        for n in names {
            let s = self.load(n)?;
            for (k, v) in &s.data {
                if !valid_env_key(k) {
                    continue;
                }
                let p = dir.join(k);
                fs::write(&p, v)?;
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
            }
        }
        Ok(())
    }

    /// Rotates the host master key: decrypts all secrets with the current key,
    /// rotates the [`CredVault`] key (which also re-encrypts the tunnel creds) and
    /// re-seals all secrets with the new key. Values preserved; legacy plaintext
    /// files become encrypted. Aborts BEFORE rotating if any secret
    /// fails to decrypt (does not end up half-done).
    pub fn rotate_key(&mut self) -> Result<()> {
        // names from disk (`<root>/<name>.json`).
        let mut names = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("json") {
                    if let Some(stem) = p.file_stem().and_then(|x| x.to_str()) {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        // 1) decrypt ALL with the current key (abort if any fails → do not rotate half-way).
        let mut loaded = Vec::with_capacity(names.len());
        for n in &names {
            loaded.push(self.load(n)?);
        }
        // 2) rotate the shared master key (also re-encrypts the tunnel creds).
        self.vault.rotate_key()?;
        // 3) re-seal all secrets with the new key.
        for s in &loaded {
            self.save(s)?;
        }
        Ok(())
    }
}

/// Parses a `.env` file (`KEY=value` lines; ignores empty ones and `#`).
/// Accepts single/double quotes around the value. Returns the valid pairs.
pub fn parse_env_file(content: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !valid_env_key(k) {
                continue;
            }
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(v);
            out.insert(k.to_string(), v.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_key_validation() {
        assert!(valid_env_key("DB_PASS"));
        assert!(valid_env_key("_X1"));
        assert!(!valid_env_key("1BAD"));
        assert!(!valid_env_key("has-dash"));
        assert!(!valid_env_key(""));
    }

    #[test]
    fn parse_dotenv() {
        let m = parse_env_file("# c\nFOO=bar\nexport BAZ=\"q x\"\nQUX='zz'\nbad-key=1\n\nEMPTY=\n");
        assert_eq!(m.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(m.get("BAZ").map(String::as_str), Some("q x"));
        assert_eq!(m.get("QUX").map(String::as_str), Some("zz"));
        assert_eq!(m.get("EMPTY").map(String::as_str), Some(""));
        assert!(!m.contains_key("bad-key"));
    }

    #[test]
    fn store_roundtrip_and_resolve() {
        let dir = std::env::temp_dir().join(format!("dlx-sec-{}", std::process::id()));
        let s = SecretStore::open(&dir).unwrap();
        let mut data = BTreeMap::new();
        data.insert("DB_PASS".to_string(), "xyz".to_string());
        s.save(&Secret {
            name: "db".into(),
            data,
            updated_unix: 1,
        })
        .unwrap();
        // 0600 file
        let mode = std::fs::metadata(dir.join("secrets/db.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let env = s.resolve_env(&["db".to_string(), "missing".to_string()]);
        assert_eq!(env, vec!["DB_PASS=xyz".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn value_encrypted_at_rest_and_legacy_plaintext_readable() {
        let dir = std::env::temp_dir().join(format!("dlx-sec-enc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SecretStore::open(&dir).unwrap();
        let mut data = BTreeMap::new();
        data.insert("TOKEN".to_string(), "PLAINTEXT-MARKER-XYZ".to_string());
        store
            .save(&Secret {
                name: "app".into(),
                data,
                updated_unix: 1,
            })
            .unwrap();

        // the value does NOT appear in cleartext on disk (encrypted at-rest).
        let raw = std::fs::read(dir.join("secrets/app.json")).unwrap();
        assert!(
            !String::from_utf8_lossy(&raw).contains("PLAINTEXT-MARKER-XYZ"),
            "o valor do segredo vazou em claro no disco!"
        );
        assert!(
            raw.starts_with(SEALED_MAGIC),
            "ficheiro devia ter o cabeçalho cifrado"
        );
        // round-trip decrypts correctly.
        assert_eq!(
            store.load("app").unwrap().data.get("TOKEN").unwrap(),
            "PLAINTEXT-MARKER-XYZ"
        );

        // backward-compatibility: an old plaintext JSON file remains readable.
        let mut old = BTreeMap::new();
        old.insert("K".to_string(), "v".to_string());
        let legacy = serde_json::to_vec(&Secret {
            name: "old".into(),
            data: old,
            updated_unix: 0,
        })
        .unwrap();
        std::fs::write(dir.join("secrets/old.json"), &legacy).unwrap();
        assert_eq!(store.load("old").unwrap().data.get("K").unwrap(), "v");
        assert_eq!(store.list().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn materialize_writes_files_0600() {
        let dir = std::env::temp_dir().join(format!("dlx-sec-mat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SecretStore::open(&dir).unwrap();
        let mut data = BTreeMap::new();
        data.insert("DB_PASS".to_string(), "xyz".to_string());
        data.insert("DB_USER".to_string(), "admin".to_string());
        store
            .save(&Secret {
                name: "db".into(),
                data,
                updated_unix: 1,
            })
            .unwrap();
        let out = dir.join("run-secrets");
        store.materialize(&["db".to_string()], &out).unwrap();
        assert_eq!(std::fs::read_to_string(out.join("DB_PASS")).unwrap(), "xyz");
        assert_eq!(
            std::fs::read_to_string(out.join("DB_USER")).unwrap(),
            "admin"
        );
        let fmode = std::fs::metadata(out.join("DB_PASS"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(fmode & 0o777, 0o600);
        let dmode = std::fs::metadata(&out).unwrap().permissions().mode();
        assert_eq!(dmode & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rotate_key_preserves_values_and_changes_key() {
        let dir = std::env::temp_dir().join(format!("dlx-sec-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = SecretStore::open(&dir).unwrap();
        let mut d1 = BTreeMap::new();
        d1.insert("A".to_string(), "1".to_string());
        store
            .save(&Secret {
                name: "s1".into(),
                data: d1,
                updated_unix: 1,
            })
            .unwrap();
        let mut d2 = BTreeMap::new();
        d2.insert("B".to_string(), "2".to_string());
        store
            .save(&Secret {
                name: "s2".into(),
                data: d2,
                updated_unix: 1,
            })
            .unwrap();

        let key_before = std::fs::read(dir.join("tunnels/keyring.key")).unwrap();
        store.rotate_key().unwrap();
        let key_after = std::fs::read(dir.join("tunnels/keyring.key")).unwrap();
        assert_ne!(key_before, key_after, "a chave-mestra não rodou");

        // values preserved, readable with the new key (in memory and reopening from disk).
        assert_eq!(store.load("s1").unwrap().data.get("A").unwrap(), "1");
        assert_eq!(store.load("s2").unwrap().data.get("B").unwrap(), "2");
        let store2 = SecretStore::open(&dir).unwrap();
        assert_eq!(store2.load("s1").unwrap().data.get("A").unwrap(), "1");
        let _ = std::fs::remove_dir_all(dir);
    }
}

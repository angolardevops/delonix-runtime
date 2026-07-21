//! CredVault — credential vault **encrypted at-rest** (Block B of Public Tunnel).
//!
//! Stores sensitive secrets (e.g.: Ngrok/Pinggy authtokens) encrypted with
//! **XChaCha20-Poly1305** (AEAD), under a local master key in
//! `<root>/tunnels/keyring.key` (32 bytes, **0600**, outside backups/exports).
//! Each credential is `nonce(24) || ciphertext` in `<root>/tunnels/cred/<name>.bin`
//! (0600). Supports **rotation** of the master key (re-encrypts everything).
//!
//! ⚠️ Honest security note: without TPM/HSM, local encryption protects against
//! **casual disk reads, backups, exports and accidental leaks** (logs, dumps),
//! **not** against an attacker already holding the privileges of the user running the
//! engine (that one reads the master key). It is the realistic level for rootless single-host;
//! an external KMS would be a future optional interface. Distinct from `SecretStore`
//! (which persists values in **plaintext** 0600 for env injection).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use crate::{Error, Result};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// Encrypted vault. `key` is the master key loaded in memory.
pub struct CredVault {
    /// `<root>/tunnels/cred`
    dir: PathBuf,
    /// `<root>/tunnels/keyring.key`
    key_path: PathBuf,
    key: [u8; KEY_LEN],
}

/// Cryptographic random bytes (getrandom → /dev/urandom on Linux).
fn random_bytes(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|e| Error::Runtime {
        context: "getrandom",
        message: e.to_string(),
    })
}

/// Valid credential name: `[a-z0-9._:-]`, 1–96 chars (allows `ngrok`,
/// `pinggy`, or scopes like `ngrok:team-a`).
pub fn valid_cred_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 96
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-' | b':')
        })
}

fn write_0600(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

impl CredVault {
    /// Opens (or initializes) the vault in `<base>/tunnels`. Creates the master key on
    /// first use.
    pub fn open(base: &Path) -> Result<CredVault> {
        let root = base.join("tunnels");
        let dir = root.join("cred");
        fs::create_dir_all(&dir)?;
        let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        let key_path = root.join("keyring.key");
        let key = Self::load_or_create_key(&key_path)?;
        Ok(CredVault { dir, key_path, key })
    }

    fn load_or_create_key(key_path: &Path) -> Result<[u8; KEY_LEN]> {
        match fs::read(key_path) {
            Ok(bytes) if bytes.len() == KEY_LEN => {
                let mut k = [0u8; KEY_LEN];
                k.copy_from_slice(&bytes);
                Ok(k)
            }
            Ok(_) => Err(Error::Invalid(format!(
                "corrupted master key at {}",
                key_path.display()
            ))),
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut k = [0u8; KEY_LEN];
                random_bytes(&mut k)?;
                write_0600(key_path, &k)?;
                Ok(k)
            }
            Err(e) => Err(Error::Io(e)),
        }
    }

    fn cipher(key: &[u8; KEY_LEN]) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(Key::from_slice(key))
    }

    fn cred_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.bin"))
    }

    /// Encrypts arbitrary bytes with the master key → `nonce(24) || ciphertext`.
    /// Shared primitive: `SecretStore` encrypts its JSON with this, so the
    /// application secrets are encrypted at-rest with the **same** host key.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        random_bytes(&mut nonce)?;
        let ct = Self::cipher(&self.key)
            .encrypt(XNonce::from_slice(&nonce), plaintext)
            .map_err(|_| Error::Invalid("failed to encrypt".into()))?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ct.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ct);
        Ok(blob)
    }

    /// Decrypts a `nonce(24) || ciphertext` blob produced by [`CredVault::seal`].
    pub fn unseal(&self, blob: &[u8]) -> Result<Vec<u8>> {
        if blob.len() < NONCE_LEN + 16 {
            return Err(Error::Invalid("corrupted encrypted blob".into()));
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        Self::cipher(&self.key)
            .decrypt(XNonce::from_slice(nonce), ct)
            .map_err(|_| Error::Invalid("failed to decrypt (wrong key?)".into()))
    }

    /// Encrypts and persists a credential (overwrites if it exists).
    pub fn put(&self, name: &str, value: &str) -> Result<()> {
        if !valid_cred_name(name) {
            return Err(Error::Invalid(format!("invalid credential name: {name}")));
        }
        write_0600(&self.cred_path(name), &self.seal(value.as_bytes())?)?;
        Ok(())
    }

    /// Reads and decrypts a credential. `Ok(None)` if it does not exist.
    pub fn get(&self, name: &str) -> Result<Option<String>> {
        let blob = match fs::read(self.cred_path(name)) {
            Ok(b) => b,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Io(e)),
        };
        let pt = self.unseal(&blob).map_err(|_| {
            Error::Invalid(format!("failed to decrypt {name} (wrong key/corrupted?)"))
        })?;
        let s = String::from_utf8(pt)
            .map_err(|_| Error::Invalid(format!("credential {name} is not UTF-8")))?;
        Ok(Some(s))
    }

    /// Does the credential exist?
    pub fn exists(&self, name: &str) -> bool {
        self.cred_path(name).exists()
    }

    /// Lists the credential **names** (NEVER the values).
    pub fn list(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stripped) = name.strip_suffix(".bin") {
                out.push(stripped.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Removes a credential (idempotent).
    pub fn remove(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.cred_path(name)) {
            Ok(()) => Ok(()),
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// **Master key rotation**: generates a new key, re-encrypts all
    /// credentials with it and replaces the key on disk. Atomic per-credential;
    /// if something fails half-way, the already-re-encrypted ones get the new key — that is why
    /// the new key is written ONLY at the end, and in case of failure the old key
    /// remains and the re-encrypted ones become unreadable (recoverable via re-`put`).
    /// To minimize the window, decrypt-everything first, then re-encrypt.
    pub fn rotate_key(&mut self) -> Result<()> {
        let names = self.list()?;
        // 1) decrypt everything with the current key.
        let mut plain: Vec<(String, String)> = Vec::with_capacity(names.len());
        for n in &names {
            if let Some(v) = self.get(n)? {
                plain.push((n.clone(), v));
            }
        }
        // 2) generate the new key and re-encrypt everything with it.
        let mut new_key = [0u8; KEY_LEN];
        random_bytes(&mut new_key)?;
        let cipher = Self::cipher(&new_key);
        for (n, v) in &plain {
            let mut nonce = [0u8; NONCE_LEN];
            random_bytes(&mut nonce)?;
            let ct = cipher
                .encrypt(XNonce::from_slice(&nonce), v.as_bytes())
                .map_err(|_| Error::Invalid("failed to re-encrypt during rotation".into()))?;
            let mut blob = Vec::with_capacity(NONCE_LEN + ct.len());
            blob.extend_from_slice(&nonce);
            blob.extend_from_slice(&ct);
            write_0600(&self.cred_path(n), &blob)?;
        }
        // 3) persist the new master key and adopt it in memory.
        write_0600(&self.key_path, &new_key)?;
        self.key = new_key;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_base() -> PathBuf {
        let mut p = std::env::temp_dir();
        let mut rnd = [0u8; 8];
        random_bytes(&mut rnd).unwrap();
        p.push(format!("dlx-credvault-test-{}", u64::from_le_bytes(rnd)));
        p
    }

    #[test]
    fn put_get_roundtrip() {
        let base = tmp_base();
        let v = CredVault::open(&base).unwrap();
        v.put("ngrok", "super-secret-token").unwrap();
        assert_eq!(
            v.get("ngrok").unwrap().as_deref(),
            Some("super-secret-token")
        );
        assert!(v.exists("ngrok"));
        assert_eq!(v.get("inexistente").unwrap(), None);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ciphertext_on_disk_is_not_plaintext() {
        let base = tmp_base();
        let v = CredVault::open(&base).unwrap();
        v.put("pinggy", "PLAINTEXT-MARKER-123").unwrap();
        let raw = fs::read(base.join("tunnels/cred/pinggy.bin")).unwrap();
        let as_str = String::from_utf8_lossy(&raw);
        assert!(
            !as_str.contains("PLAINTEXT-MARKER-123"),
            "token vazou em claro no disco!"
        );
        // 0600 permissions
        let mode = fs::metadata(base.join("tunnels/cred/pinggy.bin"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let keymode = fs::metadata(base.join("tunnels/keyring.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(keymode & 0o777, 0o600);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn list_names_only_and_remove() {
        let base = tmp_base();
        let v = CredVault::open(&base).unwrap();
        v.put("ngrok", "a").unwrap();
        v.put("pinggy", "b").unwrap();
        let mut names = v.list().unwrap();
        names.sort();
        assert_eq!(names, vec!["ngrok".to_string(), "pinggy".to_string()]);
        v.remove("ngrok").unwrap();
        assert_eq!(v.list().unwrap(), vec!["pinggy".to_string()]);
        v.remove("ngrok").unwrap(); // idempotent
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn rotation_keeps_values_and_changes_key() {
        let base = tmp_base();
        let mut v = CredVault::open(&base).unwrap();
        v.put("ngrok", "tok-1").unwrap();
        v.put("pinggy", "tok-2").unwrap();
        let key_before = fs::read(base.join("tunnels/keyring.key")).unwrap();
        v.rotate_key().unwrap();
        let key_after = fs::read(base.join("tunnels/keyring.key")).unwrap();
        assert_ne!(key_before, key_after, "a chave-mestra não rodou");
        // values remain readable with the new key
        assert_eq!(v.get("ngrok").unwrap().as_deref(), Some("tok-1"));
        assert_eq!(v.get("pinggy").unwrap().as_deref(), Some("tok-2"));
        // a reopened vault (reads the new key from disk) also decrypts
        let v2 = CredVault::open(&base).unwrap();
        assert_eq!(v2.get("ngrok").unwrap().as_deref(), Some("tok-1"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn invalid_names_rejected() {
        assert!(valid_cred_name("ngrok"));
        assert!(valid_cred_name("ngrok:equipa-a"));
        assert!(!valid_cred_name("Ngrok")); // uppercase
        assert!(!valid_cred_name("")); // empty
        assert!(!valid_cred_name("tok en")); // space
    }
}

//! CredVault — cofre de credenciais **cifrado at-rest** (Bloco B do Public Tunnel).
//!
//! Guarda segredos sensíveis (ex.: authtokens do Ngrok/Pinggy) cifrados com
//! **XChaCha20-Poly1305** (AEAD), sob uma chave-mestra local em
//! `<root>/tunnels/keyring.key` (32 bytes, **0600**, fora de backups/exports).
//! Cada credencial é `nonce(24) || ciphertext` em `<root>/tunnels/cred/<nome>.bin`
//! (0600). Suporta **rotação** da chave-mestra (recifra tudo).
//!
//! ⚠️ Nota honesta de segurança: sem TPM/HSM, a cifra local protege contra
//! **leitura casual do disco, backups, exports e leaks acidentais** (logs, dumps),
//! **não** contra um atacante já com os privilégios do utilizador que corre o
//! engine (esse lê a chave-mestra). É o nível realista para rootless single-host;
//! um KMS externo seria uma interface opcional futura. Distinto do `SecretStore`
//! (que persiste valores em **plaintext** 0600 para injeção de env).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use crate::{Error, Result};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// Cofre cifrado. `key` é a chave-mestra carregada em memória.
pub struct CredVault {
    /// `<root>/tunnels/cred`
    dir: PathBuf,
    /// `<root>/tunnels/keyring.key`
    key_path: PathBuf,
    key: [u8; KEY_LEN],
}

/// Bytes aleatórios criptográficos (getrandom → /dev/urandom no Linux).
fn random_bytes(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|e| Error::Runtime {
        context: "getrandom",
        message: e.to_string(),
    })
}

/// Nome de credencial válido: `[a-z0-9._:-]`, 1–96 chars (permite `ngrok`,
/// `pinggy`, ou escopos como `ngrok:equipa-a`).
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
    /// Abre (ou inicializa) o cofre em `<base>/tunnels`. Cria a chave-mestra na
    /// primeira utilização.
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

    /// Cifra bytes arbitrários com a chave-mestra → `nonce(24) || ciphertext`.
    /// Primitiva partilhada: o `SecretStore` cifra o seu JSON com isto, para os
    /// secrets de aplicação ficarem cifrados at-rest com a **mesma** chave do host.
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

    /// Decifra um blob `nonce(24) || ciphertext` produzido por [`CredVault::seal`].
    pub fn unseal(&self, blob: &[u8]) -> Result<Vec<u8>> {
        if blob.len() < NONCE_LEN + 16 {
            return Err(Error::Invalid("corrupted encrypted blob".into()));
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        Self::cipher(&self.key)
            .decrypt(XNonce::from_slice(nonce), ct)
            .map_err(|_| Error::Invalid("failed to decrypt (wrong key?)".into()))
    }

    /// Cifra e persiste uma credencial (sobrescreve se existir).
    pub fn put(&self, name: &str, value: &str) -> Result<()> {
        if !valid_cred_name(name) {
            return Err(Error::Invalid(format!("invalid credential name: {name}")));
        }
        write_0600(&self.cred_path(name), &self.seal(value.as_bytes())?)?;
        Ok(())
    }

    /// Lê e decifra uma credencial. `Ok(None)` se não existe.
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

    /// Existe a credencial?
    pub fn exists(&self, name: &str) -> bool {
        self.cred_path(name).exists()
    }

    /// Lista os **nomes** das credenciais (NUNCA os valores).
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

    /// Remove uma credencial (idempotente).
    pub fn remove(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.cred_path(name)) {
            Ok(()) => Ok(()),
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// **Rotação da chave-mestra**: gera uma chave nova, recifra todas as
    /// credenciais com ela e substitui a chave em disco. Atómico por-credencial;
    /// se algo falhar a meio, as já-recifradas ficam com a nova chave — por isso
    /// a chave nova é escrita SÓ no fim, e em caso de falha a chave antiga
    /// permanece e as recifradas tornam-se ilegíveis (recuperáveis por re-`put`).
    /// Para minimizar a janela, decifra-tudo primeiro, depois recifra.
    pub fn rotate_key(&mut self) -> Result<()> {
        let names = self.list()?;
        // 1) decifra tudo com a chave atual.
        let mut plain: Vec<(String, String)> = Vec::with_capacity(names.len());
        for n in &names {
            if let Some(v) = self.get(n)? {
                plain.push((n.clone(), v));
            }
        }
        // 2) gera a nova chave e recifra tudo com ela.
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
        // 3) persiste a nova chave-mestra e adota-a em memória.
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
        // permissões 0600
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
        v.remove("ngrok").unwrap(); // idempotente
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
        // valores continuam legíveis com a nova chave
        assert_eq!(v.get("ngrok").unwrap().as_deref(), Some("tok-1"));
        assert_eq!(v.get("pinggy").unwrap().as_deref(), Some("tok-2"));
        // um cofre reaberto (lê a nova chave do disco) também decifra
        let v2 = CredVault::open(&base).unwrap();
        assert_eq!(v2.get("ngrok").unwrap().as_deref(), Some("tok-1"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn invalid_names_rejected() {
        assert!(valid_cred_name("ngrok"));
        assert!(valid_cred_name("ngrok:equipa-a"));
        assert!(!valid_cred_name("Ngrok")); // maiúscula
        assert!(!valid_cred_name("")); // vazio
        assert!(!valid_cred_name("tok en")); // espaço
    }
}

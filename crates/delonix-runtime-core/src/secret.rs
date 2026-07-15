//! Secret Manager: segredos nomeados (pares chave→valor) injetados como variáveis
//! de ambiente nos containers. Persistidos um ficheiro por segredo sob
//! `<root>/secrets/<nome>.json` com permissões **0600** e **cifrados at-rest**
//! (XChaCha20-Poly1305, via [`crate::cred_vault::CredVault`], chave-mestra do host).
//! Ficheiros plaintext de versões anteriores continuam legíveis (retrocompatível);
//! ao re-gravar passam a cifrados. Os valores nunca devem ir para logs/inspect em claro.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cred_vault::CredVault;
use crate::{Error, Result};

/// Um segredo nomeado: um conjunto de pares `CHAVE=valor` (env).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Secret {
    /// Nome do segredo (referência usada por `run --secret <nome>`).
    pub name: String,
    /// Pares chave→valor. As chaves são nomes de variáveis de ambiente.
    #[serde(default)]
    pub data: BTreeMap<String, String>,
    /// Instante de criação/atualização (segundos Unix).
    #[serde(default)]
    pub updated_unix: u64,
}

impl Secret {
    /// Os pares no formato `CHAVE=valor` para injeção como env.
    pub fn env_pairs(&self) -> Vec<String> {
        self.data.iter().map(|(k, v)| format!("{k}={v}")).collect()
    }
}

/// Um nome de variável de ambiente é válido? (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn valid_env_key(k: &str) -> bool {
    let mut it = k.chars();
    matches!(it.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Nome de segredo válido? (`[a-z0-9._-]`, não vazio, ≤ 64).
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Cabeçalho dos ficheiros cifrados (distingue do formato legado plaintext JSON).
const SEALED_MAGIC: &[u8] = b"DLXSEC1\n";

/// Armazém de segredos sob `<root>/secrets`, com ficheiros 0600 **cifrados at-rest**
/// (XChaCha20-Poly1305 via [`CredVault`], chave-mestra do host). Ficheiros plaintext
/// antigos continuam legíveis (retrocompatível).
pub struct SecretStore {
    root: PathBuf,
    vault: CredVault,
}

impl SecretStore {
    /// Abre (criando) o armazém. `base` = `$DELONIX_ROOT` (o dir `secrets` é criado lá).
    /// Reutiliza a chave-mestra do host (a mesma do [`CredVault`]) para cifrar os valores.
    pub fn open(base: impl Into<PathBuf>) -> Result<Self> {
        let base = base.into();
        let root = base.join("secrets");
        fs::create_dir_all(&root)?;
        // o próprio diretório é 0700 (não listável por outros).
        let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        let vault = CredVault::open(&base)?;
        Ok(Self { root, vault })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.json"))
    }

    /// Descodifica os bytes de um ficheiro: cifrado (com cabeçalho) ou legado (plaintext JSON).
    fn decode(&self, bytes: &[u8]) -> Result<Secret> {
        if let Some(sealed) = bytes.strip_prefix(SEALED_MAGIC) {
            Ok(serde_json::from_slice(&self.vault.unseal(sealed)?)?)
        } else {
            Ok(serde_json::from_slice(bytes)?)
        }
    }

    /// Persiste um segredo (escrita atómica + chmod 0600).
    pub fn save(&self, s: &Secret) -> Result<()> {
        if !valid_name(&s.name) {
            return Err(Error::Invalid(format!("nome de segredo inválido: {:?}", s.name)));
        }
        for k in s.data.keys() {
            if !valid_env_key(k) {
                return Err(Error::Invalid(format!("chave de env inválida: {k:?}")));
            }
        }
        // valor cifrado at-rest: cabeçalho || nonce || ciphertext.
        let mut blob = Vec::from(SEALED_MAGIC);
        blob.extend_from_slice(&self.vault.seal(&serde_json::to_vec(s)?)?);
        let tmp = self.root.join(format!(".{}.tmp", s.name));
        fs::write(&tmp, &blob)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, self.path(&s.name))?;
        Ok(())
    }

    /// Carrega um segredo por nome.
    pub fn load(&self, name: &str) -> Result<Secret> {
        let p = self.path(name);
        if !p.exists() {
            return Err(Error::NotFound(format!("segredo {name}")));
        }
        self.decode(&fs::read(p)?)
    }

    /// Lista os nomes dos segredos existentes.
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

    /// Remove um segredo.
    pub fn remove(&self, name: &str) -> Result<()> {
        let p = self.path(name);
        if !p.exists() {
            return Err(Error::NotFound(format!("segredo {name}")));
        }
        fs::remove_file(p)?;
        Ok(())
    }

    /// Resolve uma lista de nomes de segredos nos seus pares `CHAVE=valor` (env),
    /// pela ordem dada (os últimos sobrepõem-se aos primeiros na mesma chave).
    /// Nomes inexistentes são ignorados (best-effort no arranque).
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

    /// Materializa os segredos como **ficheiros** em `dir` (um por chave, 0600;
    /// `dir` 0700) — para injeção via bind-mount em `/run/secrets` (Pilar 5 Bloco B),
    /// em vez de variáveis de ambiente (que vazam em `environ`/`inspect`). Idempotente.
    /// Nomes inexistentes → erro (o chamador validou antes).
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

    /// Roda a chave-mestra do host: decifra todos os segredos com a chave atual,
    /// roda a chave do [`CredVault`] (que re-cifra também as creds de túnel) e
    /// re-sela todos os segredos com a nova chave. Valores preservados; ficheiros
    /// plaintext legados passam a cifrados. Aborta ANTES de rodar se algum segredo
    /// não decifrar (não fica a meio).
    pub fn rotate_key(&mut self) -> Result<()> {
        // nomes a partir do disco (`<root>/<nome>.json`).
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
        // 1) decifra TODOS com a chave atual (aborta se algum falhar → não rodar a meio).
        let mut loaded = Vec::with_capacity(names.len());
        for n in &names {
            loaded.push(self.load(n)?);
        }
        // 2) roda a chave-mestra partilhada (recifra também as creds de túnel).
        self.vault.rotate_key()?;
        // 3) re-sela todos os segredos com a nova chave.
        for s in &loaded {
            self.save(s)?;
        }
        Ok(())
    }
}

/// Faz o parse de um ficheiro `.env` (linhas `CHAVE=valor`; ignora vazias e `#`).
/// Aceita aspas simples/duplas à volta do valor. Devolve os pares válidos.
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
        s.save(&Secret { name: "db".into(), data, updated_unix: 1 }).unwrap();
        // ficheiro 0600
        let mode = std::fs::metadata(dir.join("secrets/db.json")).unwrap().permissions().mode();
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
        store.save(&Secret { name: "app".into(), data, updated_unix: 1 }).unwrap();

        // o valor NÃO aparece em claro no disco (cifrado at-rest).
        let raw = std::fs::read(dir.join("secrets/app.json")).unwrap();
        assert!(
            !String::from_utf8_lossy(&raw).contains("PLAINTEXT-MARKER-XYZ"),
            "o valor do segredo vazou em claro no disco!"
        );
        assert!(raw.starts_with(SEALED_MAGIC), "ficheiro devia ter o cabeçalho cifrado");
        // round-trip decifra corretamente.
        assert_eq!(store.load("app").unwrap().data.get("TOKEN").unwrap(), "PLAINTEXT-MARKER-XYZ");

        // retrocompatibilidade: um ficheiro plaintext JSON antigo continua legível.
        let mut old = BTreeMap::new();
        old.insert("K".to_string(), "v".to_string());
        let legacy = serde_json::to_vec(&Secret { name: "old".into(), data: old, updated_unix: 0 }).unwrap();
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
        store.save(&Secret { name: "db".into(), data, updated_unix: 1 }).unwrap();
        let out = dir.join("run-secrets");
        store.materialize(&["db".to_string()], &out).unwrap();
        assert_eq!(std::fs::read_to_string(out.join("DB_PASS")).unwrap(), "xyz");
        assert_eq!(std::fs::read_to_string(out.join("DB_USER")).unwrap(), "admin");
        let fmode = std::fs::metadata(out.join("DB_PASS")).unwrap().permissions().mode();
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
        store.save(&Secret { name: "s1".into(), data: d1, updated_unix: 1 }).unwrap();
        let mut d2 = BTreeMap::new();
        d2.insert("B".to_string(), "2".to_string());
        store.save(&Secret { name: "s2".into(), data: d2, updated_unix: 1 }).unwrap();

        let key_before = std::fs::read(dir.join("tunnels/keyring.key")).unwrap();
        store.rotate_key().unwrap();
        let key_after = std::fs::read(dir.join("tunnels/keyring.key")).unwrap();
        assert_ne!(key_before, key_after, "a chave-mestra não rodou");

        // valores preservados, legíveis com a nova chave (em memória e reabrindo do disco).
        assert_eq!(store.load("s1").unwrap().data.get("A").unwrap(), "1");
        assert_eq!(store.load("s2").unwrap().data.get("B").unwrap(), "2");
        let store2 = SecretStore::open(&dir).unwrap();
        assert_eq!(store2.load("s1").unwrap().data.get("A").unwrap(), "1");
        let _ = std::fs::remove_dir_all(dir);
    }
}

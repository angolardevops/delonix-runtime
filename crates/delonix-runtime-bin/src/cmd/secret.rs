//! `delonix secret` — runtime secret vault (Secret Manager, docker/k8s
//! style). Thin wrapper over `delonix_runtime_core::SecretStore`, which already
//! encrypts at rest (XChaCha20-Poly1305 under a local master key).
//!
//! It is the producer of the secrets that `container run --secret <name>` consumes.
//! **Values are never printed** by default (`inspect` redacts them; `--reveal`
//! is explicit opt-in) — a `secret` is routinely pasted into issues/chats.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use delonix_runtime_core::secret::{parse_env_file, valid_name};
use delonix_runtime_core::{Error, Result, Secret, SecretStore};
use serde::Deserialize;

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::state_root;

#[derive(Subcommand)]
pub enum SecretCmd {
    /// Create/replace a secret from literals and/or a `.env` file.
    Create {
        name: String,
        /// `KEY=value` pair. Repeatable.
        #[arg(long = "from-literal")]
        from_literal: Vec<String>,
        /// Load `KEY=value` lines from a file (e.g. `.env`).
        #[arg(long = "from-env-file")]
        from_env_file: Option<PathBuf>,
    },
    /// List the secrets (name + number of keys; values NEVER shown).
    Ls,
    /// Show the keys of a secret (values redacted, unless `--reveal`).
    Inspect {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::secrets))]
        name: String,
        /// Reveal the VALUES in cleartext (dangerous — avoid on shared terminals).
        #[arg(long)]
        reveal: bool,
    },
    /// Set/update keys in a secret (creates it if it does not exist).
    Set {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::secrets))]
        name: String,
        /// `KEY=value` pairs.
        pairs: Vec<String>,
    },
    /// Remove a key from a secret (or the whole secret with `--all`).
    Unset {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::secrets))]
        name: String,
        key: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Remove a secret.
    Rm {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::secrets))]
        name: String,
    },
    /// Rotate the host master key: re-encrypt ALL secrets with a new key.
    /// The values are preserved.
    RotateKey,
    /// Apply the `kind: Secret` documents from a manifest (declarative — creates
    /// the secret without needing `secret create` on the CLI).
    Apply {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

/// `spec` of `kind: Secret` — a bag of key/value pairs encrypted at-rest,
/// consumed by `Container.secret` (env/files) and `Storage.passwordSecret`
/// (`password` key). Closes the "no CLI" gap: the secret is declared in YAML
/// instead of `delonix secret create`.
#[derive(Debug, Deserialize)]
struct SecretSpec {
    /// Inline `KEY: value` pairs. **Plaintext in the manifest** — convenient for
    /// dev, but the value stays in cleartext in the file; for production prefer
    /// `fromEnvFile` (outside version control) or the CLI's `secret create`. Warned at apply.
    #[serde(default, rename = "stringData")]
    string_data: BTreeMap<String, String>,
    /// Path to a `KEY=value` file (e.g. `.env`) — keeps the values OUT of the
    /// manifest. Applied BEFORE `stringData` (inline overrides the file).
    #[serde(default, rename = "fromEnvFile")]
    from_env_file: Option<PathBuf>,
}

/// Names accepted in the `kind: Secret` `spec`, for the unknown-field warning.
pub(crate) const SECRET_SPEC_FIELDS: &[&str] = &["stringData", "fromEnvFile"];

/// Reads and parses a `KEY=value` file, resolving the path relative to `base`
/// (the CWD for the `secret create` CLI; the MANIFEST folder for `kind: Secret` —
/// otherwise a `fromEnvFile: ./app.env` would look in the CWD of whoever runs the
/// command, not next to the manifest). Shared by `create` and `apply`.
fn load_env_file(base: &Path, f: &Path) -> Result<BTreeMap<String, String>> {
    let path = if f.is_absolute() {
        f.to_path_buf()
    } else {
        base.join(f)
    };
    let content = std::fs::read_to_string(&path)
        .map_err(|e| Error::Invalid(format!("env-file {}: {e}", path.display())))?;
    Ok(parse_env_file(&content))
}

/// Applies the `kind: Secret` documents (called by `secret apply` and by
/// `stack apply`). Idempotent: `SecretStore::save` creates or replaces. `base` is
/// the manifest folder, to resolve `fromEnvFile` relative to it.
pub fn apply(docs: &[ManifestDoc], base: &Path) -> Result<()> {
    let store = SecretStore::open(state_root())?;
    for doc in manifest::of_kind(docs, "Secret") {
        let name = &doc.metadata.name;
        manifest::warn_unknown_fields(doc, SECRET_SPEC_FIELDS);
        let spec: SecretSpec = manifest::spec_of(doc)?;

        let mut data = BTreeMap::new();
        if let Some(f) = &spec.from_env_file {
            data.extend(load_env_file(base, f)?);
        }
        // Inline overrides the file. Warning: the values stay in cleartext in the manifest.
        if !spec.string_data.is_empty() {
            eprintln!(
                "AVISO: Secret '{name}': stringData tem valores em CLARO no manifesto — não commites isto num repo; usa fromEnvFile ou `delonix secret create` para produção"
            );
            data.extend(spec.string_data);
        }
        if data.is_empty() {
            return Err(Error::Invalid(format!(
                "Secret '{name}': vazio — indica stringData e/ou fromEnvFile"
            )));
        }
        let n = data.len();
        store.save(&Secret {
            name: name.clone(),
            data,
            updated_unix: now_unix(),
        })?;
        println!("secret/{name}: garantido ({n} chave(s))");
    }
    Ok(())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Splits `KEY=value` (at the FIRST `=`; the value may contain `=`).
fn parse_kv(s: &str) -> Option<(String, String)> {
    let (k, v) = s.split_once('=')?;
    if k.is_empty() {
        return None;
    }
    Some((k.to_string(), v.to_string()))
}

pub fn run(action: SecretCmd) -> Result<()> {
    // `Apply` does not use the vault opened below (it opens its own) and resolves
    // the paths relative to the MANIFEST folder — handled separately, before opening
    // the store (avoids an unnecessary vault open). Same pattern as `stack::run`.
    if let SecretCmd::Apply { file } = action {
        let path = manifest::resolve_path(file)?;
        let docs = manifest::load(&path)?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        return apply(&docs, base);
    }
    let mut store = SecretStore::open(state_root())?;
    match action {
        // Handled at the top (does a `return`).
        SecretCmd::Apply { .. } => unreachable!("tratado acima"),
        SecretCmd::Create {
            name,
            from_literal,
            from_env_file,
        } => {
            if !valid_name(&name) {
                return Err(Error::Invalid(format!(
                    "nome de segredo inválido: {name:?}"
                )));
            }
            let mut data = std::collections::BTreeMap::new();
            if let Some(f) = from_env_file {
                // CLI: path relative to the CWD of whoever runs the command.
                data.extend(load_env_file(Path::new("."), &f)?);
            }
            for lit in &from_literal {
                let (k, v) = parse_kv(lit).ok_or_else(|| {
                    Error::Invalid(format!("--from-literal inválido: {lit:?} (usa KEY=value)"))
                })?;
                data.insert(k, v);
            }
            if data.is_empty() {
                return Err(Error::Invalid(
                    "segredo vazio — usa --from-literal KEY=value e/ou --from-env-file".into(),
                ));
            }
            let n = data.len();
            store.save(&Secret {
                name: name.clone(),
                data,
                updated_unix: now_unix(),
            })?;
            println!("segredo '{name}' criado ({n} chave(s))");
        }
        SecretCmd::Ls => {
            let mut t = output::Table::new(&["NAME", "KEYS", "NAMES"]).right_align(1);
            for s in store.list() {
                let keys: Vec<&str> = s.data.keys().map(String::as_str).collect();
                t.row(vec![
                    s.name.clone(),
                    s.data.len().to_string(),
                    keys.join(", "),
                ]);
            }
            t.print();
        }
        SecretCmd::Inspect { name, reveal } => {
            let s = store.load(&name)?;
            println!("Name:  {}", s.name);
            for (k, v) in &s.data {
                // Redaction by default — the value only comes out with explicit --reveal.
                println!(
                    "  {k}={}",
                    if reveal {
                        v.clone()
                    } else {
                        "••••••".into()
                    }
                );
            }
            if !reveal && !s.data.is_empty() {
                println!(
                    "{}",
                    output::dim("(valores ocultos — usa --reveal para os mostrar)")
                );
            }
        }
        SecretCmd::Set { name, pairs } => {
            if pairs.is_empty() {
                return Err(Error::Invalid("indica pelo menos um KEY=value".into()));
            }
            let mut s = store.load(&name).unwrap_or_else(|_| Secret {
                name: name.clone(),
                ..Default::default()
            });
            s.name = name.clone();
            for p in &pairs {
                let (k, v) = parse_kv(p).ok_or_else(|| {
                    Error::Invalid(format!("par inválido: {p:?} (usa KEY=value)"))
                })?;
                s.data.insert(k, v);
            }
            s.updated_unix = now_unix();
            store.save(&s)?;
            println!("segredo '{name}' actualizado ({} chave(s))", s.data.len());
        }
        SecretCmd::Unset { name, key, all } => {
            if all {
                store.remove(&name)?;
                println!("segredo '{name}' removido");
                return Ok(());
            }
            let k = key.ok_or_else(|| {
                Error::Invalid(super::po::t("say which key to remove (or --all)").into())
            })?;
            let mut s = store.load(&name)?;
            if s.data.remove(&k).is_none() {
                return Err(Error::Invalid(format!(
                    "chave '{k}' não existe em '{name}'"
                )));
            }
            s.updated_unix = now_unix();
            store.save(&s)?;
            println!("chave '{k}' removida de '{name}'");
        }
        SecretCmd::Rm { name } => {
            store.remove(&name)?;
            println!("segredo '{name}' removido");
        }
        SecretCmd::RotateKey => {
            store.rotate_key()?;
            println!("chave-mestra rodada — todos os segredos re-cifrados com a nova chave");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_kv;

    #[test]
    fn parse_kv_corta_no_primeiro_igual() {
        assert_eq!(parse_kv("K=v"), Some(("K".into(), "v".into())));
        // The value may contain '=' (e.g. a base64 token with padding).
        assert_eq!(
            parse_kv("TOKEN=ab==cd"),
            Some(("TOKEN".into(), "ab==cd".into()))
        );
        // An empty key is not valid.
        assert_eq!(parse_kv("=v"), None);
        assert_eq!(parse_kv("semigual"), None);
    }
}

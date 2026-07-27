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
        /// Load `KEY=value` lines from a file (e.g. `.env`), or `-` to read them from stdin (the value never touches argv/process list).
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
    // BUG FOUND (code review, live testing): the docs' own flagship example
    // — "secret value via stdin, never in argv" — was `printf 's3nha' |
    // delonix secret create db-pass`, but there was NO way to actually get a
    // piped value into a secret: `create` only had `--from-literal
    // KEY=value` (puts the value IN argv/process-list, defeating the exact
    // thing the example was demonstrating) and `--from-env-file <path>` (a
    // real file only). `-` as the env-file path now means "read from
    // stdin" — the standard Unix convention (`tar -`, `docker ... -f -`) —
    // so the documented pattern becomes real: `printf 'password=s3nha' |
    // delonix secret create db-pass --from-env-file -`.
    if f == Path::new("-") {
        use std::io::Read;
        let mut content = String::new();
        std::io::stdin()
            .read_to_string(&mut content)
            .map_err(|e| Error::Invalid(format!("reading stdin: {e}")))?;
        return Ok(parse_env_file(&content));
    }
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
                "{}",
                super::po::tf(
                    "WARNING: Secret '{name}': stringData has values in CLEARTEXT in the manifest — do not commit this to a repo; use fromEnvFile or `delonix secret create` for production",
                    &[("name", name)],
                )
            );
            data.extend(spec.string_data);
        }
        if data.is_empty() {
            return Err(Error::Invalid(super::po::tf(
                "Secret '{name}': empty — provide stringData and/or fromEnvFile",
                &[("name", name)],
            )));
        }
        let n = data.len();
        store.save(&Secret {
            name: name.clone(),
            data,
            updated_unix: now_unix(),
        })?;
        println!(
            "{}",
            super::po::tf(
                "secret/{name}: ensured ({n} key(s))",
                &[("name", name), ("n", &n.to_string())],
            )
        );
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
        SecretCmd::Apply { .. } => unreachable!("handled above"),
        SecretCmd::Create {
            name,
            from_literal,
            from_env_file,
        } => {
            if !valid_name(&name) {
                return Err(Error::Invalid(super::po::tf(
                    "invalid secret name: {name}",
                    &[("name", &format!("{name:?}"))],
                )));
            }
            let mut data = std::collections::BTreeMap::new();
            if let Some(f) = from_env_file {
                // CLI: path relative to the CWD of whoever runs the command.
                data.extend(load_env_file(Path::new("."), &f)?);
            }
            for lit in &from_literal {
                let (k, v) = parse_kv(lit).ok_or_else(|| {
                    Error::Invalid(super::po::tf(
                        "invalid --from-literal: {lit} (use KEY=value)",
                        &[("lit", &format!("{lit:?}"))],
                    ))
                })?;
                data.insert(k, v);
            }
            if data.is_empty() {
                return Err(Error::Invalid(
                    super::po::t(
                        "empty secret — use --from-literal KEY=value and/or --from-env-file",
                    )
                    .into(),
                ));
            }
            let n = data.len();
            store.save(&Secret {
                name: name.clone(),
                data,
                updated_unix: now_unix(),
            })?;
            println!(
                "{}",
                super::po::tf(
                    "secret '{name}' created ({n} key(s))",
                    &[("name", &name), ("n", &n.to_string())],
                )
            );
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
                    output::dim(super::po::t("(hidden values — use --reveal to show them)"))
                );
            }
        }
        SecretCmd::Set { name, pairs } => {
            if pairs.is_empty() {
                return Err(Error::Invalid(
                    super::po::t("provide at least one KEY=value").into(),
                ));
            }
            // Parse ALL pairs before touching the store — a bad pair must
            // fail before we ever take the lock, not half-way through.
            let mut kvs = Vec::with_capacity(pairs.len());
            for p in &pairs {
                let (k, v) = parse_kv(p).ok_or_else(|| {
                    Error::Invalid(super::po::tf(
                        "invalid pair: {p} (use KEY=value)",
                        &[("p", &format!("{p:?}"))],
                    ))
                })?;
                kvs.push((k, v));
            }
            // BUG FOUND: this used to be a naive load+mutate+save with no
            // lock — two concurrent `secret set db A=1` / `secret set db
            // B=2` (or a `stack apply` re-run racing automation) could both
            // read the same starting state and each save its own version,
            // silently dropping the other's key. `SecretStore::update` is
            // the same flock-guarded read-modify-write `Store::update`
            // already uses for container state.
            let s = store.update(&name, |s| {
                s.name = name.clone();
                for (k, v) in &kvs {
                    s.data.insert(k.clone(), v.clone());
                }
                s.updated_unix = now_unix();
                true
            })?;
            println!(
                "{}",
                super::po::tf(
                    "secret '{name}' updated ({n} key(s))",
                    &[("name", &name), ("n", &s.data.len().to_string())],
                )
            );
        }
        SecretCmd::Unset { name, key, all } => {
            if all {
                store.remove(&name)?;
                println!(
                    "{}",
                    super::po::tf("secret '{name}' removed", &[("name", &name)])
                );
                return Ok(());
            }
            let k = key.ok_or_else(|| {
                Error::Invalid(super::po::t("say which key to remove (or --all)").into())
            })?;
            store.load(&name)?; // distinct "secret not found" vs. "key not found" below
            let mut removed = false;
            store.update(&name, |s| {
                removed = s.data.remove(&k).is_some();
                if removed {
                    s.updated_unix = now_unix();
                }
                removed
            })?;
            if !removed {
                return Err(Error::Invalid(super::po::tf(
                    "key '{k}' does not exist in '{name}'",
                    &[("k", &k), ("name", &name)],
                )));
            }
            println!(
                "{}",
                super::po::tf(
                    "key '{k}' removed from '{name}'",
                    &[("k", &k), ("name", &name)],
                )
            );
        }
        SecretCmd::Rm { name } => {
            store.remove(&name)?;
            println!(
                "{}",
                super::po::tf("secret '{name}' removed", &[("name", &name)])
            );
        }
        SecretCmd::RotateKey => {
            store.rotate_key()?;
            println!(
                "{}",
                super::po::t("master key rotated — all secrets re-encrypted with the new key")
            );
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

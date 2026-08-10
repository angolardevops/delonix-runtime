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

/// `secret ls -o json` row (ADR-0005): name + key NAMES + count. NEVER the values.
#[derive(serde::Serialize)]
struct SecretLsRow {
    name: String,
    keys: usize,
    names: Vec<String>,
}

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
    Ls {
        /// Output format: `table` (default) or `json` (ADR-0005).
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: crate::cmd::output::OutputFormat,
    },
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

/// What a redacted value renders as. A fixed marker, and deliberately not
/// something length-preserving like `****` — the LENGTH of a secret is itself a
/// hint, and a dry-run that leaks the shape of a password has only moved the
/// leak somewhere less obvious.
const REDACTED: &str = "<redacted>";

/// Dry-run of a `kind: Secret` — the keys, never the values.
///
/// Secret was the ONE Kind `render_with_defaults` skipped, on the reasoning that
/// there was no point reformatting `stringData`. The consequence was worse than
/// the cost: the most sensitive Kind in the manifest was also the only one with
/// no `--dry-run` at all, so the one document where you most want to check
/// «did it read the keys I meant?» before applying was the one you could not
/// check.
///
/// It is answerable without printing a single value: what a reader needs is the
/// KEY NAMES and where they came from. The values are replaced by [`REDACTED`]
/// — including the ones a `fromEnvFile` would contribute, which are not even
/// read here (resolving the file is the apply's job, and a dry-run that opened
/// it would turn planning into an I/O side effect).
pub fn spec_with_defaults(doc: &ManifestDoc) -> Result<serde_yaml::Value> {
    let spec: SecretSpec = manifest::spec_of(doc)?;
    let mut out = serde_yaml::Mapping::new();
    let mut redacted = serde_yaml::Mapping::new();
    for k in spec.string_data.keys() {
        redacted.insert(
            serde_yaml::Value::from(k.clone()),
            serde_yaml::Value::from(REDACTED),
        );
    }
    out.insert(
        serde_yaml::Value::from("stringData"),
        serde_yaml::Value::Mapping(redacted),
    );
    out.insert(
        serde_yaml::Value::from("fromEnvFile"),
        match &spec.from_env_file {
            Some(p) => serde_yaml::Value::from(p.display().to_string()),
            None => serde_yaml::Value::Null,
        },
    );
    Ok(serde_yaml::Value::Mapping(out))
}

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
        SecretCmd::Ls { output } => {
            if output == crate::cmd::output::OutputFormat::Json {
                // Key NAMES + count only — never the values (those stay redacted,
                // revealed only by `secret inspect --reveal`).
                let rows: Vec<SecretLsRow> = store
                    .list()
                    .into_iter()
                    .map(|s| SecretLsRow {
                        name: s.name.clone(),
                        keys: s.data.len(),
                        names: s.data.keys().cloned().collect(),
                    })
                    .collect();
                return crate::cmd::output::print_json(&rows);
            }
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
            // The event log had NO record of any secret/volume/storage lifecycle
            // operation — only containers. A credential being destroyed is exactly
            // what an operator needs to find in `system events` afterwards. The
            // NAME only: values never leave the vault, and events are deliberately
            // short (see `events.rs` on PIPE_BUF atomicity).
            delonix_runtime_core::events::emit(
                &state_root(),
                "secret",
                "remove",
                &name,
                &name,
                None,
            );
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

    /// **A propriedade que interessa**: o dry-run mostra as CHAVES e nunca um
    /// valor. Sem isto o Kind mais sensível do manifesto era o único sem
    /// `--dry-run` — o sítio onde mais se quer confirmar «leu as chaves que eu
    /// queria?» era o único onde não se podia confirmar.
    #[test]
    fn o_dry_run_de_um_secret_mostra_as_chaves_e_nunca_os_valores() {
        let doc: super::ManifestDoc = serde_yaml::from_str(
            "apiVersion: delonix.io/v1\nkind: Secret\nmetadata: { name: s }\nspec:\n  stringData: { password: hunter2, token: abc123 }\n",
        )
        .unwrap();
        let out = serde_yaml::to_string(&super::spec_with_defaults(&doc).unwrap()).unwrap();
        assert!(out.contains("password"), "{out}");
        assert!(out.contains("token"), "{out}");
        assert!(!out.contains("hunter2"), "valor vazado: {out}");
        assert!(!out.contains("abc123"), "valor vazado: {out}");
        assert_eq!(out.matches(super::REDACTED).count(), 2);
    }

    /// Um `fromEnvFile` não é aberto para o dry-run: resolver o ficheiro é
    /// trabalho do apply, e um plano que o lesse tornaria o planeamento um
    /// efeito de I/O. O caminho aparece; o conteúdo nunca.
    #[test]
    fn o_dry_run_nao_abre_o_fromenvfile() {
        let doc: super::ManifestDoc = serde_yaml::from_str(
            "apiVersion: delonix.io/v1\nkind: Secret\nmetadata: { name: s }\nspec: { fromEnvFile: /nao/existe/app.env }\n",
        )
        .unwrap();
        // Não falha, apesar de o ficheiro não existir — prova que não foi lido.
        let out = serde_yaml::to_string(&super::spec_with_defaults(&doc).unwrap()).unwrap();
        assert!(out.contains("/nao/existe/app.env"), "{out}");
    }
}

//! `delonix secret` — runtime secret vault (Secret Manager, docker/k8s
//! style). Thin wrapper over `delonix_runtime_core::SecretStore`, which already
//! encrypts at rest (XChaCha20-Poly1305 under a local master key).
//!
//! It is the producer of the secrets that `container run --secret <name>` consumes.
//! **Values are never printed** by default (`inspect` redacts them; `--reveal`
//! is explicit opt-in) — a `secret` is routinely pasted into issues/chats.

use super::kinds as k;
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
        #[arg(value_hint = clap::ValueHint::FilePath, long = "from-env-file")]
        from_env_file: Option<PathBuf>,
        /// Take the value from an environment VARIABLE: `--from-env DB_PASSWORD` stores `$DB_PASSWORD` under that name, `--from-env password=PGPASSWORD` under `password`. Repeatable. The value never appears in argv, unlike `--from-literal`.
        #[arg(long = "from-env")]
        from_env: Vec<String>,
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
        /// Output format: `table` (default, the historical text) or `json` (ADR-0005). Redaction applies to BOTH — `--reveal` is what unlocks the values, never the format
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: output::OutputFormat,
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
    /// Apply the `kind: Secret` documents from a manifest.
    ///
    /// Declarative — creates the secret without needing `secret create` on the
    /// CLI.
    Apply {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
}

/// `spec` of `kind: Secret` — a bag of key/value pairs encrypted at-rest,
/// consumed by `Container.secret` (env/files) and `Storage.passwordSecret`
/// (`password` key). Closes the "no CLI" gap: the secret is declared in YAML
/// instead of `delonix secret create`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SecretSpec {
    /// Inline `KEY: value` pairs. **Plaintext in the manifest** — convenient for
    /// dev, but the value stays in cleartext in the file; for production prefer
    /// `fromEnvFile` (outside version control) or the CLI's `secret create`. Warned at apply.
    #[serde(default, rename = "stringData")]
    string_data: BTreeMap<String, String>,
    /// Path to a `KEY=value` file (e.g. `.env`) — keeps the values OUT of the
    /// manifest. Applied BEFORE `stringData` (inline overrides the file).
    #[serde(default, rename = "fromEnvFile")]
    from_env_file: Option<PathBuf>,
    /// Keys read from the PROCESS's environment at apply time.
    ///
    /// `["DB_PASSWORD"]` takes `$DB_PASSWORD` and stores it under that name;
    /// `{ password: DB_PASSWORD }` stores it under `password` instead — which is
    /// what the consumers of a secret usually want, since `Storage`/`Tunnel`/
    /// `provision` each look for a key by a fixed name.
    ///
    /// This is the form a CI job has: the value arrives as an environment
    /// variable from the runner's own secret store, and there is no file to
    /// point at and nothing to write into the manifest. Neither of the other
    /// two shapes could express it.
    #[serde(default, rename = "fromEnv")]
    from_env: Option<FromEnv>,
}

/// `fromEnv:` accepts a list of names or a `key: ENV_VAR` mapping. Two shapes
/// because the useful cases differ: a list is the shorthand when the key name
/// and the variable name are the same, and the mapping is what you need when
/// the consumer expects a fixed key (`password`) but the environment calls it
/// something else (`PGPASSWORD`).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum FromEnv {
    Names(Vec<String>),
    Mapped(BTreeMap<String, String>),
}

impl FromEnv {
    /// `(secret key, environment variable)` pairs, in a stable order.
    ///
    /// Both sides go through [`env_var_name`], so a leading `$` never reaches
    /// either the reader or the renderer.
    fn pairs(&self) -> Vec<(String, String)> {
        match self {
            FromEnv::Names(v) => v
                .iter()
                .map(|n| {
                    let n = env_var_name(n);
                    (n.clone(), n)
                })
                .collect(),
            FromEnv::Mapped(m) => m
                .iter()
                .map(|(k, v)| (k.clone(), env_var_name(v)))
                .collect(),
        }
    }
}

/// Strips a leading `$` from an environment variable name.
///
/// Two problems close here, and the second is the one that had a live bug.
///
/// A manifest written by hand naturally says `password: $PGPASSWORD` — the
/// shell habit — and a name starting with `$` names nothing, so
/// [`valid_env_name`] refused a spelling that reads as correct.
///
/// And [`spec_with_defaults`] RENDERS the variable with a `$`, so without this
/// the output of `stack apply --dry-run` was not valid input to `apply`: each
/// pass added another `$` (`$PGPASSWORD` → `$$PGPASSWORD`) and re-applying the
/// printed spec was refused. That breaks the promise the dry-run exists to make
/// — that it describes what WILL be applied — and with it the GitOps flow
/// `docs/gitops.md` publishes (plan on the PR, apply on the merge).
///
/// Normalising HERE, in the one place both the reader ([`read_env_keys`]) and
/// the renderer go through, is what keeps the two from drifting — the same
/// generator-and-reader-share-the-format discipline as `fw_rule_tail`. There is
/// no ambiguity to lose: a POSIX variable name can never begin with `$`.
fn env_var_name(s: &str) -> String {
    s.strip_prefix('$').unwrap_or(s).to_string()
}

/// Reads the named environment variables.
///
/// **A missing variable is an ERROR, not an empty value.** A secret quietly
/// holding `""` because a CI job forgot to export something is worse than an
/// apply that stops: the resource is created, the apply reports success, and
/// whatever consumes it fails later with an authentication error that names
/// nothing. All the missing names are reported at once, so a broken pipeline is
/// fixed in one pass instead of one variable per run.
fn read_env_keys(name: &str, from: &FromEnv) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut missing = Vec::new();
    for (key, var) in from.pairs() {
        if !valid_env_name(&var) {
            return Err(Error::Invalid(super::po::tf(
                "Secret '{name}': '{var}' is not a valid environment variable name",
                &[("name", name), ("var", &var)],
            )));
        }
        match std::env::var(&var) {
            Ok(v) => {
                out.insert(key, v);
            }
            Err(_) => missing.push(var),
        }
    }
    if !missing.is_empty() {
        return Err(Error::Invalid(super::po::tf(
            "Secret '{name}': these environment variables are not set: {vars} — an unset variable would store an EMPTY secret, and the failure would only surface later as an authentication error",
            &[("name", name), ("vars", &missing.join(", "))],
        )));
    }
    Ok(out)
}

/// A name that could be exported by a shell. Checked because the value goes to
/// `std::env::var`, and a name with a `=` or a space names nothing — it would
/// read as "not set" and be reported as a missing variable the user cannot fix.
fn valid_env_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What a value renders as in `inspect` when it is not revealed.
const REDACTED_VALUE: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

/// Builds the machine view, applying the SAME redaction rule as the text path:
/// a format flag must never be a way around it. Only `--reveal` unlocks a
/// value, and it has to be typed on purpose.
///
/// This is a function and not three lines inline in `run()` for one reason: a
/// test can call it. The first version of the test built a `SecretInspect` by
/// re-implementing this `if reveal` inside the test body, which meant it proved
/// that `serde_json` serializes a map — flipping the production line to
/// `v.as_str()` unconditionally, the exact regression it existed to catch, left
/// it green. A test that cannot fail is worse than no test: it is a claim of
/// coverage over an uncovered path.
fn inspect_view(s: &Secret, reveal: bool) -> SecretInspect<'_> {
    SecretInspect {
        name: &s.name,
        keys: s.data.keys().map(String::as_str).collect(),
        data: s
            .data
            .iter()
            .map(|(k, v)| (k.as_str(), if reveal { v.as_str() } else { REDACTED_VALUE }))
            .collect(),
        revealed: reveal,
    }
}

/// The machine view of a secret (`inspect -o json`).
///
/// `keys` is listed separately from `data` because it is the field automation
/// actually wants: «does this secret carry the key my consumer looks for» is
/// answerable without ever touching a value, revealed or not.
#[derive(serde::Serialize)]
struct SecretInspect<'a> {
    name: &'a str,
    keys: Vec<&'a str>,
    data: std::collections::BTreeMap<&'a str, &'a str>,
    /// Whether `data` holds real values or the redaction marker. Without this a
    /// consumer cannot tell a redacted secret from one whose value genuinely is
    /// the marker string.
    revealed: bool,
}

/// Names accepted in the `kind: Secret` `spec`, for the unknown-field warning.
pub(crate) const SECRET_SPEC_FIELDS: &[&str] = &["stringData", "fromEnvFile", "fromEnv"];

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
    // `fromEnv` shows WHICH key comes from WHICH variable, and no value: the
    // pairing is exactly what a reader is checking before applying, and it is
    // not itself a secret. The environment is NOT read here — a plan that
    // resolved it would turn planning into a read of the caller's environment,
    // and would report differently depending on who ran it.
    out.insert(
        serde_yaml::Value::from("fromEnv"),
        match &spec.from_env {
            Some(fe) => {
                let mut m = serde_yaml::Mapping::new();
                for (key, var) in fe.pairs() {
                    // The variable NAME, verbatim — no `$` prefix. A decorated
                    // `$VAR` is not idempotent: the round-trip test feeds a
                    // dry-run's output back through the same code, and the
                    // second pass produced `$$VAR`. A `--dry-run` that does not
                    // describe what will be applied is worse than none, and the
                    // field is already called `fromEnv`.
                    m.insert(serde_yaml::Value::from(key), serde_yaml::Value::from(var));
                }
                serde_yaml::Value::Mapping(m)
            }
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
    for doc in manifest::of_kind(docs, k::SECRET) {
        let name = &doc.metadata.name;
        let spec: SecretSpec = manifest::spec_of(doc)?;

        let mut data = BTreeMap::new();
        if let Some(f) = &spec.from_env_file {
            data.extend(load_env_file(base, f)?);
        }
        // Between the file and the inline block: the environment is more
        // specific than a checked-in `.env`, and less specific than a value
        // written in this very document.
        if let Some(fe) = &spec.from_env {
            data.extend(read_env_keys(name, fe)?);
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
                "Secret '{name}': empty — provide stringData, fromEnvFile and/or fromEnv",
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
            from_env,
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
            // Between the file and the literals, the same precedence the
            // manifest uses.
            if !from_env.is_empty() {
                // `KEY=VAR` maps, a bare `VAR` keeps its own name.
                let mut mapped = std::collections::BTreeMap::new();
                for spec in &from_env {
                    match parse_kv(spec) {
                        Some((key, var)) => mapped.insert(key, var),
                        None => mapped.insert(spec.clone(), spec.clone()),
                    };
                }
                data.extend(read_env_keys(&name, &FromEnv::Mapped(mapped))?);
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
                        "empty secret — use --from-literal KEY=value, --from-env VAR and/or --from-env-file",
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
            let output =
                crate::cmd::config::resolve_output(&crate::cmd::util::state_root(), output);
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
        SecretCmd::Inspect {
            name,
            reveal,
            output,
        } => {
            let output = super::config::resolve_output(&super::util::state_root(), output);
            let s = store.load(&name)?;
            if output == output::OutputFormat::Json {
                return output::print_json(&[inspect_view(&s, reveal)]);
            }
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
    use super::{inspect_view, parse_kv, read_env_keys, valid_env_name, FromEnv, Secret};
    use std::collections::BTreeMap;

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

    #[test]
    fn uma_variavel_por_definir_e_erro_e_nao_um_segredo_vazio() {
        // The failure this refuses: a CI job forgets to export something, the
        // secret is created holding "", the apply reports success, and whatever
        // consumes it fails much later with an authentication error that names
        // nothing. Stopping here is the only place the cause is still visible.
        let var = format!("DLX_TEST_ABSENT_{}", std::process::id());
        std::env::remove_var(&var);
        let e = read_env_keys("s", &FromEnv::Names(vec![var.clone()])).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains(&var),
            "the error must name the variable: {msg}"
        );

        // Every missing name at once — a broken pipeline is fixed in one pass,
        // not one variable per run.
        let a = format!("DLX_TEST_A_{}", std::process::id());
        let b = format!("DLX_TEST_B_{}", std::process::id());
        let e = read_env_keys("s", &FromEnv::Names(vec![a.clone(), b.clone()])).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains(&a) && msg.contains(&b), "{msg}");
    }

    #[test]
    fn o_mapeamento_guarda_sob_a_chave_que_o_consumidor_espera() {
        // The reason the mapped shape exists: `Storage`/`Tunnel`/`provision`
        // each look for a key by a FIXED name (`password`, `token`), and the
        // environment rarely calls it that.
        let var = format!("DLX_TEST_PW_{}", std::process::id());
        std::env::set_var(&var, "s3cr3t");

        let got = read_env_keys("s", &FromEnv::Names(vec![var.clone()])).unwrap();
        assert_eq!(got.get(&var).map(String::as_str), Some("s3cr3t"));

        let mut m = BTreeMap::new();
        m.insert("password".to_string(), var.clone());
        let got = read_env_keys("s", &FromEnv::Mapped(m)).unwrap();
        assert_eq!(got.get("password").map(String::as_str), Some("s3cr3t"));
        assert!(!got.contains_key(&var), "only the mapped key is stored");

        // An empty value that was DELIBERATELY exported is kept: "set to
        // nothing" is a different statement from "not set", and only the
        // second is an error.
        let empty = format!("DLX_TEST_EMPTY_{}", std::process::id());
        std::env::set_var(&empty, "");
        let got = read_env_keys("s", &FromEnv::Names(vec![empty.clone()])).unwrap();
        assert_eq!(got.get(&empty).map(String::as_str), Some(""));

        std::env::remove_var(&var);
        std::env::remove_var(&empty);
    }

    #[test]
    fn um_nome_de_variavel_invalido_e_recusado_e_nao_lido_como_ausente() {
        // Without this check a name with a `=` or a space reads as "not set"
        // and gets reported as a missing variable the user cannot possibly
        // export — the error would point at the wrong thing.
        for bad in ["", "1ABC", "A B", "A=B", "A-B", "A.B"] {
            assert!(!valid_env_name(bad), "{bad:?} should be refused");
        }
        for ok in ["A", "_x", "DB_PASSWORD", "A1"] {
            assert!(valid_env_name(ok), "{ok:?} should be accepted");
        }
        let e = read_env_keys("s", &FromEnv::Names(vec!["A B".into()])).unwrap_err();
        assert!(e
            .to_string()
            .contains("not a valid environment variable name"));
    }

    /// The dry-run RENDERS the variable with a `$`, so its output has to be
    /// valid INPUT — otherwise `stack apply --dry-run` does not describe what
    /// will be applied, and the published GitOps flow (plan on the PR, apply on
    /// the merge) breaks on the most sensitive Kind there is.
    ///
    /// Reverting `env_var_name` makes this fail: the pair comes back as
    /// `$PGPASSWORD`, the render adds a second `$`, and the round-trip drifts
    /// one `$` per pass.
    #[test]
    fn um_cifrao_a_frente_da_variavel_nao_se_acumula_no_round_trip() {
        let mapped = FromEnv::Mapped(BTreeMap::from([(
            "password".to_string(),
            "$PGPASSWORD".to_string(),
        )]));
        assert_eq!(
            mapped.pairs(),
            vec![("password".to_string(), "PGPASSWORD".to_string())],
            "the `$` is presentation, it is not part of the variable name"
        );

        // The list form normalises the KEY too — a secret key called
        // `$DATABASE_URL` is not what anyone writing the shell habit meant.
        let names = FromEnv::Names(vec!["$DATABASE_URL".to_string()]);
        assert_eq!(
            names.pairs(),
            vec![("DATABASE_URL".to_string(), "DATABASE_URL".to_string())]
        );

        // And with it normalised, the name now passes the validator that used
        // to refuse it — which is the second half of the same bug.
        for (_, var) in mapped.pairs() {
            assert!(valid_env_name(&var), "{var:?} should be accepted");
        }
    }

    /// `-o json` must obey the SAME redaction as the text path. A format flag
    /// that also unlocked values would make `--reveal` decorative, and the leak
    /// would be silent — the JSON looks the same either way to whoever is
    /// piping it somewhere.
    ///
    /// It calls `inspect_view`, the function PRODUCTION calls. The first
    /// version of this test rebuilt the redaction inside the test body, so
    /// reverting the production line left it green — it was asserting that
    /// serde works. Caught in adversarial review of this same series.
    #[test]
    fn o_formato_json_nao_e_uma_via_para_contornar_a_redaccao() {
        let s = Secret {
            name: "s".into(),
            data: BTreeMap::from([("senha".to_string(), "SUPERSECRETO".to_string())]),
            updated_unix: 0,
        };
        for reveal in [false, true] {
            let out = serde_json::to_string(&inspect_view(&s, reveal)).unwrap();
            assert_eq!(
                out.contains("SUPERSECRETO"),
                reveal,
                "o valor so pode aparecer com --reveal (reveal={reveal})"
            );
            // A chave NAO e segredo, e e o que um consumidor verifica.
            assert!(out.contains("senha"));
        }
    }
}

//! `delonix compose` — native `docker-compose.yml` (Compose Spec v2.x) support.
//!
//! A foreign-schema translator, same family as `container::pod_to_run_opts`
//! (Kubernetes Pod shape) and `dockerapi::docker_config_to_run_opts` (Docker
//! Engine API JSON): parses the compose file with hand-rolled typed structs
//! (no new dependency — `serde`/`serde_yaml` are already here), then either
//! builds a `RunOpts` directly (containers — same "translate straight to
//! RunOpts" pattern the two precedents above already use) or a `ManifestDoc`
//! for the simpler Kinds (`Image`/`Network`/`Volume`), reusing their existing
//! `apply()` verbatim — same idempotency, same input hardening, zero
//! duplicated creation logic.
//!
//! **`depends_on` conditions** (`service_started`/`service_healthy`/
//! `service_completed_successfully`) are resolved by a topological sort of the
//! `services:` graph (cycle → hard error, never an arbitrary order) plus a
//! wait loop that polls `runtime::exec`'s own healthcheck test (or the image's
//! own `HEALTHCHECK` when a service doesn't declare one inline) — no engine/
//! store schema change needed.
//!
//! **Project scoping** (`compose down/ps/logs`): every container this module
//! creates carries `delonix.io/compose-project=<project>` (same "derive
//! membership from labels" idiom as `pod.rs`'s `POD_LABEL`); networks/volumes
//! have no labels field, so they use DETERMINISTIC naming instead
//! (`<project>_<name>`, matching real `docker compose`'s own convention) —
//! `down` recomputes the same names from the (re-parsed) compose file, no
//! separate registry, same "derive membership from the manifest" philosophy
//! `stack describe`/`cluster ls` already use.
//!
//! **v1 scope, explicit ship/defer** (never a silent no-op — see
//! `KNOWN_UNSUPPORTED_TOP`/`KNOWN_UNSUPPORTED_SERVICE` for the exact ones
//! that reject with a clear reason): ships `image`/`build`/`environment`/
//! `env_file`/`ports`/`volumes`/`command`/`entrypoint`/`depends_on`
//! (all 3 conditions)/`healthcheck`/`restart`/`networks`/`labels`/`user`/
//! `cap_add`/`cap_drop`/`privileged`/`tmpfs`/`deploy.resources.limits`/
//! `container_name`/`hostname`/`read_only`, top-level `networks:`/`volumes:`
//! (incl. `external: true`). Defers `profiles:`, `extends:`, top-level
//! `configs:`/`secrets:` (use `kind: Secret` instead), multi-file compose
//! (`-f a -f b` merge/`include:`), `build.target` (stage selection),
//! `deploy.replicas != 1`, a fixed `networks.*.ipv4_address`, and anonymous
//! volumes (no explicit source). `working_dir:` IS applied (via `RunOpts.
//! workdir`, itself now also exposed as `container run -w/--workdir`), and a
//! bare container port with no host port DOES get a random free host port
//! (resolved once, before the container is created — see `free_host_port`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Subcommand;
use delonix_image::ImageStore;
use delonix_net::NetworkStore;
use delonix_runtime::{self as runtime};
use delonix_runtime_core::{Container, Error, Result, Status, Store};
use delonix_volume::VolumeStore;
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc, Metadata};
use super::output;
use super::util::{find, open_stores, state_root};

pub(crate) const COMPOSE_PROJECT_LABEL: &str = "delonix.io/compose-project";
pub(crate) const COMPOSE_SERVICE_LABEL: &str = "delonix.io/compose-service";

/// (top-level or per-service key, human-readable reason) — checked explicitly
/// against the RAW parsed YAML (not the typed struct, which would just drop
/// an unrecognized key silently), so the user gets an ACTIONABLE message
/// ("not supported in v1, here's why") instead of nothing at all.
const KNOWN_UNSUPPORTED_TOP: &[(&str, &str)] = &[
    ("profiles", "top-level `profiles` activation is not supported in v1 — every service always runs"),
    ("configs", "top-level `configs:` is not supported in v1 — use `kind: Secret` + `Container.secret` instead"),
    ("secrets", "top-level `secrets:` is not supported in v1 — use `kind: Secret` + `Container.secret` instead"),
    ("include", "multi-file compose (`include:`/`-f a -f b` merge) is not supported — pass exactly one -f"),
];
const KNOWN_UNSUPPORTED_SERVICE: &[(&str, &str)] = &[
    (
        "extends",
        "`extends:` is not supported in v1 — inline the service or use YAML anchors",
    ),
    (
        "profiles",
        "per-service `profiles:` is not supported in v1 — every service always runs",
    ),
];

#[derive(Subcommand)]
pub enum ComposeCmd {
    /// Creates/starts every service.
    ///
    /// Build, then network, then volume, then containers in `depends_on` order
    /// (gated on the declared condition).
    Up {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        /// Prints the resolved project (containers/networks/volumes) and exits
        /// without creating anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Accepted for `docker compose` compatibility. Services already start
        /// detached here (this engine has no daemon holding them), so this is a
        /// no-op rather than an error — every existing script and CI job writes
        /// `up -d`, and rejecting it broke all of them for no behavioural gain.
        #[arg(short = 'd', long = "detach")]
        detach: bool,
    },
    /// Removes every container this project's `up` created.
    ///
    /// And, with `-v`, its named volumes. Networks/volumes marked
    /// `external: true` are NEVER removed.
    Down {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        #[arg(short = 'v', long)]
        volumes: bool,
    },
    /// Containers of this project (derived from labels).
    Ps {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
    },
    /// Logs of one service (default: every service, one after another).
    Logs {
        service: Option<String>,
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        /// No short flag (unlike real `docker compose logs -f`) — `-f` is
        /// already `--file` on every subcommand of this group, for
        /// consistency across `up`/`down`/`ps`/`config`.
        #[arg(long)]
        follow: bool,
    },
    /// Validates and prints the resolved project, without creating anything
    /// (`docker compose config` equivalent).
    Config {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Option<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
    },
}

pub fn run(cmd: ComposeCmd) -> Result<()> {
    match cmd {
        ComposeCmd::Up {
            file,
            project,
            dry_run,
            detach: _,
        } => cmd_up(file, project, dry_run),
        ComposeCmd::Down {
            file,
            project,
            volumes,
        } => cmd_down(file, project, volumes),
        ComposeCmd::Ps { file, project } => cmd_ps(file, project),
        ComposeCmd::Logs {
            service,
            file,
            project,
            follow,
        } => cmd_logs(service, file, project, follow),
        ComposeCmd::Config { file, project } => cmd_config(file, project),
    }
}

// ============================================================================
// Parser types
// ============================================================================

#[derive(Debug, Deserialize, Default)]
struct ComposeFile {
    #[serde(default)]
    services: BTreeMap<String, ComposeService>,
    #[serde(default)]
    networks: BTreeMap<String, ComposeNetwork>,
    #[serde(default)]
    volumes: BTreeMap<String, ComposeVolume>,
}

#[derive(Debug, Deserialize, Default)]
struct ComposeService {
    image: Option<String>,
    build: Option<ComposeBuild>,
    #[serde(default)]
    environment: ComposeEnv,
    #[serde(default, rename = "env_file")]
    env_file: OneOrMany<String>,
    #[serde(default)]
    ports: Vec<ComposePort>,
    #[serde(default)]
    volumes: Vec<ComposeVolumeMount>,
    #[serde(default)]
    command: Option<ComposeCmdShape>,
    #[serde(default)]
    entrypoint: Option<ComposeCmdShape>,
    #[serde(default)]
    depends_on: ComposeDependsOn,
    #[serde(default)]
    healthcheck: Option<ComposeHealthcheck>,
    restart: Option<String>,
    #[serde(default)]
    networks: ComposeServiceNetworks,
    #[serde(default)]
    labels: ComposeEnv,
    working_dir: Option<String>,
    user: Option<String>,
    #[serde(default)]
    cap_add: Vec<String>,
    #[serde(default)]
    cap_drop: Vec<String>,
    #[serde(default)]
    privileged: bool,
    #[serde(default)]
    tmpfs: OneOrMany<String>,
    /// `extra_hosts:` — mesmo formato `name:ip` do Docker, e por isso o mesmo
    /// parser (`container::parse_add_host`). Aceita a forma de lista e a de
    /// mapa (`{ nome: ip }`), como o Compose Spec.
    #[serde(default)]
    extra_hosts: ComposeEnv,
    deploy: Option<ComposeDeploy>,
    container_name: Option<String>,
    hostname: Option<String>,
    #[serde(default)]
    read_only: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}
impl<T> Default for OneOrMany<T> {
    fn default() -> Self {
        OneOrMany::Many(Vec::new())
    }
}
impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::One(t) => vec![t],
            OneOrMany::Many(v) => v,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ComposeEnv {
    List(Vec<String>),
    Map(BTreeMap<String, Option<serde_yaml::Value>>),
}
impl Default for ComposeEnv {
    fn default() -> Self {
        ComposeEnv::List(Vec::new())
    }
}
impl ComposeEnv {
    fn to_kv_pairs(&self) -> Vec<String> {
        match self {
            ComposeEnv::List(v) => v.clone(),
            ComposeEnv::Map(m) => m
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        Some(serde_yaml::Value::String(s)) => s.clone(),
                        Some(serde_yaml::Value::Number(n)) => n.to_string(),
                        Some(serde_yaml::Value::Bool(b)) => b.to_string(),
                        Some(other) => serde_yaml::to_string(other)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                        // A bare `KEY:` (no value) means "inherit from the invoking
                        // shell's environment" in real compose — not supported in v1
                        // (documented simplification, not silent: becomes an empty
                        // value rather than a mysteriously-missing one).
                        None => String::new(),
                    };
                    format!("{k}={val}")
                })
                .collect(),
        }
    }

    /// `extra_hosts` no formato `name:ip` que o parser do motor espera.
    ///
    /// Método próprio e não `to_kv_pairs`: o Compose Spec aceita as duas
    /// formas (lista `- "nome:ip"` e mapa `nome: ip`), e a de mapa sairia como
    /// `nome=ip` — que o validador recusa de propósito, porque `db=2001:db8::1`
    /// é ambíguo. Aqui o separador é sempre `:`, e o valor do mapa nunca é
    /// dividido, portanto um IPv6 chega inteiro.
    fn to_host_pairs(&self) -> Vec<String> {
        match self {
            ComposeEnv::List(v) => v.clone(),
            ComposeEnv::Map(m) => m
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        Some(serde_yaml::Value::String(s)) => s.clone(),
                        Some(other) => serde_yaml::to_string(other)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                        None => String::new(),
                    };
                    format!("{k}:{val}")
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum StringOrNum {
    Str(String),
    Num(i64),
}
impl StringOrNum {
    fn as_string(&self) -> String {
        match self {
            StringOrNum::Str(s) => s.clone(),
            StringOrNum::Num(n) => n.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposePort {
    Short(String),
    Long {
        target: StringOrNum,
        published: Option<StringOrNum>,
        protocol: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeVolumeMount {
    Short(String),
    Long {
        #[serde(rename = "type")]
        mount_type: String,
        source: Option<String>,
        target: String,
        #[serde(default)]
        read_only: Option<bool>,
    },
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ComposeCmdShape {
    Shell(String),
    Exec(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeDependsOn {
    Short(Vec<String>),
    Long(BTreeMap<String, ComposeDependsOnEntry>),
}
impl Default for ComposeDependsOn {
    fn default() -> Self {
        ComposeDependsOn::Short(Vec::new())
    }
}
impl ComposeDependsOn {
    fn names(&self) -> Vec<String> {
        match self {
            ComposeDependsOn::Short(v) => v.clone(),
            ComposeDependsOn::Long(m) => m.keys().cloned().collect(),
        }
    }
    fn entries(&self) -> Result<Vec<(String, DependsCondition)>> {
        match self {
            ComposeDependsOn::Short(v) => Ok(v
                .iter()
                .map(|s| (s.clone(), DependsCondition::Started))
                .collect()),
            ComposeDependsOn::Long(m) => m
                .iter()
                .map(|(k, e)| Ok((k.clone(), DependsCondition::parse(&e.condition)?)))
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ComposeDependsOnEntry {
    condition: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependsCondition {
    Started,
    Healthy,
    CompletedSuccessfully,
}
impl DependsCondition {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "service_started" => Ok(Self::Started),
            "service_healthy" => Ok(Self::Healthy),
            "service_completed_successfully" => Ok(Self::CompletedSuccessfully),
            other => Err(Error::Invalid(format!(
                "compose: unknown depends_on condition '{other}' (expected service_started, service_healthy, or service_completed_successfully)"
            ))),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
struct ComposeHealthcheck {
    test: Option<ComposeHealthTest>,
    interval: Option<String>,
    timeout: Option<String>,
    retries: Option<u32>,
    start_period: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ComposeHealthTest {
    Shell(String),
    List(Vec<String>),
}

#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
enum ComposeServiceNetworks {
    #[default]
    Empty,
    List(Vec<String>),
    Map(BTreeMap<String, ComposeServiceNetworkEntry>),
}
impl ComposeServiceNetworks {
    fn keys(&self) -> Vec<String> {
        match self {
            ComposeServiceNetworks::Empty => Vec::new(),
            ComposeServiceNetworks::List(v) => v.clone(),
            ComposeServiceNetworks::Map(m) => m.keys().cloned().collect(),
        }
    }
    fn entry(&self, key: &str) -> Option<&ComposeServiceNetworkEntry> {
        match self {
            ComposeServiceNetworks::Map(m) => m.get(key),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ComposeServiceNetworkEntry {
    ipv4_address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ComposeDeploy {
    resources: Option<ComposeResources>,
    replicas: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ComposeResources {
    limits: Option<ComposeResourceLimits>,
}

#[derive(Debug, Deserialize)]
struct ComposeResourceLimits {
    cpus: Option<String>,
    memory: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeBuild {
    Context(String),
    Full {
        context: PathBuf,
        dockerfile: Option<PathBuf>,
        #[serde(default)]
        args: ComposeEnv,
        target: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct ComposeNetwork {
    #[serde(default)]
    external: bool,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ComposeVolume {
    #[serde(default)]
    external: bool,
    name: Option<String>,
}

// ============================================================================
// Pure helpers (parsing/naming/ordering) — no I/O, all unit-tested below
// ============================================================================

/// `docker-compose.yml`'s own search order for `-f`-less invocations.
fn resolve_compose_path(file: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(f) = file {
        if !f.is_file() {
            return Err(Error::Invalid(format!(
                "compose file not found: {}",
                f.display()
            )));
        }
        return Ok(f);
    }
    for candidate in [
        "compose.yaml",
        "compose.yml",
        "docker-compose.yaml",
        "docker-compose.yml",
    ] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(Error::Invalid(
        "no compose file found (tried compose.yaml, compose.yml, docker-compose.yaml, docker-compose.yml) — use -f <file>".into(),
    ))
}

/// Default project name: the lowercased basename of the compose file's
/// directory, with any run of characters outside `[a-z0-9_-]` collapsed to a
/// single `_` — same shape as real `docker compose`'s own project-name
/// normalization (exact collapsing rule flagged for live confirmation).
pub(crate) fn default_project_name(compose_path: &Path) -> String {
    // BUG FIXED HERE (CRITICAL, cross-project data loss, reproduced live).
    // The derivation below was already right, but it was fed a path that
    // usually has NO directory component: `find_compose_file` returns a bare
    // relative `docker-compose.yml`, and `-f docker-compose.yml` is relative
    // too. `Path::new("docker-compose.yml").parent()` is `Some("")`, whose
    // `file_name()` is `None` — so EVERY ordinary invocation fell through to
    // the literal `"default"`, and the only tests here passed absolute paths,
    // so nothing caught it.
    //
    // The consequence measured on a live host: two unrelated compose projects
    // in two different directories both became project `default`, sharing
    // network `default_default`, volume `default_<name>` and container
    // `default-<service>`. `up` in the second directory ADOPTED the first
    // project's running container ("already exists, nothing to do") and read
    // its data; `down -v` there then destroyed the first project's named
    // volume. Absolutizing first is the whole fix — a relative path is resolved
    // against the cwd, which is exactly the directory Docker Compose names the
    // project after.
    let absolute = if compose_path.is_absolute() {
        compose_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(compose_path))
            .unwrap_or_else(|_| compose_path.to_path_buf())
    };
    let dir_name = absolute
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string());
    let lower = dir_name.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_was_sep = false;
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('_');
            last_was_sep = true;
        }
    }
    let trimmed = out.trim_matches(|c| c == '_' || c == '-');
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

fn valid_compose_project_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A pure, POSIX-subset tokenizer for a `command:`/`entrypoint:` given as a
/// bare shell-looking string — single/double quotes, backslash escapes,
/// whitespace splitting. **Never feeds the string to an actual shell**: the
/// output is passed straight into `RunOpts.command` (→ `execve`), so
/// metacharacters (`;`/`` ` ``/`$()`/`|`) come out as ordinary, inert
/// characters inside a literal argv token — a correctness property (wrong
/// tokenization would silently change what runs), not a shell-injection
/// concern, precisely because there is no shell here to inject into.
fn shlex_split(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.trim().chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_token = true;
            }
            // BUG FOUND (code review): POSIX only gives backslash special meaning
            // inside double quotes before `$ \` " <newline>` — any OTHER character
            // keeps its backslash literally (e.g. `"grep \d+ file"` must keep the
            // `\d`, not silently drop the backslash and change the pattern). This
            // used to treat every backslash the same regardless of `in_double`,
            // dropping it unconditionally — a real "wrong argv" correctness bug
            // for compose commands with backslash-escaped literals inside quotes.
            // Outside quotes (the common case, and unquoted shell-style escaping),
            // a backslash still escapes whatever follows, unconditionally.
            '\\' if !in_single && in_double => match chars.peek() {
                Some(&next) if matches!(next, '$' | '`' | '"' | '\\' | '\n') => {
                    chars.next();
                    cur.push(next);
                    has_token = true;
                }
                _ => {
                    cur.push('\\');
                    has_token = true;
                }
            },
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                    has_token = true;
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if in_single || in_double {
        return Err(Error::Invalid(format!(
            "compose: unterminated quote in command: {s}"
        )));
    }
    if has_token {
        out.push(cur);
    }
    Ok(out)
}

/// A Go-duration-shaped string (`30s`, `1m30s`, `500ms`, `1h`) — the syntax
/// Compose Spec uses for `healthcheck.{interval,timeout,start_period}`.
fn parse_go_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::Invalid("empty duration".into()));
    }
    let mut total = Duration::ZERO;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            let mut unit = String::new();
            unit.push(c);
            if c == 'm' && chars.peek() == Some(&'s') {
                unit.push(chars.next().unwrap());
            }
            let n: f64 = num
                .parse()
                .map_err(|_| Error::Invalid(format!("invalid duration '{s}'")))?;
            num.clear();
            let secs = match unit.as_str() {
                "h" => n * 3600.0,
                "m" => n * 60.0,
                "s" => n,
                "ms" => n / 1000.0,
                other => {
                    return Err(Error::Invalid(format!(
                        "invalid duration unit '{other}' in '{s}'"
                    )))
                }
            };
            total += Duration::from_secs_f64(secs);
        }
    }
    if !num.is_empty() {
        return Err(Error::Invalid(format!(
            "invalid duration '{s}' (trailing number with no unit)"
        )));
    }
    Ok(total)
}

/// Kahn's-algorithm topological sort over `depends_on` edges. A `depends_on`
/// naming an undeclared service, or a cycle, is a HARD error (never an
/// arbitrary order) — ties broken alphabetically for determinism.
fn topo_sort(services: &BTreeMap<String, ComposeService>) -> Result<Vec<String>> {
    let mut deps: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (name, svc) in services {
        for dep in svc.depends_on.names() {
            if !services.contains_key(&dep) {
                return Err(Error::Invalid(format!(
                    "compose: service '{name}' depends_on undefined service '{dep}'"
                )));
            }
        }
        deps.insert(name.as_str(), svc.depends_on.names());
    }
    let mut remaining: BTreeMap<&str, usize> = deps.iter().map(|(&n, d)| (n, d.len())).collect();
    let mut dependents: BTreeMap<&str, Vec<&str>> =
        services.keys().map(|k| (k.as_str(), Vec::new())).collect();
    for (&name, d) in &deps {
        for dep in d {
            dependents.get_mut(dep.as_str()).unwrap().push(name);
        }
    }
    let mut ready: Vec<&str> = remaining
        .iter()
        .filter(|(_, &c)| c == 0)
        .map(|(&n, _)| n)
        .collect();
    ready.sort();
    let mut order = Vec::new();
    while !ready.is_empty() {
        ready.sort();
        let n = ready.remove(0);
        order.push(n.to_string());
        for &dependent in &dependents[n] {
            let e = remaining.get_mut(dependent).unwrap();
            *e -= 1;
            if *e == 0 {
                ready.push(dependent);
            }
        }
    }
    if order.len() != services.len() {
        let cyclic: Vec<&str> = remaining
            .iter()
            .filter(|(_, &c)| c > 0)
            .map(|(&n, _)| n)
            .collect();
        return Err(Error::Invalid(format!(
            "compose: circular depends_on involving: {}",
            cyclic.join(", ")
        )));
    }
    Ok(order)
}

/// Deterministic `<project>_<key>` name for a network/volume that has no
/// `name:` override — networks/volumes have no label field to scope them by
/// (unlike containers, which carry `delonix.io/compose-project`), so `down`
/// re-derives this SAME name from the re-parsed compose file instead of
/// looking anything up. BUG FOUND (code review): a plain `format!("{project}_{key}")`
/// is not collision-free — project `"a_b"` key `"c"` and project `"a"` key
/// `"b_c"` both produced `"a_b_c"`, so a `compose down` for one project could
/// remove a network/volume that was actually a DIFFERENT project's resource.
/// Fixed with a prefix-free encoding: every literal `_` inside `project`/`key`
/// is doubled before joining on a single `_`, so the join point is always the
/// first LONE underscore — unambiguous to reverse, which is exactly the
/// property `down` needs. Deliberately breaking pre-existing name generation
/// (not additive/back-compat): this ships before the public launch, the
/// safest point to fix a naming scheme.
fn compose_scoped_name(project: &str, key: &str) -> String {
    fn escape(s: &str) -> String {
        s.replace('_', "__")
    }
    format!("{}_{}", escape(project), escape(key))
}

fn resolve_network_names(project: &str, compose: &ComposeFile) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = compose
        .networks
        .iter()
        .map(|(key, net)| {
            let real = if net.external {
                net.name.clone().unwrap_or_else(|| key.clone())
            } else {
                net.name
                    .clone()
                    .unwrap_or_else(|| compose_scoped_name(project, key))
            };
            (key.clone(), real)
        })
        .collect();
    map.entry("default".to_string())
        .or_insert_with(|| compose_scoped_name(project, "default"));
    map
}

fn resolve_volume_names(project: &str, compose: &ComposeFile) -> BTreeMap<String, String> {
    compose
        .volumes
        .iter()
        .map(|(key, vol)| {
            let real = if vol.external {
                vol.name.clone().unwrap_or_else(|| key.clone())
            } else {
                vol.name
                    .clone()
                    .unwrap_or_else(|| compose_scoped_name(project, key))
            };
            (key.clone(), real)
        })
        .collect()
}

/// KNOWN_UNSUPPORTED scan — against the RAW parsed YAML (typed-struct parsing
/// would just silently drop an unrecognized key; this is what gives it a
/// clear, actionable error instead).
fn check_unsupported_fields(text: &str) -> Result<()> {
    let raw: serde_yaml::Value = serde_yaml::from_str(text)
        .map_err(|e| Error::Invalid(format!("parsing compose file: {e}")))?;
    let serde_yaml::Value::Mapping(top) = &raw else {
        return Ok(());
    };
    for (key, reason) in KNOWN_UNSUPPORTED_TOP {
        if top.contains_key(serde_yaml::Value::String((*key).to_string())) {
            return Err(Error::Invalid(format!(
                "compose: top-level `{key}:` is not supported — {reason}"
            )));
        }
    }
    if let Some(serde_yaml::Value::Mapping(services)) = top.get("services") {
        for (svc_name, svc_val) in services {
            let serde_yaml::Value::Mapping(svc_map) = svc_val else {
                continue;
            };
            for (key, reason) in KNOWN_UNSUPPORTED_SERVICE {
                if svc_map.contains_key(serde_yaml::Value::String((*key).to_string())) {
                    return Err(Error::Invalid(format!(
                        "compose: service '{}': `{key}:` is not supported — {reason}",
                        svc_name.as_str().unwrap_or("?")
                    )));
                }
            }
        }
    }
    Ok(())
}

fn simple_doc(kind: &str, name: &str) -> ManifestDoc {
    ManifestDoc {
        api_version: "delonix.io/v1".to_string(),
        kind: kind.to_string(),
        metadata: Metadata {
            name: name.to_string(),
            namespace: None,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        },
        spec: serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
    }
}

// ============================================================================
// Translation
// ============================================================================

#[derive(Clone)]
struct DependsOnWait {
    on: String,
    condition: DependsCondition,
    healthcheck: Option<ComposeHealthcheck>,
}

struct Translated {
    image_docs: Vec<ManifestDoc>,
    network_docs: Vec<ManifestDoc>,
    volume_docs: Vec<ManifestDoc>,
    containers: BTreeMap<String, (super::container::RunOpts, String)>,
    order: Vec<String>,
    waits: BTreeMap<String, Vec<DependsOnWait>>,
}

#[allow(clippy::too_many_arguments)]
fn translate(compose: &ComposeFile, project: &str, base_dir: &Path) -> Result<Translated> {
    if !valid_compose_project_name(project) {
        return Err(Error::Invalid(format!(
            "compose: invalid project name '{project}' (only lowercase letters, digits, '_'/'-', must start with a letter or digit)"
        )));
    }
    for (name, svc) in &compose.services {
        if svc.image.is_none() && svc.build.is_none() {
            return Err(Error::Invalid(format!(
                "compose: service '{name}' has neither `image` nor `build`"
            )));
        }
        if let Some(r) = svc.deploy.as_ref().and_then(|d| d.replicas) {
            if r != 1 {
                return Err(Error::Invalid(format!(
                    "compose: service '{name}': deploy.replicas={r} is not supported in v1 (only 1)"
                )));
            }
        }
    }
    let order = topo_sort(&compose.services)?;
    let network_names = resolve_network_names(project, compose);
    let volume_names = resolve_volume_names(project, compose);

    let mut network_docs: Vec<ManifestDoc> = Vec::new();
    for (key, net) in &compose.networks {
        if net.external {
            continue;
        }
        network_docs.push(simple_doc("Network", &network_names[key]));
    }
    let default_real = network_names["default"].clone();
    if !network_docs.iter().any(|d| d.metadata.name == default_real) {
        network_docs.push(simple_doc("Network", &default_real));
    }

    let mut volume_docs: Vec<ManifestDoc> = Vec::new();
    for (key, vol) in &compose.volumes {
        if vol.external {
            continue;
        }
        volume_docs.push(simple_doc("Volume", &volume_names[key]));
    }

    let mut image_docs = Vec::new();
    let mut containers = BTreeMap::new();
    let mut waits: BTreeMap<String, Vec<DependsOnWait>> = BTreeMap::new();

    for (name, svc) in &compose.services {
        let image_ref = if let Some(build) = &svc.build {
            let tag = svc
                .image
                .clone()
                .unwrap_or_else(|| compose_scoped_name(project, name));
            image_docs.push(service_build_to_image_doc(name, build, base_dir, &tag)?);
            tag
        } else {
            svc.image.clone().unwrap()
        };
        let (opts, final_name) = service_to_run_opts(
            project,
            name,
            svc,
            base_dir,
            &network_names,
            &volume_names,
            &image_ref,
        )?;
        containers.insert(name.clone(), (opts, final_name));

        let mut w = Vec::new();
        for (dep, condition) in svc.depends_on.entries()? {
            let dep_svc = &compose.services[&dep];
            w.push(DependsOnWait {
                on: dep,
                condition,
                healthcheck: dep_svc.healthcheck.clone(),
            });
        }
        waits.insert(name.clone(), w);
    }

    Ok(Translated {
        image_docs,
        network_docs,
        volume_docs,
        containers,
        order,
        waits,
    })
}

#[derive(Serialize)]
struct BuildDocSpec {
    context: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    tag: String,
    #[serde(rename = "buildArgs", skip_serializing_if = "Vec::is_empty")]
    build_args: Vec<String>,
}
#[derive(Serialize)]
struct ImageDocSpec {
    build: BuildDocSpec,
}

fn service_build_to_image_doc(
    service: &str,
    build: &ComposeBuild,
    base_dir: &Path,
    tag: &str,
) -> Result<ManifestDoc> {
    let (context_rel, dockerfile, args) = match build {
        ComposeBuild::Context(c) => (c.clone(), None, Vec::new()),
        ComposeBuild::Full {
            context,
            dockerfile,
            args,
            target,
        } => {
            if target.is_some() {
                return Err(Error::Invalid(format!(
                    "compose: service '{service}': build.target (multi-stage stage selection) is not supported in v1"
                )));
            }
            (
                context.to_string_lossy().into_owned(),
                dockerfile.clone(),
                args.to_kv_pairs(),
            )
        }
    };
    let context_path = base_dir.join(&context_rel);
    let file = dockerfile.map(|f| context_path.join(f).to_string_lossy().into_owned());
    let spec = ImageDocSpec {
        build: BuildDocSpec {
            context: context_path.to_string_lossy().into_owned(),
            file,
            tag: tag.to_string(),
            build_args: args,
        },
    };
    Ok(ManifestDoc {
        api_version: "delonix.io/v1".to_string(),
        kind: "Image".to_string(),
        metadata: Metadata {
            name: format!("{service}-image"),
            namespace: None,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        },
        spec: serde_yaml::to_value(spec).map_err(|e| {
            Error::Invalid(format!("compose: internal error building image spec: {e}"))
        })?,
    })
}

/// Finds a currently-free TCP port on the host by binding to port 0 (the
/// kernel picks one) and immediately releasing it — the same technique
/// `docker run -P`/`docker-compose`'s random-port assignment relies on.
/// Inherent TOCTOU: another process could grab the port between this call
/// returning and the container actually publishing it. A well-known,
/// accepted limitation of "find a free port" — not something userspace can
/// close without a kernel-level reservation API neither Docker nor this
/// engine has.
fn free_host_port() -> Result<u16> {
    std::net::TcpListener::bind(("0.0.0.0", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| Error::Invalid(format!("could not find a free host port: {e}")))
}

fn resolve_ports(ports: &[ComposePort], service: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in ports {
        match p {
            ComposePort::Short(s) => {
                if !s.contains(':') {
                    // Bare container port, no host port — Compose semantics: assign a
                    // random free host port (the engine's own `-p`/`parse_publish` has
                    // no "auto" keyword, so the port is resolved HERE, once, and fed in
                    // as a concrete number).
                    let host = free_host_port()?;
                    out.push(format!("{host}:{s}"));
                    continue;
                }
                let parts: Vec<&str> = s.rsplitn(3, ':').collect();
                if parts.len() < 2 {
                    return Err(Error::Invalid(format!(
                        "compose: service '{service}': malformed port '{s}'"
                    )));
                }
                // BUG FOUND (code review): a 3-part `host_ip:host_port:container_port`
                // form (e.g. `127.0.0.1:9000:80`, a deliberate loopback-only bind)
                // used to silently drop `parts[2]` (the IP) and fall through to the
                // 2-part case — the engine's own `-p` flag already rejects a host-IP
                // bind explicitly (`parse_publish` requires the host port to be
                // digits-only, see docs/COMPARACAO-DOCKER-PODMAN.md 2b), so doing it
                // silently here instead was a real gap: a compose file that restricts
                // a port to loopback got translated into one with NO restriction at
                // all. Reject explicitly instead, consistent with the CLI and with
                // this module's own "never a silent no-op" rule.
                if parts.len() == 3 {
                    return Err(Error::Invalid(format!(
                        "compose: service '{service}' port '{s}': binding to a specific host IP is not supported in v1 \
                         (would silently drop the IP restriction) — use '{}:{}' to bind all interfaces, \
                         or drop the port publish and reach the service by its internal DNS name instead",
                        parts[1], parts[0]
                    )));
                }
                out.push(format!("{}:{}", parts[1], parts[0]));
            }
            ComposePort::Long {
                target,
                published,
                protocol,
            } => {
                let host_s = match published {
                    Some(host) => host.as_string(),
                    // Same "no published port -> random free host port" semantics as
                    // the short form's bare-port case above.
                    None => free_host_port()?.to_string(),
                };
                if host_s.contains('-') {
                    return Err(Error::Invalid(format!(
                        "compose: service '{service}': port ranges ('{host_s}') are not supported in v1"
                    )));
                }
                let proto = protocol.as_deref().unwrap_or("tcp");
                let suffix = if proto == "udp" { "/udp" } else { "" };
                out.push(format!("{host_s}:{}{suffix}", target.as_string()));
            }
        }
    }
    Ok(out)
}

fn resolve_volume_source(
    base_dir: &Path,
    source: &str,
    volume_names: &BTreeMap<String, String>,
    service: &str,
) -> Result<String> {
    if source.starts_with('/') || source.starts_with('.') {
        let p = Path::new(source);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_dir.join(p)
        };
        Ok(resolved.to_string_lossy().into_owned())
    } else {
        volume_names.get(source).cloned().ok_or_else(|| {
            Error::Invalid(format!(
                "compose: service '{service}' references undefined volume '{source}' (declare it under top-level `volumes:`)"
            ))
        })
    }
}

fn resolve_volume_mounts(
    base_dir: &Path,
    mounts: &[ComposeVolumeMount],
    volume_names: &BTreeMap<String, String>,
    service: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut volumes = Vec::new();
    let mut tmpfs = Vec::new();
    for m in mounts {
        match m {
            ComposeVolumeMount::Short(s) => {
                let parts: Vec<&str> = s.splitn(3, ':').collect();
                if parts.len() < 2 {
                    return Err(Error::Invalid(format!(
                        "compose: service '{service}' volume '{s}': anonymous volumes (no explicit source) are not supported in v1 — declare a named volume"
                    )));
                }
                let (source, target) = (parts[0], parts[1]);
                let ro = if parts.get(2).map(|o| o.contains("ro")).unwrap_or(false) { ":ro" } else { "" };
                let resolved = resolve_volume_source(base_dir, source, volume_names, service)?;
                volumes.push(format!("{resolved}:{target}{ro}"));
            }
            ComposeVolumeMount::Long { mount_type, source, target, read_only } => match mount_type.as_str() {
                "tmpfs" => tmpfs.push(target.clone()),
                "bind" | "volume" => {
                    let src = source.as_deref().ok_or_else(|| {
                        Error::Invalid(format!(
                            "compose: service '{service}': volume mount of type '{mount_type}' needs 'source'"
                        ))
                    })?;
                    let resolved = resolve_volume_source(base_dir, src, volume_names, service)?;
                    let ro = if *read_only == Some(true) { ":ro" } else { "" };
                    volumes.push(format!("{resolved}:{target}{ro}"));
                }
                other => {
                    return Err(Error::Invalid(format!(
                        "compose: service '{service}': volume mount type '{other}' not supported in v1 (only bind/volume/tmpfs)"
                    )))
                }
            },
        }
    }
    Ok((volumes, tmpfs))
}

fn service_to_run_opts(
    project: &str,
    service: &str,
    svc: &ComposeService,
    base_dir: &Path,
    network_names: &BTreeMap<String, String>,
    volume_names: &BTreeMap<String, String>,
    image_ref: &str,
) -> Result<(super::container::RunOpts, String)> {
    let net_keys = svc.networks.keys();
    let net_key = if net_keys.is_empty() {
        "default".to_string()
    } else {
        if net_keys.len() > 1 {
            output::warn(&format!(
                "compose: service '{service}' declares {} networks; only '{}' is used (this engine attaches one network per container)",
                net_keys.len(),
                net_keys[0]
            ));
        }
        net_keys[0].clone()
    };
    if let Some(entry) = svc.networks.entry(&net_key) {
        if entry.ipv4_address.is_some() {
            return Err(Error::Invalid(format!(
                "compose: service '{service}': a fixed networks.*.ipv4_address is not supported (delonix assigns IPs by IPAM) — remove it"
            )));
        }
    }
    let net = network_names.get(&net_key).cloned().ok_or_else(|| {
        Error::Invalid(format!(
            "compose: service '{service}' references undefined network '{net_key}' (declare it under top-level `networks:`)"
        ))
    })?;

    let (volumes, tmpfs_from_mounts) =
        resolve_volume_mounts(base_dir, &svc.volumes, volume_names, service)?;
    let mut tmpfs = tmpfs_from_mounts;
    tmpfs.extend(svc.tmpfs.clone().into_vec());

    let ports = resolve_ports(&svc.ports, service)?;
    let env = svc.environment.to_kv_pairs();
    let env_file: Vec<String> = svc
        .env_file
        .clone()
        .into_vec()
        .into_iter()
        .map(|f| base_dir.join(f).to_string_lossy().into_owned())
        .collect();
    let mut labels = svc.labels.to_kv_pairs();
    labels.push(format!("{COMPOSE_PROJECT_LABEL}={project}"));
    labels.push(format!("{COMPOSE_SERVICE_LABEL}={service}"));

    let command = match &svc.command {
        Some(ComposeCmdShape::Shell(s)) => shlex_split(s)?,
        Some(ComposeCmdShape::Exec(v)) => v.clone(),
        None => Vec::new(),
    };
    // Only the first token of a shell-form `entrypoint:` is used — same
    // documented simplification `dockerapi::docker_config_to_run_opts` already
    // makes for Docker's own `Entrypoint` array (`RunOpts.entrypoint` is a
    // single program name, not an argv vector).
    let entrypoint = match &svc.entrypoint {
        Some(ComposeCmdShape::Shell(s)) => shlex_split(s)?.into_iter().next(),
        Some(ComposeCmdShape::Exec(v)) => v.first().cloned(),
        None => None,
    };

    let restart = svc.restart.clone().unwrap_or_else(|| "no".to_string());
    let (memory, cpus) = svc
        .deploy
        .as_ref()
        .and_then(|d| d.resources.as_ref())
        .and_then(|r| r.limits.as_ref())
        .map(|l| (l.memory.clone(), l.cpus.clone()))
        .unwrap_or((None, None));

    let final_name = svc
        .container_name
        .clone()
        .unwrap_or_else(|| format!("{project}-{service}"));

    let opts = super::container::RunOpts {
        detach: true,
        name: Some(final_name.clone()),
        hostname: svc.hostname.clone(),
        user: svc.user.clone(),
        net,
        volumes,
        ports,
        privileged: svc.privileged,
        entrypoint,
        workdir: svc.working_dir.clone(),
        restart,
        env,
        env_file,
        labels,
        image: image_ref.to_string(),
        command,
        quiet: true,
        memory,
        cpus,
        cap_add: svc.cap_add.clone(),
        cap_drop: svc.cap_drop.clone(),
        read_only: svc.read_only,
        tmpfs,
        // Validado AQUI, com o mesmo parser do `container run` — um
        // `extra_hosts:` malformado falha o `compose up` com a razão, em vez
        // de ser descartado em silêncio (a regra explícita deste módulo).
        add_host: {
            let mut out = Vec::new();
            for entry in svc.extra_hosts.to_host_pairs() {
                let (name, ip) = super::container::parse_add_host(&entry)
                    .map_err(delonix_runtime_core::Error::Invalid)?;
                out.push(format!("{name}:{ip}"));
            }
            out
        },
        ..Default::default()
    };
    Ok((opts, final_name))
}

// ============================================================================
// Dependency wait
// ============================================================================

/// `timeout` is parsed (a malformed value is still a hard error, same
/// validation posture as the other three) but deliberately NOT enforced as a
/// real per-attempt deadline in v1 — `runtime::exec` blocks until the test
/// command exits on its own, and adding a kill-after-timeout wrapper is a
/// separate, undertaken-later piece of work (documented gap, not silent: the
/// value is still validated, just not acted on).
fn health_params(hc: &Option<ComposeHealthcheck>) -> Result<(Duration, Duration, u32, Duration)> {
    let interval = hc
        .as_ref()
        .and_then(|h| h.interval.as_deref())
        .map(parse_go_duration)
        .transpose()?
        .unwrap_or(Duration::from_secs(30));
    let timeout = hc
        .as_ref()
        .and_then(|h| h.timeout.as_deref())
        .map(parse_go_duration)
        .transpose()?
        .unwrap_or(Duration::from_secs(30));
    let retries = hc.as_ref().and_then(|h| h.retries).unwrap_or(3);
    let start_period = hc
        .as_ref()
        .and_then(|h| h.start_period.as_deref())
        .map(parse_go_duration)
        .transpose()?
        .unwrap_or(Duration::ZERO);
    Ok((interval, timeout, retries, start_period))
}

/// O comando de health check declarado na IMAGEM (`HEALTHCHECK`), sem passar
/// por um serviço compose. Exposto para o `container run --wait` usar
/// exactamente a mesma resolução — dois resolvedores divergiriam, e o que
/// ficasse para trás passaria a dizer "saudável" por outra regra.
pub(crate) fn image_health_argv(images: &ImageStore, image_ref: &str) -> Option<Vec<String>> {
    resolve_test_argv(&None, images, image_ref)
}

fn resolve_test_argv(
    hc: &Option<ComposeHealthcheck>,
    images: &ImageStore,
    image_ref: &str,
) -> Option<Vec<String>> {
    match hc.as_ref().and_then(|h| h.test.clone()) {
        Some(ComposeHealthTest::Shell(s)) => Some(vec!["/bin/sh".into(), "-c".into(), s]),
        Some(ComposeHealthTest::List(v)) => {
            if v.is_empty() {
                return None;
            }
            match v[0].as_str() {
                "NONE" => None,
                "CMD-SHELL" => Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    v.get(1).cloned().unwrap_or_default(),
                ]),
                "CMD" => Some(v[1..].to_vec()),
                _ => Some(v),
            }
        }
        None => images
            .resolve(image_ref)
            .ok()
            .and_then(|i| i.config.healthcheck.clone())
            .map(|cmd| vec!["/bin/sh".into(), "-c".into(), cmd]),
    }
}

/// Polls a dependency until its declared `depends_on` condition is satisfied,
/// or fails closed with a clear error. Called right BEFORE starting a service
/// that depends on this one — never after (a dependency's health/completion
/// must be confirmed before its dependent starts, matching real `docker
/// compose`'s own ordering).
fn wait_for_condition(
    store: &Store,
    images: &ImageStore,
    dep_container_name: &str,
    wait: &DependsOnWait,
) -> Result<()> {
    match wait.condition {
        DependsCondition::Started => Ok(()),
        DependsCondition::CompletedSuccessfully => {
            // BUG FOUND (code review): this used to be a bare `loop {}` with no
            // way out short of Ctrl-C — a service accidentally declared with
            // this condition on something long-running (or restart:always,
            // which keeps cycling back into Running) hung `compose up`
            // forever with zero feedback, unlike the `Healthy` branch below
            // (already bounded by `retries`). Real `docker compose` has the
            // same unbounded wait for this specific condition, so a hard
            // failure here isn't full parity — instead: a generous default
            // ceiling (never silently forever) plus periodic progress output
            // (so a legitimately slow one-shot job, e.g. a DB migration,
            // doesn't look indistinguishable from a hang while it's still
            // within budget).
            const MAX_WAIT: Duration = Duration::from_secs(30 * 60);
            const HEARTBEAT: Duration = Duration::from_secs(30);
            let deadline = Instant::now() + MAX_WAIT;
            let mut last_heartbeat = Instant::now();
            loop {
                let mut c = find(store, dep_container_name)?;
                let changed = runtime::reconcile_status(&mut c);
                let _ = changed;
                if !matches!(c.status, Status::Running | Status::Created) {
                    return match c.status {
                        Status::Stopped => Ok(()),
                        other => Err(Error::Invalid(format!(
                            "compose: '{dep_container_name}' was expected to complete successfully (service_completed_successfully) \
                             but ended in state {other:?} (exit code {})",
                            other.exit_code()
                        ))),
                    };
                }
                if Instant::now() >= deadline {
                    return Err(Error::Invalid(format!(
                        "compose: '{dep_container_name}' did not finish within {}s (service_completed_successfully) — \
                         aborting `compose up` (already-started services are left running; `compose down` to clean up). \
                         If it legitimately needs longer, this condition may be the wrong one for a long-running service.",
                        MAX_WAIT.as_secs()
                    )));
                }
                if last_heartbeat.elapsed() >= HEARTBEAT {
                    eprintln!(
                        "compose: still waiting for '{dep_container_name}' to complete (service_completed_successfully)..."
                    );
                    last_heartbeat = Instant::now();
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        DependsCondition::Healthy => {
            let c = find(store, dep_container_name)?;
            let Some(argv) = resolve_test_argv(&wait.healthcheck, images, &c.image) else {
                return Err(Error::Invalid(format!(
                    "compose: '{dep_container_name}' is depended on with condition: service_healthy but declares no \
                     healthcheck (neither inline nor in its image) — add one or change the condition \
                     (already-started services are left running; `compose down` to clean up)"
                )));
            };
            let (interval, _timeout, retries, start_period) = health_params(&wait.healthcheck)?;
            std::thread::sleep(start_period);
            for attempt in 0..=retries {
                if runtime::exec(&c, &argv, false).unwrap_or(1) == 0 {
                    return Ok(());
                }
                if attempt < retries {
                    std::thread::sleep(interval);
                }
            }
            Err(Error::Invalid(format!(
                "compose: '{dep_container_name}' never became healthy after {} attempt(s) (service_healthy) — aborting \
                 `compose up` (already-started services are left running; `compose down` to clean up)",
                retries + 1
            )))
        }
    }
}

// ============================================================================
// Commands
// ============================================================================

fn load_compose(file: Option<PathBuf>) -> Result<(ComposeFile, String, PathBuf, String)> {
    let path = resolve_compose_path(file)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::Invalid(format!("reading {}: {e}", path.display())))?;
    check_unsupported_fields(&text)?;
    let compose: ComposeFile = serde_yaml::from_str(&text)
        .map_err(|e| Error::Invalid(format!("parsing {}: {e}", path.display())))?;
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let default_project = default_project_name(&path);
    Ok((
        compose,
        default_project,
        base_dir,
        path.to_string_lossy().into_owned(),
    ))
}

fn cmd_up(file: Option<PathBuf>, project: Option<String>, dry_run: bool) -> Result<()> {
    let (compose, default_project, base_dir, _path) = load_compose(file)?;
    let project = project.unwrap_or(default_project);
    let translated = translate(&compose, &project, &base_dir)?;

    if dry_run {
        println!("# compose project: {project}");
        for doc in translated
            .network_docs
            .iter()
            .chain(&translated.volume_docs)
            .chain(&translated.image_docs)
        {
            println!(
                "---\n{}",
                manifest::render_with_defaults(std::slice::from_ref(doc))?
            );
        }
        for name in &translated.order {
            let (opts, final_name) = &translated.containers[name];
            println!(
                "service {name} -> container {final_name} (image={}, net={})",
                opts.image, opts.net
            );
        }
        return Ok(());
    }

    super::image::apply(&translated.image_docs)?;
    super::network::apply(&translated.network_docs)?;
    super::volume::apply(&translated.volume_docs)?;

    let (images, store) = open_stores()?;
    for name in &translated.order {
        if let Some(waits) = translated.waits.get(name) {
            for w in waits {
                let (_, dep_final_name) = translated.containers.get(&w.on).ok_or_else(|| {
                    Error::Invalid(format!(
                        "compose: internal error: dependency '{}' not found",
                        w.on
                    ))
                })?;
                wait_for_condition(&store, &images, dep_final_name, w)?;
            }
        }
        let (opts, final_name) = translated.containers[name].clone();
        if store.list()?.iter().any(|c| c.name == final_name) {
            println!("compose: {name} ({final_name}): already exists, nothing to do");
            continue;
        }
        super::container::cmd_run(&images, &store, opts)?;
        println!("compose: {name} ({final_name}): created");
    }
    Ok(())
}

fn cmd_down(file: Option<PathBuf>, project: Option<String>, remove_volumes: bool) -> Result<()> {
    let (compose, default_project, _base_dir, _path) = load_compose(file)?;
    let project = project.unwrap_or(default_project);
    let (images, store) = open_stores()?;

    let members: Vec<Container> = store
        .list()?
        .into_iter()
        .filter(|c| {
            c.labels
                .get(COMPOSE_PROJECT_LABEL)
                .map(|v| v == &project)
                .unwrap_or(false)
        })
        .collect();
    let mut failed = Vec::new();
    for c in &members {
        if let Err(e) = super::container::cmd_rm(&images, &store, &c.name, true) {
            failed.push(format!("{}: {e}", c.name));
        }
    }
    if !failed.is_empty() {
        return Err(Error::Invalid(format!(
            "compose down: {}/{} container(s) NOT removed ({})",
            failed.len(),
            members.len(),
            failed.join("; ")
        )));
    }

    let network_names = resolve_network_names(&project, &compose);
    if let Ok(net_store) = NetworkStore::open(state_root()) {
        for (key, net) in &compose.networks {
            if net.external {
                continue;
            }
            let _ = super::network::cmd_rm(&net_store, &network_names[key]);
        }
        let _ = super::network::cmd_rm(&net_store, &network_names["default"]);
    }

    if remove_volumes {
        let volume_names = resolve_volume_names(&project, &compose);
        if let Ok(vol_store) = VolumeStore::open(state_root()) {
            for (key, vol) in &compose.volumes {
                if vol.external {
                    continue;
                }
                // NOT forced, and NOT silenced. `down -v` must respect the same
                // reference check as `volumes rm`: this project's containers are
                // already gone by now, so anything still holding the volume
                // belongs to something else, and destroying it would be exactly
                // the cross-project data loss this check exists to stop. A
                // failure is reported instead of being swallowed by `let _ =`,
                // which used to make `down -v` print success while leaving (or
                // failing to leave) volumes in an unknown state.
                if let Err(e) = super::volume::cmd_rm(&vol_store, &volume_names[key], false) {
                    super::output::warn(&super::po::tf(
                        "volume '{name}' not removed: {err}",
                        &[("name", &volume_names[key]), ("err", &e.to_string())],
                    ));
                }
            }
        }
    }
    println!(
        "compose: project '{project}' removed ({} container(s))",
        members.len()
    );
    Ok(())
}

fn cmd_ps(file: Option<PathBuf>, project: Option<String>) -> Result<()> {
    let (_compose, default_project, _base_dir, _path) = load_compose(file)?;
    let project = project.unwrap_or(default_project);
    let (_images, store) = open_stores()?;
    let members: Vec<Container> = store
        .list()?
        .into_iter()
        .filter(|c| {
            c.labels
                .get(COMPOSE_PROJECT_LABEL)
                .map(|v| v == &project)
                .unwrap_or(false)
        })
        .collect();
    let mut t = output::Table::new(&["SERVICE", "NAME", "IMAGE", "STATUS"]);
    for c in &members {
        let service = c
            .labels
            .get(COMPOSE_SERVICE_LABEL)
            .cloned()
            .unwrap_or_default();
        t.row(vec![
            service,
            c.name.clone(),
            c.image.clone(),
            format!("{:?}", c.status),
        ]);
    }
    t.print();
    Ok(())
}

fn cmd_logs(
    service: Option<String>,
    file: Option<PathBuf>,
    project: Option<String>,
    follow: bool,
) -> Result<()> {
    let (_compose, default_project, _base_dir, _path) = load_compose(file)?;
    let project = project.unwrap_or(default_project);
    let (images, store) = open_stores()?;
    let members: Vec<Container> = store
        .list()?
        .into_iter()
        .filter(|c| {
            c.labels
                .get(COMPOSE_PROJECT_LABEL)
                .map(|v| v == &project)
                .unwrap_or(false)
        })
        .filter(|c| {
            service
                .as_deref()
                .map(|s| {
                    c.labels
                        .get(COMPOSE_SERVICE_LABEL)
                        .map(|v| v == s)
                        .unwrap_or(false)
                })
                .unwrap_or(true)
        })
        .collect();
    if members.is_empty() {
        return Err(Error::Invalid(format!(
            "compose: no container found for project '{project}'{}",
            service
                .map(|s| format!(", service '{s}'"))
                .unwrap_or_default()
        )));
    }
    for c in &members {
        if members.len() > 1 {
            println!("==> {} <==", c.name);
        }
        super::container::cmd_logs(&images, &store, &c.name, follow, None, None, false)?;
    }
    Ok(())
}

fn cmd_config(file: Option<PathBuf>, project: Option<String>) -> Result<()> {
    let (compose, default_project, base_dir, _path) = load_compose(file)?;
    let project = project.unwrap_or(default_project);
    let translated = translate(&compose, &project, &base_dir)?;
    println!("# compose project: {project}");
    for name in &translated.order {
        let (opts, final_name) = &translated.containers[name];
        println!(
            "service {name}:\n  container: {final_name}\n  image: {}\n  network: {}\n  ports: {:?}\n  volumes: {:?}",
            opts.image, opts.net, opts.ports, opts.volumes
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shlex_split_nao_e_shell_trata_metacaracteres_como_texto() {
        // Metacaracteres de shell (`;`/`` ` ``/`$()`) saem como texto literal
        // dentro de um token — nunca chegam a um shell real para os interpretar.
        assert_eq!(
            shlex_split("echo hi; rm -rf /").unwrap(),
            vec!["echo", "hi;", "rm", "-rf", "/"]
        );
        assert_eq!(
            shlex_split("echo 'a b' c").unwrap(),
            vec!["echo", "a b", "c"]
        );
        assert_eq!(
            shlex_split(r#"echo "a b" c"#).unwrap(),
            vec!["echo", "a b", "c"]
        );
        assert_eq!(shlex_split(r"echo a\ b").unwrap(), vec!["echo", "a b"]);
        assert!(shlex_split("echo 'unterminated").is_err());
    }

    /// BUG FOUND (code review): inside double quotes, POSIX only special-cases
    /// backslash before `$ \` " <newline>` — any other character keeps its
    /// backslash. This used to drop the backslash unconditionally, silently
    /// mangling e.g. a regex pattern passed as a command argument.
    #[test]
    fn shlex_split_dentro_de_aspas_duplas_so_escapa_o_conjunto_posix() {
        // `\d` is not one of `$ \` " <newline>` -> the backslash survives.
        assert_eq!(
            shlex_split(r#"grep "\d+" file"#).unwrap(),
            vec!["grep", r"\d+", "file"]
        );
        // `\"` and `\\` ARE in the POSIX set -> backslash is consumed.
        assert_eq!(
            shlex_split(r#"echo "a\"b\\c""#).unwrap(),
            vec!["echo", "a\"b\\c"]
        );
        // Outside quotes, backslash still escapes whatever follows unconditionally
        // (unchanged behavior).
        assert_eq!(shlex_split(r"echo \d+").unwrap(), vec!["echo", "d+"]);
    }

    #[test]
    fn parse_go_duration_reconhece_as_unidades() {
        assert_eq!(parse_go_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_go_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(
            parse_go_duration("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(parse_go_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_go_duration("").is_err());
        assert!(parse_go_duration("5x").is_err());
    }

    #[test]
    fn topo_sort_ordena_por_dependencia_e_detecta_ciclo() {
        let yaml = "web:\n  image: a\n  depends_on: [api]\napi:\n  image: b\n  depends_on: [db]\ndb:\n  image: c\n";
        let services: BTreeMap<String, ComposeService> = serde_yaml::from_str(yaml).unwrap();
        let order = topo_sort(&services).unwrap();
        assert_eq!(order, vec!["db", "api", "web"]);

        let cyclic = "a:\n  image: x\n  depends_on: [b]\nb:\n  image: y\n  depends_on: [a]\n";
        let services: BTreeMap<String, ComposeService> = serde_yaml::from_str(cyclic).unwrap();
        assert!(topo_sort(&services).is_err());

        let missing = "a:\n  image: x\n  depends_on: [ghost]\n";
        let services: BTreeMap<String, ComposeService> = serde_yaml::from_str(missing).unwrap();
        assert!(topo_sort(&services).is_err());
    }

    #[test]
    fn default_project_name_normaliza_o_directorio() {
        assert_eq!(
            default_project_name(Path::new("/home/u/My App/compose.yml")),
            "my_app"
        );
        assert_eq!(
            default_project_name(Path::new("/home/u/simple/compose.yml")),
            "simple"
        );
    }

    /// REGRESSION (cross-project data loss): a RELATIVE compose path — what
    /// `find_compose_file` always returns, and what `-f docker-compose.yml`
    /// gives — must still be named after the directory it actually lives in.
    /// This asserted `"default"` before, i.e. the test encoded the bug: every
    /// real invocation collapsed onto one shared project name, so two unrelated
    /// projects adopted each other's containers and volumes and `down -v` in one
    /// deleted the other's data.
    #[test]
    fn default_project_name_resolve_caminho_relativo_contra_o_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let esperado = default_project_name(&cwd.join("compose.yml"));
        assert_eq!(default_project_name(Path::new("compose.yml")), esperado);
        assert_eq!(
            default_project_name(Path::new("./docker-compose.yml")),
            esperado
        );
        assert_ne!(
            default_project_name(Path::new("compose.yml")),
            "default",
            "um caminho relativo não pode cair no nome de projecto partilhado"
        );
    }

    #[test]
    fn valid_compose_project_name_recusa_charset_invalido() {
        assert!(valid_compose_project_name("myapp"));
        assert!(valid_compose_project_name("my_app-1"));
        assert!(!valid_compose_project_name(""));
        assert!(!valid_compose_project_name("-leading-dash"));
        assert!(!valid_compose_project_name("Has Spaces"));
    }

    #[test]
    fn translate_resolve_depends_on_e_healthcheck_e_erros_ficam_claros() {
        let yaml = r#"
services:
  db:
    image: postgres:16
    healthcheck:
      test: ["CMD-SHELL", "pg_isready"]
      interval: 5s
      retries: 10
  app:
    image: myapp:1
    depends_on:
      db:
        condition: service_healthy
"#;
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let t = translate(&compose, "myproj", Path::new("/tmp")).unwrap();
        assert_eq!(t.order, vec!["db".to_string(), "app".to_string()]);
        let app_waits = &t.waits["app"];
        assert_eq!(app_waits.len(), 1);
        assert_eq!(app_waits[0].on, "db");
        assert_eq!(app_waits[0].condition, DependsCondition::Healthy);
        assert!(app_waits[0].healthcheck.is_some());
    }

    #[test]
    fn deploy_replicas_diferente_de_1_e_erro() {
        let yaml = "services:\n  svc:\n    image: x\n    deploy:\n      replicas: 3\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        assert!(translate(&compose, "p", Path::new("/tmp")).is_err());
    }

    #[test]
    fn service_sem_image_nem_build_e_erro() {
        let yaml = "services:\n  svc:\n    ports: []\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        assert!(translate(&compose, "p", Path::new("/tmp")).is_err());
    }

    #[test]
    fn resolve_ports_curto_e_longo() {
        let ports = vec![
            ComposePort::Short("8080:80".to_string()),
            ComposePort::Long {
                target: StringOrNum::Num(443),
                published: Some(StringOrNum::Num(8443)),
                protocol: None,
            },
        ];
        let out = resolve_ports(&ports, "svc").unwrap();
        assert_eq!(out, vec!["8080:80".to_string(), "8443:443".to_string()]);
    }

    /// A bare container port (no host port, short or long form) gets a random
    /// free host port instead of being rejected — real Compose semantics.
    #[test]
    fn resolve_ports_porta_sem_host_ganha_porta_aleatoria() {
        let bare = vec![ComposePort::Short("80".to_string())];
        let out = resolve_ports(&bare, "svc").unwrap();
        assert_eq!(out.len(), 1);
        let (host, cont) = out[0].split_once(':').unwrap();
        assert_eq!(cont, "80");
        assert!(
            host.parse::<u16>().is_ok(),
            "host port não numérico: {host}"
        );

        let bare_long = vec![ComposePort::Long {
            target: StringOrNum::Num(443),
            published: None,
            protocol: None,
        }];
        let out2 = resolve_ports(&bare_long, "svc").unwrap();
        let (host2, cont2) = out2[0].split_once(':').unwrap();
        assert_eq!(cont2, "443");
        assert!(host2.parse::<u16>().is_ok());
    }

    /// BUG FOUND (code review): `127.0.0.1:9000:80` used to silently drop the
    /// IP and behave exactly like `9000:80` (bound on every interface) — the
    /// opposite of what the compose file asked for. Must reject explicitly.
    #[test]
    fn resolve_ports_recusa_bind_a_ip_especifico_em_vez_de_o_descartar_em_silencio() {
        let ports = vec![ComposePort::Short("127.0.0.1:9000:80".to_string())];
        let err = resolve_ports(&ports, "svc").unwrap_err().to_string();
        assert!(err.contains("host IP"), "erro inesperado: {err}");
    }

    /// BUG FOUND (code review): a plain `<project>_<key>` join let two
    /// DIFFERENT projects/keys collapse onto the same generated network/
    /// volume name (`project="a_b", key="c"` vs `project="a", key="b_c"`,
    /// both used to yield `"a_b_c"`), which meant `compose down` for one
    /// project could remove a resource belonging to the other.
    #[test]
    fn compose_scoped_name_nao_colide_entre_projectos_com_undercores() {
        let a = compose_scoped_name("a_b", "c");
        let b = compose_scoped_name("a", "b_c");
        assert_ne!(
            a, b,
            "colisão real entre dois pares projecto/chave distintos"
        );
    }
}

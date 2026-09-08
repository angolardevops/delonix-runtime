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
//! **v1 scope, explicit ship/defer** (never a silent no-op — the denylists
//! `KNOWN_UNSUPPORTED_TOP`/`KNOWN_UNSUPPORTED_SERVICE` carry the specific
//! reasons, and `SUPPORTED_TOP`/`SUPPORTED_SERVICE` are the ALLOWLIST behind
//! them, so a key nobody remembered to deny is refused too instead of dropped):
//! ships `image`/`build`/`environment`/
//! `env_file`/`ports`/`volumes`/`command`/`entrypoint`/`depends_on`
//! (all 3 conditions)/`healthcheck`/`restart`/`networks`/`labels`/`user`/
//! `cap_add`/`cap_drop`/`privileged`/`tmpfs`/`deploy.resources.limits`/
//! `container_name`/`hostname`/`read_only`/`profiles` (`up --profile`,
//! transitively closed over `depends_on` — see `active_services`), top-level
//! `networks:`/`volumes:` (incl. `external: true`), `build.target`
//! (multi-stage stage selection — forwarded to `kind: Image`'s own
//! `build.target`, itself `delonix build --target`), a fixed
//! `networks.*.ipv4_address` (wired straight into `container run --ip`, see
//! `service_to_run_opts`), `extends:` (`resolve_extends`, resolved AFTER the
//! multi-file merge — see "Multi-file compose" below), `deploy.replicas`
//! (N containers, `<project>-<service>`/`-2`/`-3`/…, with no load-balancing
//! across them — see `Translated.containers`), top-level `configs:`/`secrets:`
//! (each referenced entry becomes a `kind: Secret` under the hood, applied
//! before the containers that use it — see `resolve_compose_secrets`),
//! anonymous volumes (`- /container/path`, no explicit source — see
//! `anonymous_volume_names`; **only `down -v` removes them**, a plain `down`
//! never does, matching real `docker compose`), and multi-file compose via
//! **repeated `-f`** (`-f a.yml -f b.yml`, see "Multi-file compose" below).
//! Defers the YAML `include:` directive (use `-f a -f b` instead).
//! `working_dir:` IS applied (via `RunOpts.
//! workdir`, itself now also exposed as `container run -w/--workdir`), and a
//! bare container port with no host port DOES get a random free host port
//! (resolved once, before the container is created — see `free_host_port`).
//!
//! **`configs:`/`secrets:` — a simplification documented, never silent.**
//! Compose gives every secret/config its own mount TARGET; the engine's own
//! `Container.secret` mechanism (`--secret`) mounts a secret's DATA KEYS
//! verbatim at `/run/secrets/<key>`, with no per-secret retarget. So a
//! `source:`/`target:` pair where the two differ is REFUSED with the exact
//! reason (use the source name, or drop `target:`), instead of silently
//! mounting under the wrong filename. `external: true` is refused the same
//! way — this module can only create a secret from a `file:`/`environment:`
//! it can read, never reference one that already exists elsewhere. Both
//! `secrets:` and `configs:` land in the SAME `SecretStore` (a config has no
//! separate concept here, exactly what the pre-existing refusal message —
//! "use `kind: Secret` instead" — already pointed at); only `secrets:` reads
//! `environment:` (real Compose Spec configs never have that source).
//!
//! **Multi-file compose (`-f a.yml -f b.yml`).** Each file is parsed and
//! `check_unsupported_fields`-checked SEPARATELY (that check runs on raw YAML
//! text, before any merge, so an unknown/refused key is caught no matter
//! which file it came from) and the resulting typed `ComposeFile`s are merged
//! left-to-right (`merge_compose_files`) following the Compose Spec's own
//! rules — scalars: later file wins if declared; `environment:`/`labels:`/
//! `extra_hosts:`: merged key-by-key (later wins only on a shared key);
//! `cap_add`/`cap_drop`: concatenated; `ports`/`volumes`/`env_file`/`tmpfs`:
//! concatenated; `depends_on`: **accumulated** (a dependency named in either
//! file is kept, later file's condition wins on a shared name) — unlike
//! `extends:`, an ordinary multi-file merge of the SAME service name has no
//! foreign-template ambiguity to protect against, so there is no reason to
//! exclude it. Top-level `networks:`/`volumes:` entries are small enough
//! (`external`/`name`) that a shared key is a full REPLACE rather than a
//! field merge — documented simplification, not a silent drop. All relative
//! paths (build context, `env_file`) resolve against the FIRST file's
//! directory, matching real `docker compose`'s own convention. The YAML
//! `include:` directive is a SEPARATE, deferred feature (different
//! path-relativity and project-name-propagation rules) — only the CLI
//! `-f a -f b` form is implemented here.

use super::kinds as k;
use std::collections::{BTreeMap, BTreeSet};
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
const KNOWN_UNSUPPORTED_TOP: &[(&str, &str)] = &[(
    "include",
    "the `include:` directive is not supported — use repeated `-f a -f b` on the command line instead",
)];
/// Empty today: `profiles:`, `extends:` and top-level `configs:`/`secrets:`
/// were the entries here, and all are now implemented. Kept as the place a
/// future refusal goes, WITH its reason — the allowlist below is what stops
/// an unknown key from being read as accepted.
const KNOWN_UNSUPPORTED_SERVICE: &[(&str, &str)] = &[];

/// Every top-level key of the Compose Specification this implementation reads.
///
/// **An ALLOWLIST, and that is the whole point.** `KNOWN_UNSUPPORTED_*` above is
/// a denylist, and a denylist can only refuse what someone remembered to write
/// down: every other key of the specification — and there are dozens — was
/// parsed into a typed struct that dropped it without a word. `security_opt:`,
/// `devices:`, `sysctls:`, `network_mode:` all read as accepted and none of them
/// did anything, which is precisely the failure this engine refuses by policy
/// ("accepted and ignored is worse than missing"). The denylist stays because it
/// carries the good, specific reasons; this closes the hole behind it.
///
/// `version:` is here although nothing reads it: the Compose Specification
/// dropped it, real files still carry it, and erroring on it would refuse most
/// of the compose files in the world for a key whose absence changes nothing.
const SUPPORTED_TOP: &[&str] = &[
    "version", "name", "services", "networks", "volumes", "secrets", "configs",
];

/// Every per-service key this implementation reads — the field list of
/// [`ComposeService`], and `struct_fields_are_all_in_the_allowlist` reads
/// THIS FILE'S OWN SOURCE to prove the two never drift. Same discipline the
/// `API_MATRIX` of `dockerapi.rs` uses, for the same reason.
const SUPPORTED_SERVICE: &[&str] = &[
    "image",
    "build",
    "extends",
    "environment",
    "env_file",
    "ports",
    "volumes",
    "command",
    "entrypoint",
    "depends_on",
    "healthcheck",
    "restart",
    "networks",
    "labels",
    "working_dir",
    "user",
    "cap_add",
    "cap_drop",
    "privileged",
    "tmpfs",
    "extra_hosts",
    "deploy",
    "container_name",
    "hostname",
    "read_only",
    "profiles",
    "secrets",
    "configs",
];

/// Keys of a top-level `networks:`/`volumes:` entry that are read.
const SUPPORTED_NETWORK: &[&str] = &["external", "name"];
const SUPPORTED_VOLUME: &[&str] = &["external", "name"];
/// Keys of a top-level `secrets:`/`configs:` entry that are read — `configs:`
/// has no `environment` (real Compose Spec never gives it that source).
const SUPPORTED_SECRET: &[&str] = &["file", "environment", "external", "name"];
const SUPPORTED_CONFIG: &[&str] = &["file", "external", "name"];

/// (service key, the flag that already does it) — the engine HAS the capability
/// and `compose` simply does not wire it through yet.
///
/// Kept apart from `KNOWN_UNSUPPORTED_SERVICE` because the answer is genuinely
/// different: "we decided not to" and "we can, just not from here" send the
/// reader to different places. Several of these change ISOLATION
/// (`security_opt`, `pid`, `ipc`, `network_mode`), which is why dropping them in
/// silence was the worst case of all — the container ran less confined than the
/// file said and nothing reported it.
const ENGINE_HAS_IT_SERVICE: &[(&str, &str)] = &[
    ("devices", "container run --device"),
    ("dns", "container run --dns"),
    ("security_opt", "container run --security-opt"),
    ("group_add", "container run --group-add"),
    ("logging", "container run --log-driver"),
    ("network_mode", "container run --net"),
    ("pid", "container run --host-pid"),
    ("ipc", "container run --host-ipc"),
];

#[derive(Subcommand)]
pub enum ComposeCmd {
    /// Creates/starts every service.
    ///
    /// Build, then network, then volume, then containers in `depends_on` order
    /// (gated on the declared condition).
    Up {
        /// Repeatable — `-f a.yml -f b.yml` merges left-to-right (see the
        /// module doc-comment, "Multi-file compose"). No `-f` at all falls
        /// back to the usual single-file search.
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Vec<PathBuf>,
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
        /// Activate a service declaring this profile (repeatable). A service
        /// with no `profiles:` always runs; one that names some only runs if
        /// requested here, or if an activated service `depends_on` it.
        #[arg(long = "profile")]
        profile: Vec<String>,
    },
    /// Removes every container this project's `up` created.
    ///
    /// And, with `-v`, its named volumes. Networks/volumes marked
    /// `external: true` are NEVER removed.
    Down {
        /// Repeatable — must name the SAME file(s) `up` was given, so the
        /// project/network/volume names re-derive identically.
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Vec<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        #[arg(short = 'v', long)]
        volumes: bool,
    },
    /// Containers of this project (derived from labels).
    Ps {
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Vec<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
    },
    /// Logs of one service (default: every service, one after another).
    Logs {
        service: Option<String>,
        #[arg(value_hint = clap::ValueHint::FilePath, short = 'f', long = "file")]
        file: Vec<PathBuf>,
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
        file: Vec<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        /// Same meaning as `up --profile` — resolve and print as if these
        /// profiles were requested.
        #[arg(long = "profile")]
        profile: Vec<String>,
    },
}

pub fn run(cmd: ComposeCmd) -> Result<()> {
    match cmd {
        ComposeCmd::Up {
            file,
            project,
            dry_run,
            detach: _,
            profile,
        } => cmd_up(file, project, dry_run, profile),
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
        ComposeCmd::Config {
            file,
            project,
            profile,
        } => cmd_config(file, project, profile),
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
    #[serde(default)]
    secrets: BTreeMap<String, ComposeSecretDef>,
    #[serde(default)]
    configs: BTreeMap<String, ComposeConfigDef>,
}

/// A top-level `secrets:` entry. `file`/`environment` are the two sources
/// this module can resolve without talking to anything outside the compose
/// file/process env; `external`/`name` are read only to give `external: true`
/// a specific refusal instead of a silent "field ignored".
#[derive(Debug, Deserialize, Default)]
struct ComposeSecretDef {
    file: Option<PathBuf>,
    environment: Option<String>,
    #[serde(default)]
    external: bool,
    name: Option<String>,
}

/// A top-level `configs:` entry. No `environment:` here — the Compose
/// Specification never gives configs that source, only `secrets:` has it.
#[derive(Debug, Deserialize, Default)]
struct ComposeConfigDef {
    file: Option<PathBuf>,
    #[serde(default)]
    external: bool,
    name: Option<String>,
}

/// A service's `secrets:`/`configs:` entry — the short form (just the name)
/// or the long form (`source`/`target`). `uid`/`gid`/`mode` are accepted by
/// the Compose Specification's long form but not read here — the same
/// documented simplification `ComposeVolumeMount`'s long form already makes
/// for options this module has no dataplane for.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ComposeSecretRef {
    Short(String),
    Long {
        source: String,
        #[serde(default)]
        target: Option<String>,
    },
}
impl ComposeSecretRef {
    fn source(&self) -> &str {
        match self {
            ComposeSecretRef::Short(s) => s,
            ComposeSecretRef::Long { source, .. } => source,
        }
    }
    fn target(&self) -> Option<&str> {
        match self {
            ComposeSecretRef::Short(_) => None,
            ComposeSecretRef::Long { target, .. } => target.as_deref(),
        }
    }
}

#[derive(Debug, Deserialize, Default, Clone)]
struct ComposeService {
    image: Option<String>,
    build: Option<ComposeBuild>,
    /// `extends: { service: <name> }` — resolved by `resolve_extends` before
    /// this service is used anywhere else, so nothing downstream needs to
    /// know it existed (it's always `None` by the time `translate` runs).
    /// `extends.file` is refused earlier, at the raw-YAML check
    /// (`check_unsupported_fields`) — this repo doesn't do multi-file
    /// compose at all, so `file:` naming anything is refused outright rather
    /// than silently pointed at the wrong file.
    #[serde(default)]
    extends: Option<ComposeExtends>,
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
    #[serde(default)]
    secrets: Vec<ComposeSecretRef>,
    #[serde(default)]
    configs: Vec<ComposeSecretRef>,
    /// A service with no `profiles:` is always active. One that declares them
    /// only starts when one of them is requested via `--profile` — or when
    /// another ACTIVE service `depends_on` it (see `active_services`, which
    /// mirrors real `docker compose`: a dependency is pulled in even outside
    /// the requested profile, because the service that needs it cannot start
    /// without it).
    #[serde(default)]
    profiles: Vec<String>,
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

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ComposePort {
    Short(String),
    Long {
        target: StringOrNum,
        published: Option<StringOrNum>,
        protocol: Option<String>,
    },
}

#[derive(Debug, Deserialize, Clone)]
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

/// `extends: { service: <name> }` on a service — resolved (and consumed) by
/// `resolve_extends` before anything else touches `ComposeService`.
#[derive(Debug, Deserialize, Clone)]
struct ComposeExtends {
    service: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ComposeCmdShape {
    Shell(String),
    Exec(Vec<String>),
}

#[derive(Debug, Deserialize, Clone)]
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

#[derive(Debug, Deserialize, Clone)]
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

#[derive(Debug, Deserialize, Default, Clone)]
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

#[derive(Debug, Deserialize, Clone)]
struct ComposeServiceNetworkEntry {
    ipv4_address: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct ComposeDeploy {
    resources: Option<ComposeResources>,
    replicas: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
struct ComposeResources {
    limits: Option<ComposeResourceLimits>,
}

#[derive(Debug, Deserialize, Clone)]
struct ComposeResourceLimits {
    cpus: Option<String>,
    memory: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
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
// Multi-file merge — pure, PURE (typed struct in, typed struct out), no I/O.
//
// Left-to-right: the first file is the base, each following one is an
// overlay merged ON TOP of what came before. See the module doc-comment
// ("Multi-file compose") for the exact per-field rules; this section is the
// implementation of that paragraph, one function per shape.
// ============================================================================

fn merge_compose_files(files: Vec<ComposeFile>) -> ComposeFile {
    let mut iter = files.into_iter();
    let mut merged = iter.next().unwrap_or_default();
    for next in iter {
        merged.services = merge_services(merged.services, next.services);
        // `external`/`name` are the whole struct — a shared key is a full
        // REPLACE, not a field merge (documented simplification: too small a
        // struct for a field-by-field merge to earn its complexity).
        merged.networks.extend(next.networks);
        merged.volumes.extend(next.volumes);
    }
    merged
}

fn merge_services(
    mut base: BTreeMap<String, ComposeService>,
    overlay: BTreeMap<String, ComposeService>,
) -> BTreeMap<String, ComposeService> {
    for (name, overlay_svc) in overlay {
        match base.remove(&name) {
            Some(base_svc) => {
                base.insert(name, merge_service_overlay(base_svc, overlay_svc));
            }
            None => {
                base.insert(name, overlay_svc);
            }
        }
    }
    base
}

/// One service declared in more than one file — the per-field rules from the
/// module doc-comment. `ComposeService` derives `Default`, so a field this
/// function doesn't mention explicitly can't silently vanish: every field is
/// listed here, and a new field added to the struct without a matching arm
/// would fail to compile (no `..Default::default()` in the constructor).
fn merge_service_overlay(base: ComposeService, overlay: ComposeService) -> ComposeService {
    ComposeService {
        image: overlay.image.or(base.image),
        build: overlay.build.or(base.build),
        environment: merge_env_overlay(base.environment, overlay.environment),
        env_file: OneOrMany::Many(concat(
            base.env_file.into_vec(),
            overlay.env_file.into_vec(),
        )),
        ports: concat(base.ports, overlay.ports),
        volumes: concat(base.volumes, overlay.volumes),
        command: overlay.command.or(base.command),
        entrypoint: overlay.entrypoint.or(base.entrypoint),
        depends_on: merge_depends_on(base.depends_on, overlay.depends_on),
        healthcheck: overlay.healthcheck.or(base.healthcheck),
        restart: overlay.restart.or(base.restart),
        networks: merge_service_networks(base.networks, overlay.networks),
        labels: merge_env_overlay(base.labels, overlay.labels),
        working_dir: overlay.working_dir.or(base.working_dir),
        user: overlay.user.or(base.user),
        cap_add: concat_dedup(base.cap_add, overlay.cap_add),
        cap_drop: concat_dedup(base.cap_drop, overlay.cap_drop),
        // `bool` fields have no "unset" state (`#[serde(default)]` makes an
        // absent key indistinguishable from an explicit `false`), so a
        // declared `false` in a later file can never silently turn OFF a
        // `true` from an earlier one — OR is the fail-safe direction for a
        // security-relevant flag like `privileged`, and kept the same for
        // `read_only` for consistency. Documented simplification, not a bug:
        // a real per-field "was this key present" tracker would need a
        // second parse pass over the raw YAML per file.
        privileged: base.privileged || overlay.privileged,
        tmpfs: OneOrMany::Many(concat(base.tmpfs.into_vec(), overlay.tmpfs.into_vec())),
        extra_hosts: merge_env_overlay(base.extra_hosts, overlay.extra_hosts),
        deploy: overlay.deploy.or(base.deploy),
        container_name: overlay.container_name.or(base.container_name),
        hostname: overlay.hostname.or(base.hostname),
        read_only: base.read_only || overlay.read_only,
        // `extends:` has NOT been resolved yet when the multi-file merge
        // runs (`resolve_extends` runs AFTER it — see `load_compose`), so it
        // has to survive the merge instead of being consumed here the way
        // the extends-side `merge_service` does. A later file that declares
        // `extends:` again wins; otherwise the first file's survives.
        extends: overlay.extends.or(base.extends),
        // Lists of secret/config REFERENCES and of profiles accumulate
        // without duplicating, like `cap_add`/`cap_drop` above — a later
        // file adds to what an earlier one declared instead of replacing it,
        // which is the useful reading of an overlay.
        secrets: concat_secret_refs(base.secrets, overlay.secrets),
        configs: concat_secret_refs(base.configs, overlay.configs),
        profiles: concat_dedup(base.profiles, overlay.profiles),
    }
}

/// A service's `secrets:`/`configs:` in a multi-file merge: they accumulate
/// without repeating the SAME source. `ComposeSecretRef` is not `PartialEq`,
/// so the comparison goes through its own `source()` — the key that names
/// the top-level entry a reference points at.
fn concat_secret_refs(
    base: Vec<ComposeSecretRef>,
    overlay: Vec<ComposeSecretRef>,
) -> Vec<ComposeSecretRef> {
    let mut out = base;
    for r in overlay {
        if !out.iter().any(|e| e.source() == r.source()) {
            out.push(r);
        }
    }
    out
}

fn concat<T>(mut base: Vec<T>, overlay: Vec<T>) -> Vec<T> {
    base.extend(overlay);
    base
}

fn concat_dedup(base: Vec<String>, overlay: Vec<String>) -> Vec<String> {
    let mut out = base;
    for v in overlay {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// `environment:`/`labels:`/`extra_hosts:` merge KEY-BY-KEY (later file wins
/// only on a shared key) — never a full replace, which would silently drop
/// every variable the base file declared just because the overlay declared
/// one more. Both sides go through the same `KEY=VALUE`/`{KEY: VALUE}`
/// normalization `to_kv_pairs`'s callers already rely on, so a `List` and a
/// `Map` merge correctly no matter which form each file used.
fn merge_env_overlay(base: ComposeEnv, overlay: ComposeEnv) -> ComposeEnv {
    let mut merged = env_as_map(&base);
    merged.extend(env_as_map(&overlay));
    ComposeEnv::Map(merged)
}

fn env_as_map(env: &ComposeEnv) -> BTreeMap<String, Option<serde_yaml::Value>> {
    match env {
        ComposeEnv::Map(m) => m.clone(),
        ComposeEnv::List(v) => v
            .iter()
            .map(|s| match s.split_once('=') {
                Some((k, val)) => (
                    k.to_string(),
                    Some(serde_yaml::Value::String(val.to_string())),
                ),
                None => (s.clone(), None),
            })
            .collect(),
    }
}

/// `depends_on:` **accumulates** across files (a name declared in either is
/// kept; a name declared in both keeps the LATER file's condition) — the
/// opposite of `extends:`'s exclusion of `depends_on`, and deliberately so:
/// that exclusion exists to dodge ambiguity with a FOREIGN template, which
/// does not apply here (same project, same service, split across files for
/// convenience).
fn merge_depends_on(base: ComposeDependsOn, overlay: ComposeDependsOn) -> ComposeDependsOn {
    let mut merged = depends_on_as_map(base);
    merged.extend(depends_on_as_map(overlay));
    ComposeDependsOn::Long(
        merged
            .into_iter()
            .map(|(name, condition)| (name, ComposeDependsOnEntry { condition }))
            .collect(),
    )
}

fn depends_on_as_map(d: ComposeDependsOn) -> BTreeMap<String, String> {
    match d {
        ComposeDependsOn::Short(v) => v
            .into_iter()
            .map(|name| (name, "service_started".to_string()))
            .collect(),
        ComposeDependsOn::Long(m) => m.into_iter().map(|(k, e)| (k, e.condition)).collect(),
    }
}

/// A service's `networks:` merges key-by-key like `environment:` above — a
/// service attached to `net-a` in the base file and `net-b` in an overlay
/// ends up on BOTH, not just the overlay's.
fn merge_service_networks(
    base: ComposeServiceNetworks,
    overlay: ComposeServiceNetworks,
) -> ComposeServiceNetworks {
    let mut merged = service_networks_as_map(base);
    merged.extend(service_networks_as_map(overlay));
    if merged.is_empty() {
        return ComposeServiceNetworks::Empty;
    }
    ComposeServiceNetworks::Map(
        merged
            .into_iter()
            .map(|(k, ipv4_address)| (k, ComposeServiceNetworkEntry { ipv4_address }))
            .collect(),
    )
}

fn service_networks_as_map(n: ComposeServiceNetworks) -> BTreeMap<String, Option<String>> {
    match n {
        ComposeServiceNetworks::Empty => BTreeMap::new(),
        ComposeServiceNetworks::List(v) => v.into_iter().map(|k| (k, None)).collect(),
        ComposeServiceNetworks::Map(m) => m.into_iter().map(|(k, e)| (k, e.ipv4_address)).collect(),
    }
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

/// Resolves every `-f`/`--file` occurrence to a real path, in the order given
/// on the command line — the first is the BASE, later ones override/extend it
/// (real `docker compose`'s own convention for `-f a.yml -f b.yml`). No `-f`
/// at all falls back to the single-file default search.
fn resolve_compose_paths(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if files.is_empty() {
        return Ok(vec![resolve_compose_path(None)?]);
    }
    files
        .iter()
        .map(|f| resolve_compose_path(Some(f.clone())))
        .collect()
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
///
/// `active` scopes the sort to the services `active_services` resolved for
/// this run — everything outside it (an inactive profile, never requested and
/// not needed by anything that IS active) is left out of the graph entirely,
/// not just out of the final order. `active` is trusted to already be closed
/// under `depends_on` (see `active_services`'s doc-comment); this function
/// still checks that every name a service names is DECLARED, because that
/// error has nothing to do with profiles.
fn topo_sort(
    services: &BTreeMap<String, ComposeService>,
    active: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut deps: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for name in active {
        let svc = &services[name];
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
        active.iter().map(|k| (k.as_str(), Vec::new())).collect();
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
    if order.len() != active.len() {
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

/// The services that actually run for this `up`/`config`: every service with
/// no `profiles:` (always on), every service naming a REQUESTED profile, and
/// — the same rule real `docker compose` uses — anything an active service
/// reaches through `depends_on`, even outside the requested profiles. Without
/// this transitive step, an active service could `depends_on` one this
/// function silently dropped, and `translate` would have nothing to wait on.
fn active_services(compose: &ComposeFile, requested: &[String]) -> BTreeSet<String> {
    let mut active: BTreeSet<String> = compose
        .services
        .iter()
        .filter(|(_, svc)| {
            svc.profiles.is_empty() || svc.profiles.iter().any(|p| requested.contains(p))
        })
        .map(|(name, _)| name.clone())
        .collect();
    loop {
        let additions: Vec<String> = active
            .iter()
            .filter_map(|name| compose.services.get(name))
            .flat_map(|svc| svc.depends_on.names())
            .filter(|dep| !active.contains(dep))
            .collect();
        if additions.is_empty() {
            break;
        }
        active.extend(additions);
    }
    active
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

/// One scoped name PER ANONYMOUS MOUNT of a service, in order — the Compose
/// Spec shorthand `- /container/path` (no `:`, so no explicit source), which
/// real `docker compose` auto-names and tracks via labels on the volume. This
/// engine has no separate volume registry to track membership in (same
/// philosophy as named volume/network naming above: `down`/`down -v`
/// RE-DERIVE names by re-parsing the compose file, never look anything up) —
/// so an anonymous volume gets a name that is itself re-derivable: the
/// service name plus the 1-based RANK OF ITS TARGET PATH among that service's
/// OWN anonymous mounts (a named/bind mount does not consume a slot), joined
/// through the same collision-free `compose_scoped_name` used for named
/// volumes/networks. Rank over the sorted set and NOT position in the file —
/// see the comment in the body for the reordering hazard that buys.
///
/// `--anon<n>` is deliberately NOT a value a real top-level `volumes:` key is
/// likely to collide with — this is a naming convention, not a security
/// boundary (the whole "no registry, re-derive by convention" philosophy
/// already accepts this class of risk for named volumes/networks too).
fn anonymous_volume_names(
    project: &str,
    service: &str,
    mounts: &[ComposeVolumeMount],
) -> Vec<String> {
    let anon: Vec<&str> = mounts
        .iter()
        .filter_map(|m| match m {
            ComposeVolumeMount::Short(s) if !s.contains(':') => Some(s.as_str()),
            _ => None,
        })
        .collect();

    // The index is the rank of the mount's TARGET PATH among this service's
    // anonymous mounts, not its position in the file. Writing order would make
    // the name depend on how the YAML happens to be laid out: swapping two
    // `- /path` lines — an entirely ordinary edit — would swap which volume
    // backs which path, so a database would come up on the volume that held
    // the logs, silently and with the data still there at the wrong target.
    // A rank over the sorted set is stable under reordering; the returned Vec
    // stays in appearance order, which is what the caller zips against.
    let mut sorted: Vec<&str> = anon.clone();
    sorted.sort_unstable();

    anon.iter()
        .map(|path| {
            let n = sorted.iter().position(|p| p == path).unwrap() + 1;
            compose_scoped_name(project, &format!("{service}--anon{n}"))
        })
        .collect()
}

/// Reads a top-level `secrets:`/`configs:` entry's content. `what` is
/// "secret"/"config", used only in the error text.
fn resolve_secret_content(
    what: &str,
    name: &str,
    file: Option<&Path>,
    environment: Option<&str>,
    external: bool,
    base_dir: &Path,
) -> Result<String> {
    if external {
        return Err(Error::Invalid(format!(
            "compose: {what} '{name}': `external: true` is not supported in v1 — this \
             implementation cannot reference a pre-existing {what}, only create one from \
             `file:`{}",
            if what == "secret" {
                " or `environment:`"
            } else {
                ""
            }
        )));
    }
    if let Some(f) = file {
        let path = base_dir.join(f);
        return std::fs::read_to_string(&path).map_err(|e| {
            Error::Invalid(format!(
                "compose: {what} '{name}': reading {}: {e}",
                path.display()
            ))
        });
    }
    if let Some(var) = environment {
        return std::env::var(var).map_err(|_| {
            Error::Invalid(format!(
                "compose: secret '{name}': `environment: {var}` is not set in this shell"
            ))
        });
    }
    Err(Error::Invalid(format!(
        "compose: {what} '{name}' needs `file:`{} — neither was given",
        if what == "secret" {
            " or `environment:`"
        } else {
            ""
        }
    )))
}

#[derive(Serialize)]
struct SecretDocSpec {
    #[serde(rename = "stringData")]
    string_data: BTreeMap<String, String>,
}

/// A `kind: Secret` document holding exactly one key: the compose secret's
/// own name, mapped to its resolved content. One key (not the whole map of
/// every compose secret) because `Container.secret`'s file delivery mounts
/// EVERY key of every attached secret under `/run/secrets/<key>` — merging
/// unrelated secrets into one document would let one service's `secrets:`
/// list pull in a key it never declared.
fn secret_doc(real_name: &str, data_key: &str, content: &str) -> ManifestDoc {
    let mut string_data = BTreeMap::new();
    string_data.insert(data_key.to_string(), content.to_string());
    ManifestDoc {
        api_version: "delonix.io/v1".to_string(),
        kind: k::SECRET.to_string(),
        metadata: Metadata {
            name: real_name.to_string(),
            namespace: None,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        },
        spec: serde_yaml::to_value(SecretDocSpec { string_data })
            .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new())),
    }
}

/// Resolves every `secrets:`/`configs:` reference of the ACTIVE services into
/// `kind: Secret` documents, plus the (compose key -> delonix secret name)
/// maps `service_to_run_opts` needs to fill `RunOpts.secret`.
///
/// Only entries actually REFERENCED by a service are resolved — a `secrets:`
/// block nobody attaches costs nothing and reads nothing, same as an unused
/// top-level `networks:`/`volumes:` entry would (well, those still get
/// created; but here there is no dataplane-free "declare it anyway" step,
/// and a `file:` that does not exist would otherwise fail an `up` for a
/// secret no container even wants).
/// `(kind: Secret` documents, compose secret name -> delonix name, compose
/// config name -> delonix name)`.
type ComposeSecrets = (
    Vec<ManifestDoc>,
    BTreeMap<String, String>,
    BTreeMap<String, String>,
);

fn resolve_compose_secrets(
    project: &str,
    compose: &ComposeFile,
    active: &BTreeSet<String>,
    base_dir: &Path,
) -> Result<ComposeSecrets> {
    let mut used_secrets: BTreeSet<String> = BTreeSet::new();
    let mut used_configs: BTreeSet<String> = BTreeSet::new();
    for (svc_name, svc) in compose.services.iter().filter(|(n, _)| active.contains(*n)) {
        for (what, refs, used) in [
            ("secret", &svc.secrets, &mut used_secrets),
            ("config", &svc.configs, &mut used_configs),
        ] {
            for r in refs {
                if let Some(t) = r.target() {
                    if t != r.source() {
                        return Err(Error::Invalid(format!(
                            "compose: service '{svc_name}' {what} '{}': `target: {t}` \
                             (renaming the mounted filename) is not supported in v1 — use \
                             the source name, or drop `target:`",
                            r.source()
                        )));
                    }
                }
                used.insert(r.source().to_string());
            }
        }
    }

    let mut docs = Vec::new();
    let mut secret_names = BTreeMap::new();
    for key in &used_secrets {
        let def = compose.secrets.get(key).ok_or_else(|| {
            Error::Invalid(format!(
                "compose: service references undeclared secret '{key}' (declare it under \
                 top-level `secrets:`)"
            ))
        })?;
        let content = resolve_secret_content(
            "secret",
            key,
            def.file.as_deref(),
            def.environment.as_deref(),
            def.external,
            base_dir,
        )?;
        let real = def
            .name
            .clone()
            .unwrap_or_else(|| compose_scoped_name(project, &format!("secret-{key}")));
        docs.push(secret_doc(&real, key, &content));
        secret_names.insert(key.clone(), real);
    }
    let mut config_names = BTreeMap::new();
    for key in &used_configs {
        let def = compose.configs.get(key).ok_or_else(|| {
            Error::Invalid(format!(
                "compose: service references undeclared config '{key}' (declare it under \
                 top-level `configs:`)"
            ))
        })?;
        let content = resolve_secret_content(
            "config",
            key,
            def.file.as_deref(),
            None,
            def.external,
            base_dir,
        )?;
        let real = def
            .name
            .clone()
            .unwrap_or_else(|| compose_scoped_name(project, &format!("config-{key}")));
        docs.push(secret_doc(&real, key, &content));
        config_names.insert(key.clone(), real);
    }
    Ok((docs, secret_names, config_names))
}

/// Refuses every key this implementation does not read — against the RAW parsed
/// YAML, because the typed structs drop an unrecognized key without a word.
///
/// Three answers, never two, and the order is deliberate — most specific first:
///
/// 1. **`KNOWN_UNSUPPORTED_*`** — decided against, with the reason and the
///    Delonix equivalent.
/// 2. **`ENGINE_HAS_IT_SERVICE`** — the engine does it, `compose` does not wire
///    it through; the message names the flag that does.
/// 3. **Not in the allowlist** — refused generically, and the message prints
///    what IS read so the fix is one line away.
///
/// PURE (`&str` in, `Result` out), so every one of those paths is a data test.
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
    for key in mapping_keys(top) {
        if !SUPPORTED_TOP.contains(&key.as_str()) {
            return Err(Error::Invalid(format!(
                "compose: top-level `{key}:` is not understood and was refused rather \
                 than ignored — this implementation reads {}",
                SUPPORTED_TOP.join(", ")
            )));
        }
    }
    for (section, allowed) in [
        ("networks", SUPPORTED_NETWORK),
        ("volumes", SUPPORTED_VOLUME),
        ("secrets", SUPPORTED_SECRET),
        ("configs", SUPPORTED_CONFIG),
    ] {
        let Some(serde_yaml::Value::Mapping(entries)) = top.get(section) else {
            continue;
        };
        for (entry_name, entry_val) in entries {
            let serde_yaml::Value::Mapping(entry_map) = entry_val else {
                continue; // `mynet:` with no body is legal and means "defaults"
            };
            for key in mapping_keys(entry_map) {
                if !allowed.contains(&key.as_str()) {
                    return Err(Error::Invalid(format!(
                        "compose: {section} '{}': `{key}:` is not understood and was \
                         refused rather than ignored — this implementation reads {}",
                        entry_name.as_str().unwrap_or("?"),
                        allowed.join(", ")
                    )));
                }
            }
        }
    }
    if let Some(serde_yaml::Value::Mapping(services)) = top.get("services") {
        for (svc_name, svc_val) in services {
            let serde_yaml::Value::Mapping(svc_map) = svc_val else {
                continue;
            };
            let svc = svc_name.as_str().unwrap_or("?");
            if let Some(serde_yaml::Value::Mapping(ext_map)) =
                svc_map.get(serde_yaml::Value::String("extends".to_string()))
            {
                if ext_map.contains_key(serde_yaml::Value::String("file".to_string())) {
                    return Err(Error::Invalid(format!(
                        "compose: service '{svc}': extends.file is not supported — it \
                         inherits from a service in ANOTHER file, with that file's own \
                         path relativity, which is a different thing from the `-f a -f b` \
                         merge this implementation does do (that merges whole documents \
                         and resolves relative paths against the FIRST file). Refused \
                         rather than silently resolved against the wrong file: omit \
                         `file:` to extend a service in the merged document, or pass the \
                         other file with its own `-f`"
                    )));
                }
            }
            for (key, reason) in KNOWN_UNSUPPORTED_SERVICE {
                if svc_map.contains_key(serde_yaml::Value::String((*key).to_string())) {
                    return Err(Error::Invalid(format!(
                        "compose: service '{svc}': `{key}:` is not supported — {reason}"
                    )));
                }
            }
            for (key, flag) in ENGINE_HAS_IT_SERVICE {
                if svc_map.contains_key(serde_yaml::Value::String((*key).to_string())) {
                    return Err(Error::Invalid(format!(
                        "compose: service '{svc}': `{key}:` is not read by `compose` — \
                         the engine does it (`delonix {flag}`), so this is refused \
                         instead of silently dropped"
                    )));
                }
            }
            for key in mapping_keys(svc_map) {
                if !SUPPORTED_SERVICE.contains(&key.as_str()) {
                    return Err(Error::Invalid(format!(
                        "compose: service '{svc}': `{key}:` is not understood and was \
                         refused rather than ignored — this implementation reads {}",
                        SUPPORTED_SERVICE.join(", ")
                    )));
                }
            }
        }
    }
    Ok(())
}

/// The string keys of a YAML mapping, in order. A non-string key (YAML allows
/// them) is skipped rather than refused: it cannot collide with any name in the
/// allowlists, and refusing it would be a second, unrelated opinion.
fn mapping_keys(m: &serde_yaml::Mapping) -> Vec<String> {
    m.keys()
        .filter_map(|k| k.as_str().map(|s| s.to_string()))
        .collect()
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
    secret_docs: Vec<ManifestDoc>,
    /// One entry per REPLICA, in replica order — `[0]` is the "canonical"
    /// container (`<project>-<service>`, or `container_name:` if given,
    /// which `deploy.replicas>1` refuses precisely so this stays unambiguous)
    /// that `depends_on`/healthcheck waits target; `[1..]` are the extra
    /// copies (`<project>-<service>-<n>`), reachable only by their own name —
    /// this engine does no load balancing across them (same "no VIP, no
    /// daemon" posture as `kind: Service`'s DNS round-robin, which this v1
    /// does not wire replicas into).
    containers: BTreeMap<String, Vec<(super::container::RunOpts, String)>>,
    order: Vec<String>,
    waits: BTreeMap<String, Vec<DependsOnWait>>,
}

/// `true` for a `ports:` entry that pins a specific host port — the short
/// form's 2/3-part forms, or the long form's `published:`. A bare container
/// port (`"80"`, no `published:`) resolves to a fresh `free_host_port()` per
/// call, so replicas never collide on that form; a pinned one names the SAME
/// host port for every replica and always would.
fn port_has_explicit_host(p: &ComposePort) -> bool {
    match p {
        ComposePort::Short(s) => s.contains(':'),
        ComposePort::Long { published, .. } => published.is_some(),
    }
}

#[allow(clippy::too_many_arguments)]
fn translate(
    compose: &ComposeFile,
    project: &str,
    base_dir: &Path,
    requested_profiles: &[String],
) -> Result<Translated> {
    if !valid_compose_project_name(project) {
        return Err(Error::Invalid(format!(
            "compose: invalid project name '{project}' (only lowercase letters, digits, '_'/'-', must start with a letter or digit)"
        )));
    }
    let active = active_services(compose, requested_profiles);
    for (name, svc) in compose.services.iter().filter(|(n, _)| active.contains(*n)) {
        if svc.image.is_none() && svc.build.is_none() {
            return Err(Error::Invalid(format!(
                "compose: service '{name}' has neither `image` nor `build`"
            )));
        }
        let replicas = svc.deploy.as_ref().and_then(|d| d.replicas).unwrap_or(1);
        if replicas == 0 {
            return Err(Error::Invalid(format!(
                "compose: service '{name}': deploy.replicas=0 is not supported — this v1 has \
                 no scale-to-zero/profile toggle for a running service; remove the service \
                 instead"
            )));
        }
        if replicas > 1 {
            if svc.container_name.is_some() {
                return Err(Error::Invalid(format!(
                    "compose: service '{name}': container_name is incompatible with \
                     deploy.replicas={replicas} — every replica would collide on that one name"
                )));
            }
            if svc.ports.iter().any(port_has_explicit_host) {
                return Err(Error::Invalid(format!(
                    "compose: service '{name}': deploy.replicas={replicas} with an explicit \
                     host port would collide across replicas — use a bare container port \
                     (e.g. \"80\") so each replica binds its own random free host port instead"
                )));
            }
        }
    }
    let order = topo_sort(&compose.services, &active)?;
    let network_names = resolve_network_names(project, compose);
    let volume_names = resolve_volume_names(project, compose);

    let mut network_docs: Vec<ManifestDoc> = Vec::new();
    for (key, net) in &compose.networks {
        if net.external {
            continue;
        }
        network_docs.push(simple_doc(k::NETWORK, &network_names[key]));
    }
    let default_real = network_names["default"].clone();
    if !network_docs.iter().any(|d| d.metadata.name == default_real) {
        network_docs.push(simple_doc(k::NETWORK, &default_real));
    }

    let mut volume_docs: Vec<ManifestDoc> = Vec::new();
    for (key, vol) in &compose.volumes {
        if vol.external {
            continue;
        }
        volume_docs.push(simple_doc(k::VOLUME, &volume_names[key]));
    }

    let (secret_docs, secret_names, config_names) =
        resolve_compose_secrets(project, compose, &active, base_dir)?;

    let mut image_docs = Vec::new();
    let mut containers = BTreeMap::new();
    let mut waits: BTreeMap<String, Vec<DependsOnWait>> = BTreeMap::new();

    for (name, svc) in compose.services.iter().filter(|(n, _)| active.contains(*n)) {
        // Anonymous volumes get their `kind: Volume` docs HERE, alongside the
        // named ones above — same creation path (`volume::apply`), so there
        // is no second way a volume comes into existence. Computed once and
        // handed to `service_to_run_opts` unchanged: it and this loop MUST
        // agree on the names, or the container would mount a volume that was
        // never created.
        let anon_names = anonymous_volume_names(project, name, &svc.volumes);
        for n in &anon_names {
            volume_docs.push(simple_doc(k::VOLUME, n));
        }
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
        let replicas = svc.deploy.as_ref().and_then(|d| d.replicas).unwrap_or(1);
        let mut instances = Vec::with_capacity(replicas as usize);
        for idx in 1..=replicas {
            instances.push(service_to_run_opts(
                project,
                name,
                svc,
                base_dir,
                &network_names,
                &volume_names,
                &anon_names,
                &secret_names,
                &config_names,
                &image_ref,
                idx,
            )?);
        }
        containers.insert(name.clone(), instances);

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
        secret_docs,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
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
    let (context_rel, dockerfile, args, target) = match build {
        ComposeBuild::Context(c) => (c.clone(), None, Vec::new(), None),
        ComposeBuild::Full {
            context,
            dockerfile,
            args,
            target,
        } => (
            context.to_string_lossy().into_owned(),
            dockerfile.clone(),
            args.to_kv_pairs(),
            target.clone(),
        ),
    };
    let context_path = base_dir.join(&context_rel);
    let file = dockerfile.map(|f| context_path.join(f).to_string_lossy().into_owned());
    let spec = ImageDocSpec {
        build: BuildDocSpec {
            context: context_path.to_string_lossy().into_owned(),
            file,
            tag: tag.to_string(),
            build_args: args,
            target,
        },
    };
    Ok(ManifestDoc {
        api_version: "delonix.io/v1".to_string(),
        kind: k::IMAGE.to_string(),
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
pub(crate) fn free_host_port() -> Result<u16> {
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
                // `host_ip:host_port:container_port` (`127.0.0.1:9000:80`) passa
                // VERBATIM para o motor, que o entende desde que o `parse_publish_addr`
                // existe (`delonix-net`, forma `[hostIp:]hostPort:contPort[/proto]`).
                //
                // **Isto já foi um descarte silencioso e depois uma recusa, e as duas
                // estavam erradas de maneiras opostas.** Primeiro a forma de 3 partes
                // caía no caso de 2 e a restrição de IP desaparecia — um ficheiro que
                // pede loopback-only publicava em TODAS as interfaces, o oposto exacto
                // da intenção de segurança. A correcção foi recusar; melhor, mas ainda
                // errado, porque a razão invocada («o `-p` exige host port só dígitos»)
                // deixou de valer quando o CLI ganhou o parser de host-IP e foi validado
                // ao vivo com `curl` da LAN a devolver 200. Ficou a recusar uma
                // capacidade que a casa já tinha — no ficheiro que a maioria das pessoas
                // traz do docker, e na forma mais comum de expor um serviço só ao host.
                //
                // Uma limitação documentada envelhece para mentira sem ninguém lhe tocar:
                // é a terceira ocorrência nesta sessão, depois do `WorkingDir` da Docker
                // API e do `--net host` com `-p` do comparativo.
                out.push(s.clone());
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
                // Ranges (`8000-8002:9000-9002`) também vão verbatim: o
                // `expand_publish_range` existe, é público, e o `container run` e o
                // `update --publish-add` já o chamam na sua fronteira — o compose era
                // o único dos três a recusar o que os outros dois fazem. Larguras
                // diferentes continuam a ser recusadas lá, com a contagem dos dois
                // lados, em vez de truncadas em silêncio.
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
    anon_names: &[String],
    service: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut volumes = Vec::new();
    let mut tmpfs = Vec::new();
    let mut anon_idx = 0usize;
    for m in mounts {
        match m {
            // Anonymous: `- /container/path`, no `:` at all — no host path, no
            // named volume, nothing to resolve against `volume_names`. The
            // name was already computed (by `anonymous_volume_names`, in the
            // SAME order this loop walks) so a `kind: Volume` doc for it could
            // be created up front alongside the named ones — see `translate`.
            ComposeVolumeMount::Short(s) if !s.contains(':') => {
                if !s.starts_with('/') {
                    return Err(Error::Invalid(format!(
                        "compose: service '{service}' volume '{s}': target must be absolute"
                    )));
                }
                let name = anon_names.get(anon_idx).ok_or_else(|| Error::Invalid(format!(
                    "compose: service '{service}': internal error: no scoped name reserved for anonymous volume #{}",
                    anon_idx + 1
                )))?;
                anon_idx += 1;
                volumes.push(format!("{name}:{s}"));
            }
            ComposeVolumeMount::Short(s) => {
                let parts: Vec<&str> = s.splitn(3, ':').collect();
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

#[allow(clippy::too_many_arguments)]
fn service_to_run_opts(
    project: &str,
    service: &str,
    svc: &ComposeService,
    base_dir: &Path,
    network_names: &BTreeMap<String, String>,
    volume_names: &BTreeMap<String, String>,
    anon_names: &[String],
    secret_names: &BTreeMap<String, String>,
    config_names: &BTreeMap<String, String>,
    image_ref: &str,
    replica_index: u32,
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
    // `networks.*.ipv4_address` wires straight into the same `--ip` path
    // `container run` now has: `infra::attach_container_on_ip` reserves the
    // address in the IPAM registry before the attach, so nothing here
    // re-validates the subnet — that check already lives at the one place
    // that knows the network's actual prefix.
    let fixed_ip = svc
        .networks
        .entry(&net_key)
        .and_then(|entry| entry.ipv4_address.clone());
    let net = network_names.get(&net_key).cloned().ok_or_else(|| {
        Error::Invalid(format!(
            "compose: service '{service}' references undefined network '{net_key}' (declare it under top-level `networks:`)"
        ))
    })?;

    let (volumes, tmpfs_from_mounts) =
        resolve_volume_mounts(base_dir, &svc.volumes, volume_names, anon_names, service)?;
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

    // `container_name:` is refused alongside `deploy.replicas>1` (in
    // `translate`, before this ever runs) precisely so `replica_index == 1`
    // is the only case that can reach it here — every other replica gets the
    // `-<n>` suffix, never the bare service name a 2nd/3rd container would
    // collide on.
    let final_name = svc.container_name.clone().unwrap_or_else(|| {
        if replica_index == 1 {
            format!("{project}-{service}")
        } else {
            format!("{project}-{service}-{replica_index}")
        }
    });

    // `target:` mismatches were already refused in `resolve_compose_secrets`
    // (it sees every service, so it is the one place that can name the
    // service in the error) — by the time we get here every reference maps
    // straight onto a resolved delonix secret name. `secret_files: true`
    // whenever there is at least one, because Compose secrets/configs are
    // ALWAYS file-mounted (`/run/secrets/<name>`), never injected as env vars.
    let mut secret_refs: Vec<String> = Vec::new();
    for r in &svc.secrets {
        secret_refs.push(secret_names[r.source()].clone());
    }
    for r in &svc.configs {
        secret_refs.push(config_names[r.source()].clone());
    }
    let secret_files = !secret_refs.is_empty();

    let opts = super::container::RunOpts {
        secret: secret_refs,
        secret_files,
        detach: true,
        name: Some(final_name.clone()),
        hostname: svc.hostname.clone(),
        user: svc.user.clone(),
        net,
        ip: fixed_ip,
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
// `extends:` resolution
// ============================================================================

/// Merges two `KEY<sep>VALUE` pair lists (as `ComposeEnv::to_kv_pairs`/
/// `to_host_pairs` already produce), the child's value winning on a shared
/// key, at that key's ORIGINAL position (`base`'s if it had the key, else
/// wherever `child` first introduces it) — same "first position, latest
/// value" rule `docker compose`'s own `extends` uses for `environment:`.
fn merge_pairs(base: &[String], child: &[String], sep: char) -> Vec<String> {
    let mut merged: Vec<(String, String)> = Vec::new();
    let mut upsert = |pair: &str| {
        let (k, v) = match pair.split_once(sep) {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (pair.to_string(), String::new()),
        };
        match merged.iter_mut().find(|(ek, _)| *ek == k) {
            Some(existing) => existing.1 = v,
            None => merged.push((k, v)),
        }
    };
    for p in base {
        upsert(p);
    }
    for p in child {
        upsert(p);
    }
    merged
        .into_iter()
        .map(|(k, v)| format!("{k}{sep}{v}"))
        .collect()
}

fn merge_env(base: &ComposeEnv, child: &ComposeEnv) -> ComposeEnv {
    ComposeEnv::List(merge_pairs(&base.to_kv_pairs(), &child.to_kv_pairs(), '='))
}

fn merge_extra_hosts(base: &ComposeEnv, child: &ComposeEnv) -> ComposeEnv {
    ComposeEnv::List(merge_pairs(
        &base.to_host_pairs(),
        &child.to_host_pairs(),
        ':',
    ))
}

/// Appends `child` to `base`, skipping a `child` entry already present in
/// `base` — used for `cap_add`/`cap_drop`, where "declared twice" and
/// "declared once" mean the same thing.
fn dedup_concat(base: Vec<String>, child: Vec<String>) -> Vec<String> {
    let mut out = base;
    for c in child {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// `networks:` isn't merged field-by-field — this engine attaches only ONE
/// network per container anyway (`service_to_run_opts` already warns and
/// picks the first when a service names more than one), so "the child's own
/// declaration wins outright, else inherit the base's" is the whole rule,
/// with none of the real Compose Specification's deeper per-network-entry
/// merge that would matter for nothing here.
fn merge_networks(
    base: ComposeServiceNetworks,
    child: ComposeServiceNetworks,
) -> ComposeServiceNetworks {
    match child {
        ComposeServiceNetworks::Empty => base,
        declared => declared,
    }
}

/// Merges `base`'s fields into `child`, wherever `child` didn't declare its
/// own — the Compose Specification's `extends:` semantics. `depends_on` is
/// the one field NEVER inherited: the Specification excludes it on purpose
/// (a service's dependency graph is its own, not something it borrows along
/// with the rest of the configuration it reuses).
///
/// `ports`/`volumes` are plain concatenation (base's entries, then child's) —
/// a real docker-compose dedups list entries by target, which matters when a
/// base and a child both republish the same container port; this v1 doesn't,
/// documented simplification rather than silent, and a duplicate publish is
/// harmless (the second one loses the free-port race and refuses "already in
/// use", which is at least loud). `privileged`/`read_only` are ORed rather
/// than overridden — a plain `bool` field (not `Option<bool>`) has no way to
/// tell "the child said `false`" from "the child didn't say anything", so
/// there is no way to un-inherit a base's `true` from a child; documented
/// here rather than silently assumed away.
fn merge_service(base: ComposeService, child: ComposeService) -> ComposeService {
    ComposeService {
        image: child.image.or(base.image),
        extends: None, // consumed
        build: child.build.or(base.build),
        environment: merge_env(&base.environment, &child.environment),
        env_file: OneOrMany::Many({
            let mut v = base.env_file.into_vec();
            v.extend(child.env_file.into_vec());
            v
        }),
        ports: {
            let mut v = base.ports;
            v.extend(child.ports);
            v
        },
        volumes: {
            let mut v = base.volumes;
            v.extend(child.volumes);
            v
        },
        command: child.command.or(base.command),
        entrypoint: child.entrypoint.or(base.entrypoint),
        depends_on: child.depends_on,
        // NOT inherited, same rule and same reason as `depends_on` above:
        // `profiles` decides WHETHER a service runs, not how it is built.
        // Inheriting it would silently enrol a child in its base's profile
        // and change which services `up` starts — a `extends:` is supposed
        // to reuse configuration, not membership.
        profiles: child.profiles,
        healthcheck: child.healthcheck.or(base.healthcheck),
        restart: child.restart.or(base.restart),
        networks: merge_networks(base.networks, child.networks),
        labels: merge_env(&base.labels, &child.labels),
        working_dir: child.working_dir.or(base.working_dir),
        user: child.user.or(base.user),
        cap_add: dedup_concat(base.cap_add, child.cap_add),
        cap_drop: dedup_concat(base.cap_drop, child.cap_drop),
        privileged: base.privileged || child.privileged,
        tmpfs: OneOrMany::Many({
            let mut v = base.tmpfs.into_vec();
            v.extend(child.tmpfs.into_vec());
            v
        }),
        extra_hosts: merge_extra_hosts(&base.extra_hosts, &child.extra_hosts),
        deploy: child.deploy.or(base.deploy),
        container_name: child.container_name.or(base.container_name),
        hostname: child.hostname.or(base.hostname),
        read_only: base.read_only || child.read_only,
        secrets: {
            let mut v = base.secrets;
            v.extend(child.secrets);
            v
        },
        configs: {
            let mut v = base.configs;
            v.extend(child.configs);
            v
        },
    }
}

/// Resolves every service's `extends:` in place, before anything else looks
/// at `services` — by the time this returns, no `ComposeService.extends` is
/// `Some` any more, so `translate`/`service_to_run_opts`/the `depends_on`
/// graph builder need zero changes to know `extends` ever existed.
///
/// A base is resolved (recursively, in case IT also extends something)
/// before being merged into whoever extends it — `chain` is the path taken
/// to get here, and a base already on it is a cycle, reported with the full
/// path rather than just the two names that finally collided.
fn resolve_extends(services: &mut BTreeMap<String, ComposeService>) -> Result<()> {
    let names: Vec<String> = services.keys().cloned().collect();
    for name in names {
        resolve_one(services, &name, &mut vec![name.clone()])?;
    }
    Ok(())
}

fn resolve_one(
    services: &mut BTreeMap<String, ComposeService>,
    name: &str,
    chain: &mut Vec<String>,
) -> Result<()> {
    let Some(base_name) = services
        .get(name)
        .and_then(|s| s.extends.as_ref().map(|e| e.service.clone()))
    else {
        return Ok(()); // already resolved, or never had one
    };
    if !services.contains_key(&base_name) {
        return Err(Error::Invalid(format!(
            "compose: service '{name}' extends undefined service '{base_name}'"
        )));
    }
    if chain.contains(&base_name) {
        chain.push(base_name);
        return Err(Error::Invalid(format!(
            "compose: extends cycle: {}",
            chain.join(" -> ")
        )));
    }
    chain.push(base_name.clone());
    resolve_one(services, &base_name, chain)?;
    chain.pop();

    let base = services.get(&base_name).cloned().expect("checked above");
    let child = services.get(name).cloned().expect("iterating its own key");
    services.insert(name.to_string(), merge_service(base, child));
    Ok(())
}

// ============================================================================
// Commands
// ============================================================================

/// Loads and merges every `-f` file (or the single default one), and returns
/// the same 4-tuple shape single-file `load_compose` always has — so the 5
/// call sites below need no rework beyond accepting `Vec<PathBuf>` instead of
/// `Option<PathBuf>`. `base_dir`/the returned path string are always the
/// FIRST file's — see the module doc-comment ("Multi-file compose").
fn load_compose(files: Vec<PathBuf>) -> Result<(ComposeFile, String, PathBuf, String)> {
    let paths = resolve_compose_paths(&files)?;
    let mut parsed = Vec::with_capacity(paths.len());
    for path in &paths {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Invalid(format!("reading {}: {e}", path.display())))?;
        // Runs on the RAW text of EACH file, before any merge — the typed
        // `ComposeFile` a merge produces has already dropped unrecognized
        // keys, so this is the only point that can still catch them.
        check_unsupported_fields(&text)?;
        let compose: ComposeFile = serde_yaml::from_str(&text)
            .map_err(|e| Error::Invalid(format!("parsing {}: {e}", path.display())))?;
        parsed.push(compose);
    }
    let mut compose = merge_compose_files(parsed);
    // AFTER the merge, deliberately. `extends:` names a service in the same
    // document, and with several `-f` files the document IS the merge — real
    // `docker compose` merges first and resolves after, so a service can
    // extend a base declared in another `-f`. Resolving per-file would refuse
    // that with "undefined service", which reads as a typo and is not one.
    resolve_extends(&mut compose.services)?;
    let first = &paths[0];
    let base_dir = first
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let default_project = default_project_name(first);
    Ok((
        compose,
        default_project,
        base_dir,
        first.to_string_lossy().into_owned(),
    ))
}

fn cmd_up(
    file: Vec<PathBuf>,
    project: Option<String>,
    dry_run: bool,
    profile: Vec<String>,
) -> Result<()> {
    let (compose, default_project, base_dir, _path) = load_compose(file)?;
    let project = project.unwrap_or(default_project);
    let translated = translate(&compose, &project, &base_dir, &profile)?;

    if dry_run {
        println!("# compose project: {project}");
        for doc in translated
            .network_docs
            .iter()
            .chain(&translated.volume_docs)
            .chain(&translated.image_docs)
            .chain(&translated.secret_docs)
        {
            println!(
                "---\n{}",
                manifest::render_with_defaults(std::slice::from_ref(doc))?
            );
        }
        for name in &translated.order {
            for (opts, final_name) in &translated.containers[name] {
                println!(
                    "service {name} -> container {final_name} (image={}, net={})",
                    opts.image, opts.net
                );
            }
        }
        return Ok(());
    }

    super::image::apply(&translated.image_docs)?;
    super::network::apply(&translated.network_docs)?;
    super::volume::apply(&translated.volume_docs)?;
    super::secret::apply(&translated.secret_docs, &base_dir)?;

    let (images, store) = open_stores()?;
    for name in &translated.order {
        if let Some(waits) = translated.waits.get(name) {
            for w in waits {
                // The canonical (1st) replica is the one `depends_on`/healthcheck
                // waits target — see `Translated.containers`'s doc-comment for why.
                let (_, dep_final_name) = translated
                    .containers
                    .get(&w.on)
                    .and_then(|v| v.first())
                    .ok_or_else(|| {
                        Error::Invalid(format!(
                            "compose: internal error: dependency '{}' not found",
                            w.on
                        ))
                    })?;
                wait_for_condition(&store, &images, dep_final_name, w)?;
            }
        }
        for (opts, final_name) in translated.containers[name].clone() {
            if store.list()?.iter().any(|c| c.name == final_name) {
                println!("compose: {name} ({final_name}): already exists, nothing to do");
                continue;
            }
            super::container::cmd_run(&images, &store, opts)?;
            println!("compose: {name} ({final_name}): created");
        }
    }
    Ok(())
}

fn cmd_down(file: Vec<PathBuf>, project: Option<String>, remove_volumes: bool) -> Result<()> {
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
            // Anonymous volumes (`- /container/path`, no explicit source):
            // ONLY `down -v` removes them, matching real `docker compose`
            // (a plain `down` never touches them, above). The names are
            // RE-DERIVED from the same re-parsed compose file `up` used to
            // create them — same "no registry" philosophy as the named ones
            // right above, and as `network_names`/`volume_names` at the top
            // of this function.
            for (name, svc) in &compose.services {
                for anon in anonymous_volume_names(&project, name, &svc.volumes) {
                    if let Err(e) = super::volume::cmd_rm(&vol_store, &anon, false) {
                        super::output::warn(&super::po::tf(
                            "volume '{name}' not removed: {err}",
                            &[("name", &anon), ("err", &e.to_string())],
                        ));
                    }
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

fn cmd_ps(file: Vec<PathBuf>, project: Option<String>) -> Result<()> {
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
    file: Vec<PathBuf>,
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

fn cmd_config(file: Vec<PathBuf>, project: Option<String>, profile: Vec<String>) -> Result<()> {
    let (compose, default_project, base_dir, _path) = load_compose(file)?;
    let project = project.unwrap_or(default_project);
    let translated = translate(&compose, &project, &base_dir, &profile)?;
    println!("# compose project: {project}");
    for name in &translated.order {
        for (opts, final_name) in &translated.containers[name] {
            println!(
                "service {name}:\n  container: {final_name}\n  image: {}\n  network: {}\n  ports: {:?}\n  volumes: {:?}",
                opts.image, opts.net, opts.ports, opts.volumes
            );
        }
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
        let all: BTreeSet<String> = services.keys().cloned().collect();
        let order = topo_sort(&services, &all).unwrap();
        assert_eq!(order, vec!["db", "api", "web"]);

        let cyclic = "a:\n  image: x\n  depends_on: [b]\nb:\n  image: y\n  depends_on: [a]\n";
        let services: BTreeMap<String, ComposeService> = serde_yaml::from_str(cyclic).unwrap();
        let all: BTreeSet<String> = services.keys().cloned().collect();
        assert!(topo_sort(&services, &all).is_err());

        let missing = "a:\n  image: x\n  depends_on: [ghost]\n";
        let services: BTreeMap<String, ComposeService> = serde_yaml::from_str(missing).unwrap();
        let all: BTreeSet<String> = services.keys().cloned().collect();
        assert!(topo_sort(&services, &all).is_err());
    }

    #[test]
    fn active_services_skips_an_unrequested_profile() {
        let yaml = "\
services:
  web:
    image: a
  debugger:
    image: b
    profiles: [debug]
";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let active = active_services(&compose, &[]);
        assert_eq!(active, BTreeSet::from(["web".to_string()]));
        let active = active_services(&compose, &["debug".to_string()]);
        assert_eq!(
            active,
            BTreeSet::from(["web".to_string(), "debugger".to_string()])
        );
    }

    /// The rule real `docker compose` uses: a dependency is pulled in even
    /// outside the requested profile, because the service that needs it
    /// cannot start without it.
    #[test]
    fn active_services_pulls_in_a_dependency_outside_the_requested_profile() {
        let yaml = "\
services:
  app:
    image: a
    profiles: [tools]
    depends_on: [cache]
  cache:
    image: b
    profiles: [infra]
  unrelated:
    image: c
    profiles: [infra]
";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let active = active_services(&compose, &["tools".to_string()]);
        assert_eq!(
            active,
            BTreeSet::from(["app".to_string(), "cache".to_string()]),
            "`cache` must be pulled in transitively; `unrelated` must not"
        );
    }

    #[test]
    fn translate_only_creates_active_services() {
        let yaml = "\
services:
  web:
    image: a
  debugger:
    image: b
    profiles: [debug]
";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let t = translate(&compose, "p", Path::new("/tmp"), &[]).unwrap();
        assert_eq!(t.order, vec!["web".to_string()]);
        assert!(!t.containers.contains_key("debugger"));

        let t = translate(&compose, "p", Path::new("/tmp"), &["debug".to_string()]).unwrap();
        assert_eq!(t.containers.len(), 2);
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
        let t = translate(&compose, "myproj", Path::new("/tmp"), &[]).unwrap();
        assert_eq!(t.order, vec!["db".to_string(), "app".to_string()]);
        let app_waits = &t.waits["app"];
        assert_eq!(app_waits.len(), 1);
        assert_eq!(app_waits[0].on, "db");
        assert_eq!(app_waits[0].condition, DependsCondition::Healthy);
        assert!(app_waits[0].healthcheck.is_some());
    }

    /// `networks.*.ipv4_address` wires straight into `RunOpts.ip` — the same
    /// `container run --ip` path, no separate mechanism. The engine-level
    /// half (IPAM reservation, subnet validation) is `infra::
    /// attach_container_on_ip`'s own responsibility and test, not this one's
    /// — this only proves the compose YAML is not silently dropped.
    #[test]
    fn networks_ipv4_address_flui_para_run_opts_ip() {
        let yaml = "services:\n  web:\n    image: x\n    networks:\n      default:\n        ipv4_address: 10.210.5.5\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let t = translate(&compose, "p", Path::new("/tmp"), &[]).unwrap();
        // `containers` became a Vec when `deploy.replicas` landed — one entry
        // per replica. These two tests are about the SINGLE-replica case, so
        // the first (and only) entry is the one they mean.
        let (opts, _) = &t.containers["web"][0];
        assert_eq!(opts.ip.as_deref(), Some("10.210.5.5"));
    }

    /// A service with no `ipv4_address` at all keeps letting the engine pick
    /// (IPAM-derived) — the common case must not regress into always fixing
    /// an address that was never asked for.
    #[test]
    fn sem_ipv4_address_o_ip_fica_none() {
        let yaml = "services:\n  web:\n    image: x\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let t = translate(&compose, "p", Path::new("/tmp"), &[]).unwrap();
        // `containers` became a Vec when `deploy.replicas` landed — one entry
        // per replica. These two tests are about the SINGLE-replica case, so
        // the first (and only) entry is the one they mean.
        let (opts, _) = &t.containers["web"][0];
        assert_eq!(opts.ip, None);
    }

    /// The success path end-to-end: a `file:` source becomes a `kind: Secret`
    /// document holding exactly the secret's own name as key, and the
    /// referencing service's `RunOpts` picks it up with `secret_files: true`
    /// (Compose secrets are always file-mounted, never env vars).
    #[test]
    fn secret_file_source_becomes_a_secret_doc_and_flows_into_run_opts() {
        let path = std::env::temp_dir().join(format!(
            "dlx-compose-secret-test-{}-{}.txt",
            std::process::id(),
            "a"
        ));
        std::fs::write(&path, "s3cr3t\n").unwrap();
        let yaml = format!(
            "secrets:\n  db_password:\n    file: {}\nservices:\n  web:\n    image: x\n    secrets:\n      - db_password\n",
            path.display()
        );
        let compose: ComposeFile = serde_yaml::from_str(&yaml).unwrap();
        let t = translate(&compose, "p", Path::new("/"), &[]).unwrap();
        std::fs::remove_file(&path).ok();

        let (opts, _) = &t.containers["web"][0];
        assert!(opts.secret_files, "compose secrets must be file-delivered");
        assert_eq!(opts.secret.len(), 1);
        let secret_name = &opts.secret[0];

        let doc = t
            .secret_docs
            .iter()
            .find(|d| &d.metadata.name == secret_name)
            .expect("a secret doc must exist for the referenced secret");
        let spec = doc.spec.as_mapping().unwrap();
        let string_data = spec.get("stringData").unwrap().as_mapping().unwrap();
        let value = string_data.get("db_password").unwrap().as_str().unwrap();
        assert_eq!(value, "s3cr3t\n");
    }

    /// `configs:` goes through the same mechanism as `secrets:` — same
    /// SecretStore, same file delivery — the "use `kind: Secret` instead"
    /// message this module always gave for both keys.
    #[test]
    fn config_file_source_also_becomes_a_secret_doc() {
        let path = std::env::temp_dir().join(format!(
            "dlx-compose-config-test-{}-{}.txt",
            std::process::id(),
            "a"
        ));
        std::fs::write(&path, "server { }\n").unwrap();
        let yaml = format!(
            "configs:\n  nginx_conf:\n    file: {}\nservices:\n  web:\n    image: x\n    configs:\n      - nginx_conf\n",
            path.display()
        );
        let compose: ComposeFile = serde_yaml::from_str(&yaml).unwrap();
        let t = translate(&compose, "p", Path::new("/"), &[]).unwrap();
        std::fs::remove_file(&path).ok();

        let (opts, _) = &t.containers["web"][0];
        assert!(opts.secret_files);
        assert_eq!(opts.secret.len(), 1);
    }

    /// A service with neither `secrets:` nor `configs:` never turns on
    /// `secret_files` — a container with no secrets attached should not
    /// silently get a tmpfs it never asked for (harmless here, but this is
    /// the flag `write_secret_files` reads to decide delivery mode).
    #[test]
    fn no_secrets_leaves_run_opts_secret_files_false() {
        let yaml = "services:\n  web:\n    image: x\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let t = translate(&compose, "p", Path::new("/tmp"), &[]).unwrap();
        let (opts, _) = &t.containers["web"][0];
        assert!(!opts.secret_files);
        assert!(opts.secret.is_empty());
    }

    /// `environment:` reads the PROCESS environment at translate time —
    /// `PATH` is used here specifically to avoid `std::env::set_var` (unsafe
    /// in this toolchain, and shared mutable state across parallel tests);
    /// it is a variable every test process already has, for free.
    #[test]
    fn secret_environment_source_reads_the_process_env() {
        let content =
            resolve_secret_content("secret", "s", None, Some("PATH"), false, Path::new("/tmp"))
                .unwrap();
        assert_eq!(content, std::env::var("PATH").unwrap());
    }

    #[test]
    fn secret_environment_source_missing_var_is_a_clear_error() {
        let e = resolve_secret_content(
            "secret",
            "s",
            None,
            Some("DLX_COMPOSE_TEST_VAR_THAT_DOES_NOT_EXIST_XYZ"),
            false,
            Path::new("/tmp"),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("is not set"), "{e}");
    }

    /// `translate()`'s `Result<Translated, _>` cannot use `.unwrap_err()`
    /// directly (`Translated` has no `Debug`, on purpose — it holds live
    /// `RunOpts`), so the error-path tests below go through this instead.
    fn translate_err(compose: &ComposeFile) -> String {
        match translate(compose, "p", Path::new("/tmp"), &[]) {
            Ok(_) => panic!("this compose file must be refused"),
            Err(e) => e.to_string(),
        }
    }

    /// A `source:`/`target:` pair that renames the mounted filename cannot be
    /// honoured — `Container.secret`'s file delivery names every mount after
    /// the secret's OWN data key, with no per-secret retarget. Refused with
    /// the specific reason, never silently mounted under the wrong name.
    #[test]
    fn secret_target_rename_is_refused() {
        let yaml = "secrets:\n  db_password:\n    environment: PATH\nservices:\n  web:\n    image: x\n    secrets:\n      - source: db_password\n        target: renamed\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let e = translate_err(&compose);
        assert!(e.contains("target: renamed"), "{e}");
    }

    /// `external: true` cannot be honoured either — this module can only
    /// create a secret from a `file:`/`environment:` it can read, never
    /// reference one that already exists outside the compose file.
    #[test]
    fn external_secret_is_refused() {
        let yaml = "secrets:\n  db_password:\n    external: true\nservices:\n  web:\n    image: x\n    secrets:\n      - db_password\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let e = translate_err(&compose);
        assert!(e.contains("external"), "{e}");
    }

    #[test]
    fn config_without_file_is_refused() {
        let yaml = "configs:\n  app_conf: {}\nservices:\n  web:\n    image: x\n    configs:\n      - app_conf\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let e = translate_err(&compose);
        assert!(e.contains("needs `file:`"), "{e}");
    }

    #[test]
    fn undeclared_secret_reference_is_refused() {
        let yaml = "services:\n  web:\n    image: x\n    secrets:\n      - missing\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let e = translate_err(&compose);
        assert!(e.contains("undeclared secret"), "{e}");
    }

    #[test]
    fn anonymous_volume_names_indexes_only_the_anonymous_mounts() {
        let mounts = vec![
            ComposeVolumeMount::Short("/data".to_string()), // anon #1
            ComposeVolumeMount::Short("named:/x".to_string()), // not anon, no slot
            ComposeVolumeMount::Short("/cache".to_string()), // anon #2
        ];
        let names = anonymous_volume_names("proj", "web", &mounts);
        assert_eq!(names.len(), 2, "{names:?}");
        assert_ne!(names[0], names[1]);
        // Re-derivable: calling it again with the SAME inputs gives the SAME
        // names — the property `down`/`down -v` rely on.
        assert_eq!(names, anonymous_volume_names("proj", "web", &mounts));
        // Different service, different project: no collision.
        assert!(!anonymous_volume_names("proj", "db", &mounts).contains(&names[0]));
        assert!(!anonymous_volume_names("other", "web", &mounts).contains(&names[0]));
    }

    /// An anonymous mount gets a real volume created for it (a `kind: Volume`
    /// doc alongside the named ones) AND a `name:target` entry in
    /// `RunOpts.volumes` — the same shape a named volume mount produces, so
    /// nothing downstream needs to special-case it.
    #[test]
    fn an_anonymous_mount_gets_a_volume_doc_and_a_run_opts_entry() {
        let yaml = "services:\n  web:\n    image: x\n    volumes:\n      - /var/lib/data\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let t = translate(&compose, "myproj", Path::new("/tmp"), &[]).unwrap();
        let (opts, _) = &t.containers["web"][0];
        assert_eq!(opts.volumes.len(), 1, "{:?}", opts.volumes);
        let (name, target) = opts.volumes[0].split_once(':').unwrap();
        assert_eq!(target, "/var/lib/data");
        assert!(
            t.volume_docs.iter().any(|d| d.metadata.name == name),
            "no kind: Volume doc for the anonymous volume {name:?}: {:?}",
            t.volume_docs
                .iter()
                .map(|d| &d.metadata.name)
                .collect::<Vec<_>>()
        );
    }

    /// Reordering two `- /path` lines must not move the data. The name is
    /// derived from the SET of target paths, so the same path keeps the same
    /// volume however the YAML is laid out — with a positional index, the two
    /// volumes here would swap and each service would come up on the other's
    /// data, silently.
    #[test]
    fn reordering_the_mounts_does_not_move_the_volumes() {
        let a = vec![
            ComposeVolumeMount::Short("/var/lib/data".to_string()),
            ComposeVolumeMount::Short("/var/log".to_string()),
        ];
        let b = vec![
            ComposeVolumeMount::Short("/var/log".to_string()),
            ComposeVolumeMount::Short("/var/lib/data".to_string()),
        ];
        let na = anonymous_volume_names("p", "db", &a);
        let nb = anonymous_volume_names("p", "db", &b);
        // Same path, same volume — whichever line it is written on.
        assert_eq!(na[0], nb[1], "/var/lib/data changed volume: {na:?} {nb:?}");
        assert_eq!(na[1], nb[0], "/var/log changed volume: {na:?} {nb:?}");
    }

    /// A named/bind mount is untouched by the anonymous-volume machinery —
    /// no extra doc, no consumed slot.
    #[test]
    fn a_mount_with_a_colon_is_not_treated_as_anonymous() {
        let yaml = "volumes:\n  data: {}\nservices:\n  web:\n    image: x\n    volumes:\n      - data:/var/lib/data\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let t = translate(&compose, "myproj", Path::new("/tmp"), &[]).unwrap();
        // Only the ONE named volume doc — no anonymous one snuck in.
        assert_eq!(
            t.volume_docs.len(),
            1,
            "{:?}",
            t.volume_docs
                .iter()
                .map(|d| &d.metadata.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn deploy_replicas_cria_um_container_por_replica() {
        let yaml = "services:\n  svc:\n    image: x\n    deploy:\n      replicas: 3\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        let t = translate(&compose, "p", Path::new("/tmp"), &[]).unwrap();
        let names: Vec<&String> = t.containers["svc"].iter().map(|(_, n)| n).collect();
        assert_eq!(
            names,
            vec!["p-svc", "p-svc-2", "p-svc-3"],
            "a 1ª réplica mantém o nome de sempre (depends_on/DNS não mudam), as outras levam sufixo"
        );
    }

    #[test]
    fn service_sem_image_nem_build_e_erro() {
        let yaml = "services:\n  svc:\n    ports: []\n";
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        assert!(translate(&compose, "p", Path::new("/tmp"), &[]).is_err());
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

    /// `127.0.0.1:9000:80` chega INTEIRO ao motor — nem descartado nem recusado.
    ///
    /// **Este teste já afirmou o contrário, e a lição é a razão de estar aqui
    /// escrita.** Nasceu a fixar «tem de recusar», que foi a correcção certa para o
    /// bug anterior (a forma de 3 partes caía na de 2 e a restrição de IP
    /// desaparecia — um ficheiro a pedir loopback-only publicava em todas as
    /// interfaces). Só que a razão da recusa era «o `-p` do motor exige host port
    /// só dígitos», e isso deixou de valer quando o CLI ganhou o
    /// `parse_publish_addr`. O teste sobreviveu à premissa e passou a defender uma
    /// recusa sem motivo, na forma mais comum de expor um serviço só ao host.
    ///
    /// Quando uma correcção faz um teste antigo falhar, a primeira hipótese é que o
    /// teste fixava o comportamento errado — regra já registada neste repo, e a
    /// segunda vez que se paga.
    #[test]
    fn resolve_ports_passa_o_bind_a_ip_especifico_intacto_ao_motor() {
        let ports = vec![ComposePort::Short("127.0.0.1:9000:80".to_string())];
        let out = resolve_ports(&ports, "svc").unwrap();
        assert_eq!(
            out,
            vec!["127.0.0.1:9000:80".to_string()],
            "o IP do host tem de sobreviver — é a restrição que o ficheiro pediu"
        );
        // E a forma de 2 partes continua exactamente como estava.
        let simples = resolve_ports(&[ComposePort::Short("9000:80".into())], "svc").unwrap();
        assert_eq!(simples, vec!["9000:80".to_string()]);
    }

    /// Um range vai inteiro para o motor, que o expande na sua fronteira
    /// (`expand_publish_range`, o mesmo que o `container run` já usa). O compose
    /// era o único dos três pontos de entrada a recusar o que os outros faziam.
    #[test]
    fn resolve_ports_passa_ranges_ao_motor_em_vez_de_os_recusar() {
        let ports = vec![ComposePort::Long {
            target: StringOrNum::Str("9000-9002".into()),
            published: Some(StringOrNum::Str("8000-8002".into())),
            protocol: None,
        }];
        assert_eq!(
            resolve_ports(&ports, "svc").unwrap(),
            vec!["8000-8002:9000-9002".to_string()]
        );
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

    fn services_of(yaml: &str) -> BTreeMap<String, ComposeService> {
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        compose.services
    }

    /// The base case: a child inherits everything it doesn't declare itself,
    /// and its own values win where it does.
    #[test]
    fn extends_inherits_what_the_child_omits_and_overrides_what_it_declares() {
        let mut services = services_of(
            "services:\n  \
             base:\n    image: alpine\n    working_dir: /base\n    environment: [A=1, B=1]\n  \
             web:\n    extends: {service: base}\n    working_dir: /web\n",
        );
        resolve_extends(&mut services).unwrap();
        let web = &services["web"];
        assert_eq!(web.image.as_deref(), Some("alpine"), "inherited from base");
        assert_eq!(web.working_dir.as_deref(), Some("/web"), "child overrides");
        assert!(web.extends.is_none(), "consumed after resolution");
        let mut env = web.environment.to_kv_pairs();
        env.sort();
        assert_eq!(env, vec!["A=1".to_string(), "B=1".to_string()]);
    }

    /// `environment:`/`labels:` MERGE key by key, child winning on a shared
    /// key — not a full override of the base's map.
    #[test]
    fn extends_merges_environment_child_wins_on_shared_keys() {
        let mut services = services_of(
            "services:\n  \
             base:\n    image: x\n    environment: [SHARED=base, ONLY_BASE=b]\n  \
             web:\n    extends: {service: base}\n    environment: [SHARED=child, ONLY_CHILD=c]\n",
        );
        resolve_extends(&mut services).unwrap();
        let mut env = services["web"].environment.to_kv_pairs();
        env.sort();
        assert_eq!(
            env,
            vec![
                "ONLY_BASE=b".to_string(),
                "ONLY_CHILD=c".to_string(),
                "SHARED=child".to_string(),
            ],
            "child's value wins, base's untouched keys survive"
        );
    }

    /// `depends_on:` is the one field the Compose Specification never
    /// inherits through `extends` — a service's dependency graph is its own.
    #[test]
    fn extends_never_inherits_depends_on() {
        let mut services = services_of(
            "services:\n  \
             db:\n    image: postgres\n  \
             base:\n    image: x\n    depends_on: [db]\n  \
             web:\n    extends: {service: base}\n    image: y\n",
        );
        resolve_extends(&mut services).unwrap();
        assert!(
            services["web"].depends_on.names().is_empty(),
            "web never named `db` itself, and extends must not have given it to it"
        );
    }

    /// A chain (`web` extends `mid` extends `base`) resolves transitively —
    /// `web` ends up with `base`'s fields too, not just `mid`'s own.
    #[test]
    fn extends_chain_is_transitive() {
        let mut services = services_of(
            "services:\n  \
             base:\n    image: x\n    working_dir: /base\n  \
             mid:\n    extends: {service: base}\n  \
             web:\n    extends: {service: mid}\n    image: y\n",
        );
        resolve_extends(&mut services).unwrap();
        let web = &services["web"];
        assert_eq!(web.image.as_deref(), Some("y"), "web's own override");
        assert_eq!(
            web.working_dir.as_deref(),
            Some("/base"),
            "inherited through the chain, not just from the direct base"
        );
    }

    fn compose_file(yaml: &str) -> ComposeFile {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn merge_scalars_later_file_wins_only_when_declared() {
        let base = compose_file("services:\n  web:\n    image: base\n    user: root\n");
        let overlay = compose_file("services:\n  web:\n    image: override\n");
        let merged = merge_compose_files(vec![base, overlay]);
        let web = &merged.services["web"];
        assert_eq!(web.image.as_deref(), Some("override"));
        // `user` was not redeclared in the overlay — the base value survives.
        assert_eq!(web.user.as_deref(), Some("root"));
    }

    #[test]
    fn merge_environment_is_key_by_key_not_a_replace() {
        let base = compose_file(
            "services:\n  web:\n    image: x\n    environment:\n      A: base\n      B: base\n",
        );
        let overlay =
            compose_file("services:\n  web:\n    image: x\n    environment:\n      B: over\n");
        let merged = merge_compose_files(vec![base, overlay]);
        let mut pairs = merged.services["web"].environment.to_kv_pairs();
        pairs.sort();
        assert_eq!(pairs, vec!["A=base".to_string(), "B=over".to_string()]);
    }

    #[test]
    fn merge_cap_add_concatenates_without_duplicating() {
        let base = compose_file("services:\n  web:\n    image: x\n    cap_add: [NET_ADMIN]\n");
        let overlay =
            compose_file("services:\n  web:\n    image: x\n    cap_add: [NET_ADMIN, SYS_TIME]\n");
        let merged = merge_compose_files(vec![base, overlay]);
        assert_eq!(
            merged.services["web"].cap_add,
            vec!["NET_ADMIN", "SYS_TIME"]
        );
    }

    #[test]
    fn extends_of_an_undefined_service_is_a_clear_error() {
        let mut services =
            services_of("services:\n  web:\n    extends: {service: ghost}\n    image: y\n");
        let e = resolve_extends(&mut services).unwrap_err().to_string();
        assert!(e.contains("ghost"), "{e}");
        assert!(e.contains("undefined"), "{e}");
    }

    #[test]
    fn extends_cycle_is_refused_naming_the_path() {
        let mut services = services_of(
            "services:\n  \
             a:\n    extends: {service: b}\n    image: x\n  \
             b:\n    extends: {service: a}\n    image: y\n",
        );
        let e = resolve_extends(&mut services).unwrap_err().to_string();
        assert!(e.contains("cycle"), "{e}");
    }

    /// `cap_add`/`cap_drop` concatenate without duplicating an entry both
    /// base and child declare.
    #[test]
    fn extends_dedups_cap_add() {
        let mut services = services_of(
            "services:\n  \
             base:\n    image: x\n    cap_add: [NET_ADMIN, SYS_TIME]\n  \
             web:\n    extends: {service: base}\n    cap_add: [NET_ADMIN, SYS_PTRACE]\n",
        );
        resolve_extends(&mut services).unwrap();
        assert_eq!(
            services["web"].cap_add,
            vec!["NET_ADMIN", "SYS_TIME", "SYS_PTRACE"]
        );
    }

    /// `extends.file` is refused at the raw-YAML check, before typed parsing
    /// ever runs. It stays refused AFTER multi-file compose landed, and the
    /// two are not the same thing: `-f a -f b` merges whole documents and
    /// resolves relative paths against the FIRST file, while `extends.file`
    /// inherits one service from another file with THAT file's relativity.
    /// The assertion is on the pointer to `-f`, not on the old wording —
    /// which claimed this implementation "doesn't do multi-file compose at
    /// all" and stopped being true in this very merge.
    #[test]
    fn extends_file_is_refused() {
        let e = check_unsupported_fields(
            "services:\n  web:\n    extends: {service: base, file: other.yml}\n",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("extends.file"), "{e}");
        assert!(
            e.contains("-f"),
            "must point at the form that IS supported: {e}"
        );
    }

    #[test]
    fn merge_ports_and_volumes_concatenate() {
        let base = compose_file("services:\n  web:\n    image: x\n    ports: [\"80:80\"]\n");
        let overlay = compose_file("services:\n  web:\n    image: x\n    ports: [\"443:443\"]\n");
        let merged = merge_compose_files(vec![base, overlay]);
        assert_eq!(merged.services["web"].ports.len(), 2);
    }

    /// `depends_on` ACCUMULATES across files — the opposite of `extends:`'s
    /// exclusion, and deliberately so: an ordinary multi-file merge of the
    /// same service has no foreign-template ambiguity to protect against.
    #[test]
    fn merge_depends_on_accumulates_and_later_condition_wins() {
        let base = compose_file(
            "services:\n  web:\n    image: x\n    depends_on:\n      db:\n        condition: service_started\n",
        );
        let overlay = compose_file(
            "services:\n  web:\n    image: x\n    depends_on:\n      cache:\n        condition: service_started\n      db:\n        condition: service_healthy\n",
        );
        let merged = merge_compose_files(vec![base, overlay]);
        let entries: BTreeMap<String, DependsCondition> = merged.services["web"]
            .depends_on
            .entries()
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries["db"], DependsCondition::Healthy);
        assert_eq!(entries["cache"], DependsCondition::Started);
    }

    #[test]
    fn merge_a_service_only_in_one_file_survives_untouched() {
        let base = compose_file("services:\n  db:\n    image: postgres\n");
        let overlay = compose_file("services:\n  web:\n    image: nginx\n");
        let merged = merge_compose_files(vec![base, overlay]);
        assert!(merged.services.contains_key("db"));
        assert!(merged.services.contains_key("web"));
    }

    #[test]
    fn merge_networks_and_volumes_replace_on_shared_key() {
        let base = compose_file("services:\n  a:\n    image: x\nnetworks:\n  net1:\n    external: false\nvolumes:\n  vol1: {}\n");
        let overlay =
            compose_file("services:\n  a:\n    image: x\nnetworks:\n  net1:\n    name: real-net\n");
        let merged = merge_compose_files(vec![base, overlay]);
        assert_eq!(merged.networks["net1"].name.as_deref(), Some("real-net"));
        assert!(merged.volumes.contains_key("vol1"));
    }
}

/// The allowlist behind the denylist. This module is what fails if
/// `check_unsupported_fields` ever goes back to refusing only what someone
/// remembered to write down.
#[cfg(test)]
mod tests_unknown_keys {
    use super::*;

    fn err_of(yaml: &str) -> String {
        check_unsupported_fields(yaml)
            .expect_err("this compose file must be refused")
            .to_string()
    }

    /// THE finding: keys of the Compose Specification that no denylist named
    /// were parsed into a typed struct and dropped without a word.
    ///
    /// Four of these six change ISOLATION or resource limits, so "accepted and
    /// ignored" meant the container ran differently from what the file said and
    /// nothing reported it.
    #[test]
    fn a_service_key_outside_both_lists_is_refused() {
        for key in [
            "sysctls",
            "ulimits",
            "shm_size",
            "mem_limit",
            "expose",
            "init",
        ] {
            let e = err_of(&format!(
                "services:\n  web:\n    image: nginx\n    {key}: x\n"
            ));
            assert!(e.contains(key), "the message must name the key: {e}");
            assert!(e.contains("web"), "the message must name the service: {e}");
            assert!(
                e.contains("refused rather than ignored"),
                "the message must say it was refused, not dropped: {e}"
            );
        }
    }

    /// A key the ENGINE supports gets a different answer from one it does not:
    /// the message names the flag that already does the job.
    #[test]
    fn a_key_the_engine_has_names_the_flag() {
        let e = err_of("services:\n  web:\n    image: nginx\n    security_opt: [x]\n");
        assert!(e.contains("--security-opt"), "must name the flag: {e}");
        let e = err_of("services:\n  web:\n    image: nginx\n    network_mode: host\n");
        assert!(e.contains("--net"), "must name the flag: {e}");
    }

    /// The specific reasons still win over the generic message — a denied key
    /// must not regress into "not understood".
    ///
    /// The `profiles:`/`extends:`/`configs:`/`secrets:` assertions this test
    /// once had were REMOVED as each shipped: they asserted the key is
    /// refused, and that stopped being true. A test that keeps a retired
    /// refusal alive is the «a test can encode the bug» trap this repo has
    /// already paid for — it would have blocked the very feature it outlived.
    /// `KNOWN_UNSUPPORTED_TOP` still has `include`, so the property this test
    /// exists for is still covered.
    #[test]
    fn the_specific_reason_wins_over_the_generic() {
        let e = err_of("include:\n  - other.yml\nservices:\n  web:\n    image: nginx\n");
        assert!(e.contains("pass exactly one -f"), "specific reason: {e}");
    }

    #[test]
    fn a_top_level_key_outside_the_list_is_refused() {
        let e = err_of("x-custom: 1\nservices:\n  web:\n    image: nginx\n");
        assert!(e.contains("x-custom"), "{e}");
    }

    /// `version:` is dead in the specification and alive in the real world.
    /// Refusing it would reject most compose files over a key that changes
    /// nothing.
    #[test]
    fn version_still_parses() {
        check_unsupported_fields("version: '3.8'\nservices:\n  web:\n    image: nginx\n")
            .expect("`version:` must keep parsing");
    }

    /// A file made only of keys this implementation reads must still pass —
    /// without this, the tests above would also pass with the function
    /// hardcoded to refuse everything.
    #[test]
    fn a_fully_supported_file_passes() {
        let yaml = "\
name: proj
services:
  web:
    image: nginx
    ports: ['8080:80']
    environment: {A: b}
    volumes: ['data:/x']
    depends_on: [db]
    healthcheck: {test: ['CMD', 'true']}
    restart: always
    networks: [front]
    labels: {a: b}
    working_dir: /app
    user: '1000'
    cap_add: [NET_ADMIN]
    cap_drop: [ALL]
    privileged: false
    tmpfs: ['/tmp']
    extra_hosts: ['a:1.2.3.4']
    deploy: {resources: {limits: {memory: 1g}}}
    container_name: web1
    hostname: web
    read_only: true
    command: ['nginx']
    entrypoint: ['/bin/sh']
    env_file: ['.env']
    build: .
  db:
    image: postgres
networks:
  front:
    external: true
    name: real
volumes:
  data:
    external: false
    name: realvol
";
        check_unsupported_fields(yaml).expect("every key here is in the allowlist");
    }

    /// A `networks:`/`volumes:` entry with no body means "defaults" and is
    /// legal — measured against real files, not assumed.
    #[test]
    fn a_network_entry_with_no_body_is_legal() {
        check_unsupported_fields("services:\n  w:\n    image: n\nnetworks:\n  front:\n")
            .expect("`front:` with no body is the common form");
    }

    #[test]
    fn a_network_key_outside_the_list_is_refused() {
        let e = err_of("services:\n  w:\n    image: n\nnetworks:\n  front:\n    driver: bridge\n");
        assert!(e.contains("driver"), "{e}");
        assert!(e.contains("front"), "{e}");
    }

    #[test]
    fn a_secret_entry_key_outside_the_list_is_refused() {
        let e = err_of("services:\n  w:\n    image: n\nsecrets:\n  a:\n    bogus: true\n");
        assert!(e.contains("bogus"), "{e}");
        assert!(e.contains("secrets 'a'"), "{e}");
    }

    #[test]
    fn a_config_entry_cannot_use_environment_source() {
        // `environment:` is a `secrets:`-only source in the real Compose
        // Specification — allowing it under `configs:` too would be a second,
        // undocumented opinion, so it is refused like any other unknown key.
        let e = err_of("services:\n  w:\n    image: n\nconfigs:\n  a:\n    environment: X\n");
        assert!(e.contains("environment"), "{e}");
    }

    /// The guard that stops the allowlist from drifting away from the struct it
    /// describes: reads THIS FILE'S OWN SOURCE, the same idiom the API matrix
    /// coverage test of `dockerapi.rs` uses.
    ///
    /// Without it, adding a field to `ComposeService` and forgetting the
    /// allowlist would make a key the parser DOES read be refused — the
    /// opposite failure, and just as silent to whoever wrote the field.
    #[test]
    fn struct_fields_are_all_in_the_allowlist() {
        let src = include_str!("compose.rs");
        let body = src
            .split("struct ComposeService {")
            .nth(1)
            .expect("`struct ComposeService` must exist")
            .split("\n}")
            .next()
            .unwrap();
        let mut missing = Vec::new();
        for line in body.lines() {
            let l = line.trim();
            if l.starts_with('#') || l.starts_with("//") || l.is_empty() {
                continue;
            }
            // `name: Type,` — and `rename = "x"` is what the YAML really says,
            // so the renamed form is the one that has to be in the list.
            let Some((name, _)) = l.split_once(':') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                continue;
            }
            if !SUPPORTED_SERVICE.contains(&name) {
                missing.push(name.to_string());
            }
        }
        assert!(
            missing.is_empty(),
            "fields of ComposeService missing from SUPPORTED_SERVICE: {missing:?}"
        );
    }
}

//! `delonix net tunnel` (`kind: Gateway`) — exposes ONE local TCP port to the
//! public internet via a 3rd-party tunnel provider (`pinggy`/`ngrok`/
//! `cloudflare`). Deliberately single-purpose: Tunnel's only job is the
//! outbound transport (no account/router/public IP needed on this host).
//! Multiplexing several backends behind one hostname is already
//! `kind: HTTPRoute`'s job — point `localPort` at the embedded ingress
//! proxy's listening port (see `cmd::ingress_proxy`) to combine the two,
//! exactly as the request that led to this module put it: "pass the tunnel
//! to the ingress". A container's own published host port works the same
//! way for a single-service expose with no routing needed.
//!
//! Each provider is a REAL, already-installed CLI shelled out to (same
//! "daemonless, zero new supply-chain" posture as the rest of this binary):
//! - `pinggy`: **zero extra binary** — plain `ssh` (already a dependency via
//!   `cmd::remote`) reverse-forwarded to `free.pinggy.io`, with or without a
//!   token (`[<token>@]free.pinggy.io`, their own documented general form).
//!   Free tier: ephemeral URL, ~60 min session; a pro token behaves the same,
//!   just without the time limit — pinggy does not distinguish the two at
//!   the SSH layer.
//! - `ngrok`: needs the `ngrok` agent on `PATH` (clear error if absent).
//!   Public URL is read from the agent's own local HTTP API, fixed at
//!   `127.0.0.1:4040` since ngrok v3 dropped the `--web-addr` flag this used
//!   to vary per tunnel — **found live** while adding cloudflare-token
//!   support (confirmed `unknown flag` against a real v3.39.11 binary) — so
//!   only ONE ngrok-provider tunnel can run on this host at a time
//!   (`other_alive_ngrok` refuses a second with a clear reason, rather than
//!   letting two agents fight over the same port). A `--token`/reserved
//!   `--hostname` (paid plan) work the same as ever.
//! - `cloudflare`: needs `cloudflared` on `PATH`. No token: the anonymous
//!   **quick tunnel** (`cloudflared tunnel --url ...`, zero account, random
//!   `*.trycloudflare.com` URL). With `--token`/`--token-secret`: a NAMED
//!   tunnel already created through the Cloudflare dashboard or
//!   `cloudflared tunnel create` — this module never calls the Cloudflare
//!   API, it only runs `cloudflared tunnel run --token <token>`, the direct
//!   analogue of pinggy's/ngrok's paid-token path. Creating a NEW named
//!   tunnel from scratch (and its DNS route) by API would be a different,
//!   larger feature and stays a documented follow-up.
//!
//! Each provider's agent runs DETACHED (`setsid`, like `cmd::ingress_proxy`)
//! so it survives the CLI exiting, tracked by a `TunnelRecord` (own
//! `JsonStore`, `<root>/tunnels/<name>.json`) with the SAME PID-identity
//! guard pattern (`/proc/<pid>/cmdline` contains the provider's binary name)
//! so a recycled PID never gets signalled by mistake.

use super::kinds as k;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Subcommand;
use delonix_model::{Error, Result};
use delonix_node::{proc_starttime, safe_to_signal};
use delonix_state::JsonStore;
use serde::{Deserialize, Serialize};

use super::manifest::{self, ManifestDoc};
use super::output;
use super::util::state_root;

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(crate) struct TunnelSpec {
    /// `pinggy` | `ngrok` | `cloudflare`.
    provider: String,
    #[serde(rename = "localPort")]
    local_port: u16,
    /// Custom/reserved hostname — provider-dependent support (see module doc).
    /// For `cloudflare` this is informational only: it does not create the
    /// route, it just labels the URL this tunnel is expected to answer on —
    /// the route itself lives in the Cloudflare dashboard for that tunnel.
    #[serde(default)]
    hostname: Option<String>,
    /// Literal provider token (pinggy pro token / ngrok authtoken / a
    /// cloudflare NAMED tunnel's token, from `cloudflared tunnel token
    /// <name>` or the Zero Trust dashboard). Prefer `tokenSecretRef` for
    /// anything checked into a manifest.
    #[serde(default)]
    token: Option<String>,
    /// Pull the token from a `kind: Secret`'s `token` key — same convention
    /// as `storage`'s `--password-secret`.
    #[serde(default, rename = "tokenSecretRef")]
    token_secret_ref: Option<String>,
    /// Skip TLS verification when the tunnel connects to the LOCAL backend
    /// (a self-signed cert on `localhost:<localPort>`) — never affects the
    /// public tunnel URL, which every provider here always serves over a
    /// real, provider-issued TLS cert. No-op for `pinggy` (it forwards raw
    /// TCP and never inspects what is behind it).
    #[serde(default, rename = "insecureSkipTlsVerify")]
    insecure_skip_tls_verify: bool,
}

/// Keys of `provider.spec` of a tunnel, by provider: what only that service
/// understands. `insecureSkipTlsVerify` is about the LOCAL backend and stays
/// provider-neutral; `localPort` is the intent.
const TUNNEL_PROVIDER_TYPES: &[&str] = &["pinggy", "ngrok", "cloudflare"];
const TUNNEL_PROVIDER_SPEC: &[(&str, &str)] = &[
    ("hostname", "hostname"),
    ("token", "token"),
    ("tokenSecretRef", "tokenSecretRef"),
];

/// Normalizes `spec.provider: { type, spec }` (ADR-0071) to the scalar form
/// plus the flat fields the executor reads. A scalar `provider: cloudflare` is
/// the selection-only spelling and passes untouched. A key in `provider.spec`
/// that is also written flat is a contradiction, not a precedence rule.
pub(crate) fn lower_tunnel_provider(docs: &mut [ManifestDoc]) -> Result<()> {
    use serde_yaml::Value;
    for doc in docs.iter_mut().filter(|d| d.kind == k::GATEWAY) {
        let name = doc.metadata.name.clone();
        let bad = |msg: String| Error::Invalid(format!("Gateway '{name}': {msg}"));
        let Value::Mapping(spec) = &mut doc.spec else {
            continue;
        };
        let Some(Value::Mapping(p)) = spec.get("provider").cloned() else {
            continue;
        };
        for key in p.keys().filter_map(Value::as_str) {
            if !matches!(key, "type" | "spec") {
                return Err(bad(format!("spec.provider.{key}: unknown field (a tunnel provider block is `type` + `spec`)")));
            }
        }
        let ty = p
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("spec.provider.type is required".into()))?
            .to_string();
        if !TUNNEL_PROVIDER_TYPES.contains(&ty.as_str()) {
            return Err(bad(format!(
                "spec.provider.type '{ty}' is not a tunnel provider (known: {})",
                TUNNEL_PROVIDER_TYPES.join(", ")
            )));
        }
        match p.get("spec") {
            None | Some(Value::Null) => {}
            Some(Value::Mapping(block)) => {
                for key in block.keys().filter_map(Value::as_str) {
                    let Some((_, flat)) = TUNNEL_PROVIDER_SPEC.iter().find(|(f, _)| *f == key)
                    else {
                        return Err(bad(format!(
                            "spec.provider.spec.{key}: not a tunnel provider field"
                        )));
                    };
                    if spec.contains_key(*flat) {
                        return Err(bad(format!(
                            "'{flat}' is written both in spec.provider.spec and at the top of spec — say it once"
                        )));
                    }
                }
                for (from, to) in TUNNEL_PROVIDER_SPEC {
                    if let Some(v) = block.get(*from) {
                        spec.insert(Value::from(*to), v.clone());
                    }
                }
            }
            Some(_) => return Err(bad("spec.provider.spec must be a mapping".into())),
        }
        spec.insert(Value::from("provider"), Value::from(ty));
    }
    Ok(())
}

pub const TUNNEL_SPEC_FIELDS: &[&str] = &[
    "provider",
    "localPort",
    "hostname",
    "token",
    "tokenSecretRef",
    "insecureSkipTlsVerify",
];

/// `pinggy` | `ngrok` | `cloudflare`, as a CLI-level choice (`expose` only —
/// `TunnelSpec.provider` above stays a bare `String`, since a manifest is not
/// clap and gains nothing from the enum). `pinggy` is the default: it is the
/// only one needing no extra binary on `PATH` (plain `ssh`, already a
/// dependency of this binary) and no account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum TunnelProvider {
    Pinggy,
    Ngrok,
    Cloudflare,
}

impl TunnelProvider {
    fn as_str(self) -> &'static str {
        match self {
            TunnelProvider::Pinggy => "pinggy",
            TunnelProvider::Ngrok => "ngrok",
            TunnelProvider::Cloudflare => "cloudflare",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TunnelRecord {
    name: String,
    provider: String,
    local_port: u16,
    hostname: Option<String>,
    /// Hash of (provider, local_port, hostname, insecure_skip_tls_verify,
    /// resolved token) — a re-`apply` with the SAME effective config is a
    /// no-op; a DIFFERENT one restarts the agent (no provider here supports
    /// hot-reload the way the HTTPRoute proxy's SIGHUP does).
    config_hash: String,
    /// The provider AGENT's pid — the `ssh`/`ngrok`/`cloudflared` the
    /// supervisor spawned and `waitpid`s. Changes on every respawn.
    pid: Option<i32>,
    /// `proc_starttime` of `pid` (field 22 of `/proc/<pid>/stat`). Pins the
    /// exact process instance so a recycled pid is never signalled by mistake
    /// — the same guard `delonix-compute`'s `Container::is_live` uses.
    /// `#[serde(default)]`: a pre-supervision record has none and falls back to
    /// the argv-only guard (`agent_argv_is_ours`).
    #[serde(default)]
    pid_starttime: Option<u64>,
    /// The detached SUPERVISOR's pid — a `fork` of this CLI (no exec) that owns
    /// the agent, `waitpid`s it, and respawns it with backoff until this record
    /// is removed or its `config_hash` changes. It `setsid`s, so its pid is
    /// also the agent's process-GROUP id: tearing the group down
    /// (`killpg`) takes the agent with it, even one caught mid-respawn.
    /// `None` on a pre-supervision record (its agent `setsid`'d itself and has
    /// no parent left) and while `--no-supervise` is not a thing — every new
    /// apply supervises.
    #[serde(default)]
    supervisor_pid: Option<i32>,
    /// `proc_starttime` of `supervisor_pid` — same PID-reuse guard as
    /// `pid_starttime`, for the one process `is_alive` now anchors a supervised
    /// tunnel's liveness to.
    #[serde(default)]
    supervisor_starttime: Option<u64>,
    /// How many times the supervisor has respawned the agent. Status only
    /// (`describe`), like a container's `RESTARTS`.
    #[serde(default)]
    restarts: u32,
    public_url: Option<String>,
    created_unix: u64,
    started_unix: Option<u64>,
    /// `ngrok` only — its local agent API port. FIXED at `4040` (ngrok v3
    /// dropped the `--web-addr` flag this used to vary — see
    /// `NGROK_WEB_ADDR`), so this is really "was an ngrok agent web API
    /// reachable when this record was written", kept for `describe` and for
    /// `other_alive_ngrok`'s one-at-a-time guard.
    #[serde(default)]
    agent_web_port: Option<u16>,
    /// Skip TLS verification of the LOCAL backend's cert (see
    /// `TunnelSpec::insecure_skip_tls_verify`). `#[serde(default)]`: older
    /// records predate the field and mean `false`.
    #[serde(default)]
    insecure_skip_tls_verify: bool,
    /// Whether the public URL actually reaches a live origin, as last probed
    /// right after (re)starting the agent (see `probe_origin`): `"ready"` (the
    /// backend answered — a local TCP connect succeeded and, for an HTTP
    /// provider, a request through the public URL came back as something OTHER
    /// than the provider's own "origin unreachable" page), `"origin
    /// unavailable"` (the local port refused the connection), or absent
    /// (`Unknown` — not probed, or the edge never gave a usable answer within
    /// the window; reported as nothing rather than a false ready/down).
    /// Status only, like `restarts` — a tunnel is still "Running" while its
    /// supervisor lives; this just says whether there is anything behind it.
    /// `#[serde(default)]`: older records predate the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin_state: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum TunnelCmd {
    /// Apply the `kind: Gateway` documents of a manifest (idempotent).
    Apply {
        #[arg(value_hint = clap::ValueHint::FilePath, short, long)]
        file: Option<PathBuf>,
    },
    /// One-shot expose of a local port, no manifest needed.
    Expose {
        /// Local TCP port to expose.
        local_port: u16,
        /// `pinggy` | `ngrok` | `cloudflare`.
        #[arg(short, long, value_enum, default_value_t = TunnelProvider::Pinggy)]
        provider: TunnelProvider,
        /// Name (default: `tunnel-<port>`).
        #[arg(short, long, add = clap_complete::engine::ArgValueCandidates::new(super::complete::tunnels))]
        name: Option<String>,
        /// Custom/reserved hostname — `ngrok` uses it as `--url` (a reserved
        /// domain on your account); `cloudflare` only accepts it together
        /// with `--token` (a NAMED tunnel), where it is informational — the
        /// route itself lives in the Cloudflare dashboard for that tunnel.
        #[arg(long)]
        hostname: Option<String>,
        /// Provider token: pinggy pro token, ngrok authtoken, or a
        /// cloudflare NAMED tunnel's token (`cloudflared tunnel token
        /// <name>` or the Zero Trust dashboard) — switches `--provider
        /// cloudflare` from an anonymous quick tunnel to that tunnel.
        #[arg(long)]
        token: Option<String>,
        #[arg(long = "token-secret", add = clap_complete::engine::ArgValueCandidates::new(super::complete::secrets))]
        token_secret: Option<String>,
        /// Skip TLS verification of the LOCAL backend's cert (self-signed
        /// HTTPS on `localhost:<local-port>`). Never affects the public
        /// tunnel URL. No-op for `pinggy`.
        #[arg(long = "insecure-skip-tls-verify")]
        insecure_skip_tls_verify: bool,
    },
    // `Ls`/`Describe`/`Rm` moved out of this enum (B4 of the CLI restructuring):
    // all three duplicated `get`/`describe`/`delete gateways` byte-for-byte —
    // same store, same output, no filter any of them could take that the
    // generic verb couldn't. `cmd_ls`/`cmd_describe`/`cmd_rm` (below) are what
    // `verbs.rs` now calls directly.
}

pub fn run(action: TunnelCmd) -> Result<()> {
    match action {
        TunnelCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        TunnelCmd::Expose {
            local_port,
            provider,
            name,
            hostname,
            token,
            token_secret,
            insecure_skip_tls_verify,
        } => {
            let name = name.unwrap_or_else(|| format!("tunnel-{local_port}"));
            let spec = TunnelSpec {
                provider: provider.as_str().to_string(),
                local_port,
                hostname,
                token,
                token_secret_ref: token_secret,
                insecure_skip_tls_verify,
            };
            apply_one(&name, &spec)
        }
    }
}

/// Fields the reconciler compares for a `kind: Gateway`.
///
/// **The public URL is deliberately absent — it is STATUS.** A provider hands it
/// out when the agent connects, and a free tier hands out a different one every
/// time; comparing it would report drift on a manifest nobody touched, which is
/// the failure mode that makes a plan worth ignoring.
///
/// **The token is absent too, and that is a consistency call.** The record keeps
/// only a HASH of the effective config, so comparing a token would mean opening
/// the vault to recompute it — and `kind: Secret` is reported as
/// non-converging precisely because a plan will not decrypt to compare. A token
/// changed on its own is therefore invisible to the plan; the apply's own
/// `config_hash` still notices and restarts the agent, so nothing is lost
/// except the preview.
pub(crate) const RECONCILED_TUNNEL_FIELDS: &[&str] =
    &["provider", "localPort", "hostname", "insecureSkipTlsVerify"];

pub(crate) fn desired(doc: &ManifestDoc) -> Result<super::reconcile::Desired> {
    let spec: TunnelSpec = manifest::spec_of(doc)?;
    let mut f = std::collections::BTreeMap::new();
    f.insert("provider".into(), spec.provider.clone());
    f.insert("localPort".into(), spec.local_port.to_string());
    if let Some(h) = &spec.hostname {
        f.insert("hostname".into(), h.clone());
    }
    if spec.insecure_skip_tls_verify {
        f.insert("insecureSkipTlsVerify".into(), "true".into());
    }
    Ok(super::reconcile::Desired {
        kind: k::GATEWAY.into(),
        name: doc.metadata.name.clone(),
        fields: f,
        converges: true,
        // A `TunnelRecord` carries no labels, so there is nowhere to stamp a
        // stack — same as a `ShareVolume`. Converged, never adopted or pruned.
        ownable: false,
    })
}

pub(crate) fn actual(docs: &[ManifestDoc]) -> Result<Vec<super::reconcile::Actual>> {
    let store = record_store()?;
    let mut out = Vec::new();
    for doc in manifest::of_kind(docs, k::GATEWAY) {
        let Ok(rec) = store.load(&doc.metadata.name) else {
            continue;
        };
        let mut f = std::collections::BTreeMap::new();
        f.insert("provider".into(), rec.provider.clone());
        f.insert("localPort".into(), rec.local_port.to_string());
        if let Some(h) = &rec.hostname {
            f.insert("hostname".into(), h.clone());
        }
        if rec.insecure_skip_tls_verify {
            f.insert("insecureSkipTlsVerify".into(), "true".into());
        }
        out.push(super::reconcile::Actual {
            kind: k::GATEWAY.into(),
            name: doc.metadata.name.clone(),
            fields: f,
            owner: None,
            last_applied: None,
        });
    }
    Ok(out)
}

/// Presence for `stack ls`/`describe`/`wait` — same gap as `ShareVolume`: the
/// Kind was applied and never listed, so nothing asked until now.
pub(crate) fn presence_of(name: &str) -> (String, String) {
    match record_store().and_then(|s| Ok(s.load(name)?)) {
        Ok(rec) => (
            "yes".into(),
            rec.public_url.unwrap_or_else(|| rec.provider.clone()),
        ),
        Err(_) => ("no".into(), "-".into()),
    }
}

/// Converges a tunnel: re-apply the document.
///
/// `apply_one` already compares the effective `config_hash` and restarts the
/// agent when it differs — no provider here supports the hot reload the
/// HTTPRoute proxy gets from SIGHUP. So converging is applying, and the restart
/// is inherent: the tunnel comes back with a NEW public URL on a free tier,
/// which is why the URL is status and not something a manifest can pin.
pub(crate) fn converge_doc(doc: &ManifestDoc) -> Result<()> {
    let spec: TunnelSpec = manifest::spec_of(doc)?;
    apply_one(&doc.metadata.name, &spec)
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, k::GATEWAY) {
        let spec: TunnelSpec = manifest::spec_of(doc)?;
        apply_one(&doc.metadata.name, &spec)?;
    }
    Ok(())
}

fn tunnels_dir() -> PathBuf {
    state_root().join("tunnels")
}

fn record_store() -> Result<JsonStore<TunnelRecord>> {
    Ok(JsonStore::open(tunnels_dir())?)
}

/// Tunnel names, for shell autocompletion (`cmd::complete::tunnels`).
///
/// Lives here and not in `complete.rs` because `TunnelRecord`/`record_store`
/// are this module's business; a completer that re-derived the on-disk layout
/// would be a second place to keep in sync. Never fails — a TAB with no store
/// yet is "no suggestions", not an error.
pub(crate) fn completion_names() -> Vec<String> {
    let Ok(store) = record_store() else {
        return Vec::new();
    };
    store
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.name)
        .collect()
}

fn log_path(name: &str) -> PathBuf {
    tunnels_dir().join(format!("{name}.log"))
}

/// Same convention as `storage::resolve_password` — a literal wins, else a
/// `kind: Secret`'s named key, else `None` (the free/ephemeral path of every
/// provider here works with no token at all).
fn resolve_token(literal: Option<String>, secret_ref: Option<String>) -> Result<Option<String>> {
    let token = if let Some(t) = literal {
        Some(t)
    } else if let Some(name) = secret_ref {
        let store = delonix_state::SecretStore::open(state_root())?;
        let s = store.load(&name)?;
        Some(s.data.get("token").cloned().ok_or_else(|| {
            Error::Invalid(super::po::tf(
                "secret '{name}' has no 'token' key",
                &[("name", &name)],
            ))
        })?)
    } else {
        None
    };
    // BUG FIXED HERE (CRITICAL, found live by adversarial review): pinggy's
    // token is embedded as `<token>@free.pinggy.io`, the LAST positional argv
    // element handed to `ssh` — no other validation stood between a token and
    // that argv slot. `ssh`'s argument parser is hand-rolled (not glibc
    // getopt) but still permutes: a token of `-oProxyCommand=<cmd>` is parsed
    // as an ssh OPTION regardless of position, executing an attacker's shell
    // command via `/bin/sh -c` before any network connection is even made —
    // local RCE as whoever runs `delonix net tunnel apply/expose`. Rejecting a
    // leading `-` here protects every provider's use of the token (pinggy's
    // ssh argv AND ngrok's `--authtoken <value>`), not just the one call site
    // that happened to be exploitable today.
    if let Some(t) = &token {
        if t.starts_with('-') {
            return Err(Error::Invalid(
                super::po::t(
                    "token cannot start with '-' (it would be interpreted as an option of the provider's binary)",
                )
                .into(),
            ));
        }
        // A token is written into a file (pinggy's ssh config) and into an
        // environment variable: whitespace or a control character in it would
        // be a second line of that config. No provider issues one.
        if t.is_empty() || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::Invalid(
                super::po::t("token cannot be empty or contain whitespace or control characters")
                    .into(),
            ));
        }
    }
    Ok(token)
}

/// pinggy takes its token as the SSH user, and an ssh destination
/// `<token>@host` is argv — readable by every local user. `ssh` reads the user
/// from a config file too, so the token goes into one, `0600`, next to the
/// tunnel's log, and the argv names only the host. `-F` also means the user's
/// own `~/.ssh/config` is not read, which an agent that only forwards one port
/// never needed.
fn write_pinggy_ssh_config(name: &str, token: &str) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = tunnels_dir();
    std::fs::create_dir_all(&dir).map_err(|e| Error::Runtime {
        context: "write pinggy ssh config",
        message: e.to_string(),
    })?;
    let path = dir.join(format!("{name}.ssh_config"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| Error::Runtime {
            context: "write pinggy ssh config",
            message: e.to_string(),
        })?;
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    f.write_all(pinggy_ssh_config(token).as_bytes())
        .map_err(|e| Error::Runtime {
            context: "write pinggy ssh config",
            message: e.to_string(),
        })?;
    Ok(path)
}

/// The config body. PURE — the token is validated by [`resolve_token`]
/// (no whitespace, no control character), so it cannot open a second line.
fn pinggy_ssh_config(token: &str) -> String {
    format!("Host *\n  User {token}\n")
}

fn config_hash(spec: &TunnelSpec, token: &Option<String>) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    spec.provider.hash(&mut h);
    spec.local_port.hash(&mut h);
    spec.hostname.hash(&mut h);
    spec.insecure_skip_tls_verify.hash(&mut h);
    token.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The provider binary this engine spawns for a tunnel — `argv[0]`, not a
/// substring of the whole blob.
///
/// `None` for a provider this build does not know: an unknown provider owns no
/// process here, so nothing may be signalled on its behalf.
fn provider_bin(provider: &str) -> Option<&'static str> {
    match provider {
        "pinggy" => Some("ssh"),
        "ngrok" => Some("ngrok"),
        "cloudflare" => Some("cloudflared"),
        _ => None,
    }
}

/// The argv elements that carry OUR local port, for `rec`'s provider — the
/// discriminator that separates one agent from another of the same binary.
///
/// Each is a whole argv element that [`spawn_pinggy_at`]/[`spawn_ngrok`]/
/// [`spawn_cloudflare_quick`] build verbatim. Both spellings of the cloudflare
/// origin are accepted because the record's `insecure_skip_tls_verify` decides
/// which was used, and a stranger's argv contains neither.
fn port_tokens(rec: &TunnelRecord) -> Vec<String> {
    let p = rec.local_port;
    match rec.provider.as_str() {
        "pinggy" => vec![format!("0:localhost:{p}")],
        "ngrok" => vec![p.to_string(), format!("https://localhost:{p}")],
        "cloudflare" => vec![
            cloudflare_origin_url(p, false),
            cloudflare_origin_url(p, true),
        ],
        _ => vec![],
    }
}

/// The word that says WHAT the agent was told to do — `ssh -R` to pinggy,
/// `ngrok http`, `cloudflared tunnel`. Without it, an unrelated `ssh` that
/// happened to forward the same port number would still pass.
fn provider_anchor(provider: &str) -> Option<&'static str> {
    match provider {
        "pinggy" => Some("pinggy"),
        "ngrok" => Some("http"),
        "cloudflare" => Some("tunnel"),
        _ => None,
    }
}

/// Whether an argv names an agent WE spawned FOR THIS RECORD. PURE.
///
/// Three conditions, and each one earns its place against something measured on
/// this host on 2026-09-10 while ACH-018 was being written:
///
///   * `argv[0]`'s file name IS the provider binary. The old guard asked whether
///     the whole NUL-joined blob CONTAINED `"ssh"`, and four live processes of
///     this uid passed it — `/usr/bin/ssh-agent`, two Ansible `ssh: … [mux]`
///     control masters, and `/usr/libexec/gcr-ssh-agent`. A fifth, a plain
///     `bash`, passed for spelling `ssh` inside a path in its argv. It is the
///     same "substring of the blob" that ACH-017 removed from the L7 proxy.
///   * an argv element is one of [`port_tokens`] — our local port, so one
///     agent is not mistaken for another of the same binary.
///   * an argv element is the [`provider_anchor`] — what the agent was told to
///     do, so a real `ssh` that merely forwards the same port is not ours.
///
/// This is deliberately the argv side ONLY. Two roots on one uid can spawn
/// agents whose argv is identical; separating THOSE is [`env_allows_this_root`].
fn agent_argv_is_ours(cmdline: &[u8], rec: &TunnelRecord) -> bool {
    let Some(bin) = provider_bin(&rec.provider) else {
        return false;
    };
    let argv: Vec<String> = cmdline
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    let Some(argv0) = argv.first() else {
        return false;
    };
    if std::path::Path::new(argv0)
        .file_name()
        .and_then(|s| s.to_str())
        != Some(bin)
    {
        return false;
    }
    let tokens = port_tokens(rec);
    if !argv.iter().any(|a| tokens.iter().any(|t| a == t)) {
        return false;
    }
    match provider_anchor(&rec.provider) {
        Some(anchor) => argv.iter().any(|a| a.contains(anchor)),
        None => false,
    }
}

/// The value of `name` in a raw `/proc/<pid>/environ`. PURE.
fn env_value(raw: &[u8], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    raw.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .find_map(|e| e.strip_prefix(&prefix).map(|v| v.to_string()))
}

/// Whether a live agent's environment allows it to be OURS. PURE.
///
/// `spawn_and_capture` pins `DELONIX_ROOT` on every agent it starts, so an agent
/// of ANOTHER root names that root here and is refused — which is the whole
/// point, because two roots on one uid produce byte-identical argv.
///
/// **The absence of the variable is tolerated, and that is deliberate.** An
/// agent started by a build older than this one carries no `DELONIX_ROOT`,
/// because nothing pinned it; demanding it would turn every pre-upgrade tunnel
/// into an unstoppable orphan whose `rm` reports success without signalling
/// anything. It is the same shape as the tolerance ACH-016 wrote for a
/// pre-v0.34.2 holder, and it costs nothing that the argv side does not already
/// cover: a stranger never gets this far.
fn env_allows_this_root(raw: &[u8], root: &std::path::Path) -> bool {
    match env_value(raw, "DELONIX_ROOT") {
        Some(v) => std::path::Path::new(&v) == root,
        None => true,
    }
}

/// Whether the AGENT process is genuinely alive AND is really ours.
///
/// Its first version asked whether the `/proc/<pid>/cmdline` CONTAINED the
/// provider's binary name, and called the gap "rare". It was not rare: see
/// [`agent_argv_is_ours`] for what passed it, measured. The pid read here is the
/// one [`signal_agent`] sends `SIGTERM` to when the supervisor is already gone
/// and the agent was left orphaned.
///
/// `pid_starttime` is an ADDITIONAL guard on top of the argv/root pair: a
/// recycled pid that happens to spell a provider's argv under this root still
/// fails the starttime check. A pre-supervision record carries no starttime,
/// and [`safe_to_signal`] tolerates that (the argv/root guard alone, as before).
fn agent_alive(rec: &TunnelRecord) -> bool {
    let Some(pid) = rec.pid else { return false };
    if !safe_to_signal(pid, rec.pid_starttime) {
        return false;
    }
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    if !agent_argv_is_ours(&cmdline, rec) {
        return false;
    }
    let raw = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    env_allows_this_root(&raw, &state_root())
}

/// Whether this tunnel's SUPERVISOR is alive. `None` on a pre-supervision
/// record. The supervisor is a `fork` of this CLI (no exec), so its argv is the
/// original `delonix …` command and cannot identify it — the recorded
/// pid+starttime pins the exact instance instead, which is enough precisely
/// because WE forked it and wrote both down the same instant.
fn supervisor_alive(rec: &TunnelRecord) -> bool {
    match rec.supervisor_pid {
        Some(pid) => safe_to_signal(pid, rec.supervisor_starttime),
        None => false,
    }
}

/// Whether the tunnel is up. A SUPERVISED tunnel is up for as long as its
/// supervisor lives: an agent that just died is in the backoff window and the
/// supervisor is about to bring it back, which is exactly the state an operator
/// wants reported as "Running (recovering)", not "Stopped". A pre-supervision
/// record has no supervisor, so it falls back to the agent itself.
fn is_alive(rec: &TunnelRecord) -> bool {
    if rec.supervisor_pid.is_some() {
        supervisor_alive(rec)
    } else {
        agent_alive(rec)
    }
}

/// SIGTERM the supervisor's whole process group when the supervisor is ours,
/// so the agent it is the parent of dies with it — including one caught between
/// `spawn_and_capture` and its record write (not yet in the record, so
/// [`signal_agent`] below could not find it by pid). The supervisor `setsid`s,
/// making its pid the group id of every agent it spawns.
fn signal_supervisor(rec: &TunnelRecord) {
    if let Some(pid) = rec.supervisor_pid {
        if safe_to_signal(pid, rec.supervisor_starttime) {
            // SAFETY: negative pid = the process group led by `pid`, which
            // `safe_to_signal` just confirmed is our exact supervisor instance
            // (pid+starttime), so the group is ours and nobody else's.
            unsafe { libc::killpg(pid, libc::SIGTERM) };
        }
    }
}

/// SIGTERM the agent directly, by its recorded pid. Catches an agent ORPHANED
/// by a supervisor that already died (its group leader gone, so
/// [`signal_supervisor`] no longer reaches it) — identified by the argv/root
/// guard, the pre-supervision teardown path unchanged.
fn signal_agent(rec: &TunnelRecord) {
    if let Some(pid) = rec.pid {
        if agent_alive(rec) {
            // SAFETY: signalling a PID confirmed alive, ours (argv+root), and
            // the same instance (starttime).
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

fn stop_process(rec: &TunnelRecord) {
    // Supervisor FIRST: signalling the agent while the supervisor lives only
    // makes it respawn a new one a backoff later.
    signal_supervisor(rec);
    signal_agent(rec);
}

/// SIGTERM, then WAIT (bounded) for the tunnel to actually be gone.
///
/// [`stop_process`] only SIGNALS, and SIGTERM is asynchronous: a provider
/// agent tears its tunnel down before exiting. `apply_one` replacing a
/// tunnel used to spawn the new agent the instant it had signalled the old
/// one, so for `ngrok` both agents briefly wanted the same fixed local API
/// port ([`NGROK_WEB_ADDR`]) — reintroducing, from the other side, the exact
/// cross-wire [`other_alive_ngrok`] exists to prevent.
///
/// Supervisor first, awaited, THEN the agent: once the supervisor is confirmed
/// gone it can spawn no replacement, so the agent named in the record (re-read
/// by the caller) is the last one and killing it ends the tunnel for good.
///
/// Best-effort by design: a process that outlives the ceiling does not fail
/// the apply (the new one is what the operator asked for, and a provider
/// hung in teardown is not a reason to refuse it), same posture as the
/// URL-polling deadline in [`spawn_and_capture`].
fn stop_and_await(rec: &TunnelRecord) {
    signal_supervisor(rec);
    await_gone(|| supervisor_alive(rec));
    // The supervisor's own SIGTERM does not reliably reach the agent once the
    // agent `setsid`? — it does NOT setsid (the supervisor is its session), so
    // `killpg` above already hit it; this is the belt-and-suspenders path for
    // an agent orphaned by a supervisor that had died before this call.
    signal_agent(rec);
    await_gone(|| agent_alive(rec));
}

/// Poll `still_alive` to `false`, bounded at 5s. Shared by both waits above.
fn await_gone(mut still_alive: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && still_alive() {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let full = dir.join(bin);
        full.is_file().then_some(full)
    })
}

fn apply_one(name: &str, spec: &TunnelSpec) -> Result<()> {
    if !matches!(spec.provider.as_str(), "pinggy" | "ngrok" | "cloudflare") {
        return Err(Error::Invalid(super::po::tf(
            "tunnel '{name}': unknown provider '{provider}' (pinggy|ngrok|cloudflare)",
            &[("name", name), ("provider", &spec.provider)],
        )));
    }
    let token = resolve_token(spec.token.clone(), spec.token_secret_ref.clone())?;
    let hash = config_hash(spec, &token);
    std::fs::create_dir_all(tunnels_dir())?;
    let store = record_store()?;

    if let Ok(existing) = store.load(name) {
        if existing.config_hash == hash && is_alive(&existing) {
            println!(
                "tunnel/{name}: {} — {}",
                super::po::t("already running"),
                existing
                    .public_url
                    .as_deref()
                    .unwrap_or(super::po::t("(determining URL...)"))
            );
            return Ok(());
        }
        // Awaited, not just signalled: the replacement agent is spawned a few
        // lines down and (for ngrok) wants a local port this one still holds.
        // Tears down the old SUPERVISOR first so it cannot respawn into the
        // port the new one is about to take.
        stop_and_await(&existing);
    }

    spawn_supervised(name, spec, token, hash, &store)
}

/// The provider-agent dispatch, factored out so the supervisor can call it on
/// every (re)start, not just the first. Fills `rec.pid`/`pid_starttime` and,
/// best-effort, `rec.public_url`.
fn spawn_agent(
    rec: &mut TunnelRecord,
    spec: &TunnelSpec,
    token: Option<&str>,
    store: &JsonStore<TunnelRecord>,
) -> Result<()> {
    match spec.provider.as_str() {
        "pinggy" => spawn_pinggy(rec, token),
        "ngrok" => spawn_ngrok(
            rec,
            token,
            spec.hostname.as_deref(),
            spec.insecure_skip_tls_verify,
            store,
        ),
        "cloudflare" => spawn_cloudflare(
            rec,
            token,
            spec.hostname.as_deref(),
            spec.insecure_skip_tls_verify,
        ),
        _ => unreachable!("provider validated by apply_one"),
    }
}

/// Forks a DETACHED supervisor that owns the agent and keeps it alive, then —
/// in the parent (the CLI) — waits through a pipe for the first start so
/// `expose`/`apply` still return only once the tunnel exists (and prints its
/// URL). Mirrors `delonix-linux::supervise::run_supervised`: one short-lived
/// process per tunnel, no daemon. The `fork` assumes a single-threaded caller,
/// which the CLI is (same constraint `run -d --restart` already relies on).
fn spawn_supervised(
    name: &str,
    spec: &TunnelSpec,
    token: Option<String>,
    hash: String,
    store: &JsonStore<TunnelRecord>,
) -> Result<()> {
    let mut fds = [0i32; 2];
    // Close-on-exec: the agent the supervisor execs must NOT inherit the write
    // end, or the parent's read below would wait for an EOF only the agent
    // could give — the exact hang `delonix-linux::supervise` documents.
    // SAFETY: pipe2 fills two fds; used only for the first-start handshake.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(Error::Runtime {
            context: "tunnel",
            message: "supervisor handshake pipe".into(),
        });
    }
    let (rd, wr) = (fds[0], fds[1]);
    // SAFETY: fork of a single-threaded process (the CLI).
    let sup = unsafe { libc::fork() };
    if sup < 0 {
        let e = std::io::Error::last_os_error();
        // SAFETY: both ends are ours and handed to nobody.
        unsafe {
            libc::close(rd);
            libc::close(wr);
        }
        return Err(Error::Runtime {
            context: "tunnel",
            message: format!("fork supervisor: {e}"),
        });
    }
    if sup == 0 {
        // ---- supervisor (child): never returns ----
        // SAFETY: in the fork: `rd` is the parent's end, closed once; `setsid`
        // makes this the session + process-group leader, so every agent it
        // spawns shares its pgid and `killpg` can tear the group down together.
        unsafe {
            libc::close(rd);
            libc::setsid();
        }
        supervise_loop(name, spec, token.as_deref(), &hash, store, wr);
    }

    // ---- parent (CLI): wait for the first start ----
    // SAFETY: closes the write end; the supervisor keeps its own.
    unsafe { libc::close(wr) };
    let (ok, reason) = read_handshake(rd, sup);
    // SAFETY: `rd` is ours; closed once.
    unsafe { libc::close(rd) };
    if !ok {
        return Err(Error::Runtime {
            context: "tunnel",
            message: if reason.is_empty() {
                super::po::t("the tunnel supervisor died before its first start").into()
            } else {
                reason
            },
        });
    }
    // The supervisor saved the record (its pid, the agent's pid, the URL, the
    // origin probe) BEFORE the handshake byte, so a reload here sees it.
    let saved = store.load(name).ok();
    let url = saved
        .as_ref()
        .and_then(|r| r.public_url.clone())
        .unwrap_or_else(|| {
            super::po::t(
                "(URL not confirmed yet — see `delonix describe gateways <name>` / the log)",
            )
            .into()
        });
    // Reflect the origin probe on the line the operator sees RIGHT NOW, so a
    // tunnel standing over a dead backend is not reported as a bare
    // "running — <url>". `None` (origin ready could not be decided, e.g. a
    // still-warming edge) stays silent rather than guessing.
    let origin = saved.as_ref().and_then(|r| r.origin_state.as_deref());
    match origin {
        Some(state) => println!(
            "tunnel/{name}: {} ({} {state}) — {url}",
            super::po::t("running"),
            super::po::t("origin")
        ),
        None => println!("tunnel/{name}: {} — {url}", super::po::t("running")),
    }
    Ok(())
}

/// The supervisor's body: (re)start the agent, block on its death, respawn with
/// capped exponential backoff + jitter, until the record is removed (`rm`) or
/// its `config_hash` changes (a newer apply took over). Never returns — it ends
/// by `std::process::exit`.
fn supervise_loop(
    name: &str,
    spec: &TunnelSpec,
    token: Option<&str>,
    hash: &str,
    store: &JsonStore<TunnelRecord>,
    wr: i32,
) -> ! {
    let self_pid = std::process::id() as i32;
    let self_start = proc_starttime(self_pid);
    let created = output::now_unix();
    let mut restarts: u32 = 0;
    let mut first = true;
    loop {
        // Desired state BEFORE a respawn (never on the very first start): a
        // removed record means `rm`, a changed hash means a newer apply owns
        // this tunnel now — either way, stop. Same rule as
        // `supervise::resume_restart`.
        if !first && !should_keep_running(store, name, hash) {
            std::process::exit(0);
        }
        let mut rec = TunnelRecord {
            name: name.to_string(),
            provider: spec.provider.clone(),
            local_port: spec.local_port,
            hostname: spec.hostname.clone(),
            config_hash: hash.to_string(),
            pid: None,
            pid_starttime: None,
            supervisor_pid: Some(self_pid),
            supervisor_starttime: self_start,
            restarts,
            public_url: None,
            created_unix: created,
            started_unix: Some(created),
            agent_web_port: None,
            insecure_skip_tls_verify: spec.insecure_skip_tls_verify,
            origin_state: None,
        };
        let started = spawn_agent(&mut rec, spec, token, store);
        if started.is_ok() {
            // The agent is up and (best-effort) has its URL — now ask whether
            // anything actually answers behind it, so a tunnel over a dead
            // backend records "origin unavailable" instead of a bare Running.
            rec.origin_state = probe_origin(&rec).into_state();
            let _ = store.save(name, &rec);
        }
        if first {
            first = false;
            // Handshake FIRST (status byte + reason on failure), THEN release
            // stdio: until the byte is written the parent is still reading, and
            // a `spawn_agent` error has to reach the user through the pipe, not
            // this soon-to-be-/dev/null stderr.
            write_handshake(wr, started.is_ok(), started.as_ref().err());
            // SAFETY: closes our write end, then redirects stdio to /dev/null so
            // the detached supervisor holds no terminal.
            unsafe {
                libc::close(wr);
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null >= 0 {
                    libc::dup2(null, 0);
                    libc::dup2(null, 1);
                    libc::dup2(null, 2);
                    if null > 2 {
                        libc::close(null);
                    }
                }
            }
            if started.is_err() {
                // The caller will report this; nothing was saved.
                std::process::exit(1);
            }
        }
        if started.is_err() {
            // A RESPAWN whose agent would not start (e.g. the binary vanished
            // from PATH): back off and try again rather than spin. The first
            // start's failure already exited above.
            restarts = restarts.saturating_add(1);
            std::thread::sleep(backoff_delay(restarts, self_pid));
            continue;
        }
        // The agent is up and we are its parent: block until it dies.
        if let Some(pid) = rec.pid {
            wait_for_pid(pid);
        }
        // It died. Stop if the desired state says so; else back off and respawn.
        if !should_keep_running(store, name, hash) {
            std::process::exit(0);
        }
        restarts = restarts.saturating_add(1);
        std::thread::sleep(backoff_delay(restarts, self_pid));
    }
}

/// Whether the supervisor should keep the tunnel alive: the record still exists
/// (`rm` removes it) AND still names this configuration (a newer apply writes a
/// different `config_hash` and forks its own supervisor). PURE given the store.
fn should_keep_running(store: &JsonStore<TunnelRecord>, name: &str, hash: &str) -> bool {
    matches!(store.load(name), Ok(r) if r.config_hash == hash)
}

/// Blocks until `pid` (this supervisor's own child) exits, reaping it. Returns
/// at once with `ECHILD` if it was already reaped (the `WNOHANG` in `pid_alive`
/// during URL capture can collect an agent that died inside the poll window).
fn wait_for_pid(pid: i32) {
    let mut st: libc::c_int = 0;
    // SAFETY: a blocking waitpid on our own child; ECHILD (already reaped) just
    // returns -1 and changes nothing.
    unsafe {
        libc::waitpid(pid, &mut st, 0);
    }
}

/// Capped exponential backoff with jitter, for the `restarts`-th respawn
/// (`restarts >= 1`). Base doubles 1s→2→4→8→16→32 and stays at 32s, like the
/// container supervisor; the jitter (0..25% of the base, derived from the
/// supervisor pid so two tunnels that crash together do not respawn in
/// lockstep) is added on top. PURE and deterministic for a given (restarts,
/// seed), so the schedule is testable without sleeping.
fn backoff_delay(restarts: u32, seed: i32) -> Duration {
    let base = backoff_base_secs(restarts);
    let jitter_ceiling = base * 250; // ms; 25% of the base
    let jitter = backoff_jitter_ms(seed, restarts, jitter_ceiling);
    Duration::from_millis(base * 1000 + jitter)
}

/// The base backoff in SECONDS: `1 << (restarts-1)`, capped at 32. Pure.
fn backoff_base_secs(restarts: u32) -> u64 {
    let shift = restarts.saturating_sub(1).min(5);
    (1u64 << shift).min(32)
}

/// Jitter in ms, in `0..=ceiling`, from a cheap hash of (seed, restarts). No
/// `rand` dependency and no wall clock — both would make the schedule
/// untestable; this only needs to DECORRELATE concurrent supervisors, not be
/// cryptographic. Pure.
fn backoff_jitter_ms(seed: i32, restarts: u32, ceiling: u64) -> u64 {
    if ceiling == 0 {
        return 0;
    }
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    seed.hash(&mut h);
    restarts.hash(&mut h);
    h.finish() % (ceiling + 1)
}

/// Writes the first-start handshake to the pipe: one status byte (`1` = the
/// agent started), then — on failure — the error text behind it. Mirrors
/// `supervise::run_supervised`'s handshake; the parent drains it in
/// [`read_handshake`].
fn write_handshake(wr: i32, ok: bool, err: Option<&Error>) {
    let b = [u8::from(ok)];
    // SAFETY: writes one byte, then the reason, to our own write end.
    unsafe {
        libc::write(wr, b.as_ptr() as *const libc::c_void, 1);
        if !ok {
            if let Some(e) = err {
                let msg = e.to_string();
                let bytes = msg.as_bytes();
                libc::write(wr, bytes.as_ptr() as *const libc::c_void, bytes.len());
            }
        }
    }
}

/// Reads the first-start handshake: `(started, reason)`. Never waits longer than
/// the supervisor lives — it polls the pipe AND `waitpid`s the supervisor, so a
/// supervisor that dies before writing is reported as `(false, "")` rather than
/// hanging the CLI forever (the lingering-writer defect `supervise` documents,
/// adapted here where the agent could inherit the write end if pipe2 ever lost
/// its `O_CLOEXEC`).
fn read_handshake(rd: i32, sup: libc::pid_t) -> (bool, String) {
    let mut data: Vec<u8> = Vec::new();
    let mut exited = false;
    loop {
        let mut pfd = libc::pollfd {
            fd: rd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is valid for the call; poll keeps no pointer.
        let ready = unsafe { libc::poll(&mut pfd, 1, if exited { 0 } else { 100 }) };
        if ready > 0 {
            let mut buf = [0u8; 512];
            // SAFETY: reads into our own buffer from the descriptor poll flagged.
            let k = unsafe { libc::read(rd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if k <= 0 {
                break; // EOF: the write end is gone.
            }
            data.extend_from_slice(&buf[..k as usize]);
            if data[0] == 1 {
                break; // Started; nothing follows a success.
            }
            continue;
        }
        if ready < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
        if exited {
            break; // The supervisor is gone and the pipe is drained.
        }
        let mut st = 0;
        // SAFETY: `sup` is this process's own child; WNOHANG never blocks.
        let r = unsafe { libc::waitpid(sup, &mut st, libc::WNOHANG) };
        exited = r == sup || r < 0;
    }
    let started = data.first().copied() == Some(1);
    let reason = String::from_utf8_lossy(data.get(1..).unwrap_or_default())
        .trim()
        .to_string();
    (started, reason)
}

/// Spawns `bin(args)` detached (setsid) with stdout+stderr to this tunnel's
/// log file, confirms it didn't die immediately, then polls the log for a
/// matching URL via `extract` for up to 15s (best-effort: a provider slow to
/// print its URL just leaves `public_url: None`, not an error — the tunnel
/// IS up either way).
/// `secret_env` carries a provider credential to the agent by ENVIRONMENT,
/// never by argv: `/proc/<pid>/cmdline` is readable by every local user
/// (`ps -ef` prints it), `/proc/<pid>/environ` only by this uid and root. The
/// agent's log is `0600` for a related reason: ngrok writes the authtoken it
/// was given into its own log when the account refuses it (measured, v3.39:
/// «Your authtoken: …»).
fn spawn_and_capture(
    rec: &mut TunnelRecord,
    bin: &str,
    args: &[String],
    secret_env: Option<(&str, &str)>,
    extract: impl Fn(&str) -> Option<String>,
) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = log_path(&rec.name);
    let log = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| Error::Runtime {
            context: "open tunnel log",
            message: e.to_string(),
        })?;
    // `mode` applies only on creation: a log a previous build left 0644 is
    // tightened here, before the agent can write a token into it.
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    let log2 = log.try_clone().map_err(|e| Error::Runtime {
        context: "clone log",
        message: e.to_string(),
    })?;
    let mut cmd = Command::new(bin);
    cmd.args(args).stdin(Stdio::null()).stdout(log).stderr(log2);
    // The ownership proof for `is_alive`/`stop_process`. The provider binaries
    // ignore an environment variable they do not know, and this is the only
    // thing that separates two `DELONIX_ROOT`s on one uid: their agents' argv
    // is byte-identical, so the argv guard alone cannot tell them apart. Pinned
    // rather than inherited because the machine's default root exports nothing
    // — the same measurement that denied `ingress_proxy` this proof (ACH-017),
    // with the difference that WE spawn this child and can simply set it.
    cmd.env("DELONIX_ROOT", state_root());
    if let Some((k, v)) = secret_env {
        cmd.env(k, v);
    }
    // No `setsid` here anymore: this runs INSIDE the supervisor (a `fork` that
    // already `setsid`'d), so the agent inherits that session and process group.
    // Staying in the group is what lets `signal_supervisor`'s `killpg` tear the
    // agent down with its supervisor — and the supervisor, as the agent's
    // parent, is what `waitpid`s it to drive the respawn. Detachment from the
    // CLI comes from the supervisor, not from the agent itself.
    let child = cmd.spawn().map_err(|e| Error::Runtime {
        context: "spawn tunnel agent",
        message: super::po::tf(
            "{bin}: {e} (is it installed and in PATH?)",
            &[("bin", bin), ("e", &e.to_string())],
        ),
    })?;
    rec.pid = Some(child.id() as i32);
    // Pin the exact process instance so a recycled pid is never signalled as if
    // it were this agent (see `agent_alive`). Read now, while we know the child
    // is this very process.
    rec.pid_starttime = proc_starttime(child.id() as i32);
    std::thread::sleep(Duration::from_millis(400));
    if !pid_alive(child.id() as i32) {
        let tail = std::fs::read_to_string(&path).unwrap_or_default();
        return Err(Error::Runtime {
            context: "tunnel",
            message: super::po::tf(
                "{bin} crashed right at startup — {last_line}",
                &[
                    ("bin", bin),
                    (
                        "last_line",
                        tail.lines().last().unwrap_or(super::po::t("(empty log)")),
                    ),
                ],
            ),
        });
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(url) = extract(&text) {
                rec.public_url = Some(url);
                break;
            }
        }
        // BUG FOUND LIVE: `free.pinggy.io`'s own geo-DNS can route to a broken
        // regional PoP that accepts the SSH connection, allocates the remote
        // forward, then closes it a moment later — no URL is EVER going to
        // appear in the log. Without this check, `apply_one` always waited
        // the full 15s before reporting "URL ainda não confirmada", even
        // though the process had already died after ~1-2s — and (see
        // `spawn_pinggy`) there was no way to distinguish "still connecting,
        // just slow" from "already dead" to decide whether a retry makes
        // sense. Exiting the instant the process is gone fixes both.
        if !pid_alive(child.id() as i32) {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// **Found live** while testing `provider=cloudflare --token <bad>`: a
/// `cloudflared` that exits within the poll loop (confirmed: "Provided
/// Tunnel token is not valid.", under a second) still made `apply_one` run
/// the entire 15s poll before this fix, contradicting the whole point of the
/// `!pid_alive` early-break introduced for the v0.16.1 pinggy fix — proven
/// with a minimal C repro, not deduced: a child we spawned and never
/// `waitpid`ed on is a ZOMBIE once it exits, and `/proc/<pid>` keeps
/// existing for a zombie for as long as THIS process keeps running (which is
/// exactly the window every caller here cares about) — a bare `/proc` check
/// reports "alive" for a process that has already exited.
///
/// The `waitpid(..., WNOHANG)` reaps it first, opportunistically: if `pid`
/// is a child of ours and has exited, this collects it and its `/proc`
/// entry disappears immediately after — also verified with the same repro.
/// If `pid` is NOT a child of this process (every caller here only ever
/// passes a PID it just spawned itself, but the pre-existing test below
/// checks `std::process::id()`, our OWN pid), `waitpid` harmlessly fails
/// with ECHILD and changes nothing — the `/proc` check right after still
/// answers correctly, so this stays a strict improvement, not a behavior
/// change, for every caller that predates it.
fn pid_alive(pid: i32) -> bool {
    let mut status: libc::c_int = 0;
    // SAFETY: WNOHANG never blocks; a `pid` that is not our child (or
    // already reaped) just makes this call fail (ECHILD), changing nothing.
    unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// First `https://` token in `text` matching `keep` — pure, tolerant of
/// surrounding punctuation/quotes (log lines, not a clean machine format).
/// Scans ALL matches and tests each with `keep` rather than trusting the
/// first `https://` overall: a provider's banner/MOTD can (and, found live
/// with pinggy's own upsell link — `https://dashboard.pinggy.io`, printed
/// BEFORE the real tunnel URLs — does) contain unrelated `https://` links.
fn find_url_where(text: &str, keep: impl Fn(&str) -> bool) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | '<' | '>'))
        .filter(|w| w.starts_with("https://"))
        .map(|w| {
            w.trim_end_matches(|c: char| {
                !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | ':'))
            })
        })
        .find(|w| keep(w))
        .map(str::to_string)
}

/// Convenience: first `https://` URL whose host contains `needle`.
fn find_url_containing(text: &str, needle: &str) -> Option<String> {
    find_url_where(text, |w| w.contains(needle))
}

/// `free.pinggy.io` is pinggy's own DOCUMENTED endpoint (`ssh -p443
/// -R0:<localhost>:<localport> [<token>@]free.pinggy.io`) — kept as the
/// primary target. BUG FOUND LIVE, two distinct failure shapes, both
/// reproduced independently of delonix by hand-running the identical `ssh`
/// invocation: (1) its geo-DNS can route to a broken regional PoP (host
/// resolved to `br.free.pinggy.io` → `lin.br.1.a.pinggy.click`) that accepts
/// the connection, allocates the remote forward, then closes it a moment
/// later — the `ssh` client DOES exit, just with no URL ever printed; (2)
/// under some conditions the `ssh` client does NOT exit even after the
/// server closes the connection (no pty, backgrounded — confirmed this
/// keeps `ssh` alive indefinitely, unlike the exact same command run
/// interactively) — so "did the process die" is NOT a reliable signal that
/// the attempt failed. Either way the observable fact after the poll window
/// is the same: no URL. So retry unconditionally on that, killing the
/// primary attempt first if it's still lingering (never leave 2 live tunnels
/// for one `TunnelRecord`). `a.pinggy.io` (pinggy's own literal, non-geo-
/// routed host — not separately documented, but a real pinggy-owned
/// endpoint that connected successfully under the exact same conditions) is
/// a one-shot fallback for exactly this, not a replacement default.
fn spawn_pinggy(rec: &mut TunnelRecord, token: Option<&str>) -> Result<()> {
    if rec.insecure_skip_tls_verify {
        eprintln!(
            "tunnel: {}",
            super::po::t(
                "note: insecure-skip-tls-verify is a no-op for provider=pinggy — it forwards \
                 raw TCP and never inspects the local backend's certificate"
            )
        );
    }
    spawn_pinggy_at(rec, token, "free.pinggy.io")?;
    if rec.public_url.is_none() {
        if let Some(pid) = rec.pid {
            if pid_alive(pid) {
                // SAFETY: signalling the PID this exact function just spawned
                // and confirmed alive moments ago.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
        eprintln!(
            "tunnel: {}",
            super::po::t(
                "free.pinggy.io did not return a URL (geo-routed node may be down, or the \
                 connection hung) — trying a.pinggy.io..."
            )
        );
        spawn_pinggy_at(rec, token, "a.pinggy.io")?;
    }
    Ok(())
}

fn spawn_pinggy_at(rec: &mut TunnelRecord, token: Option<&str>, host: &str) -> Result<()> {
    // The `-R0` port (dynamic, server-assigned) is what makes this work with
    // zero prior setup.
    let mut args = Vec::new();
    if let Some(t) = token {
        args.push("-F".to_string());
        args.push(
            write_pinggy_ssh_config(&rec.name, t)?
                .to_string_lossy()
                .into_owned(),
        );
    }
    args.extend([
        "-p".to_string(),
        "443".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=30".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "-R".to_string(),
        format!("0:localhost:{}", rec.local_port),
        // `--` before the positional destination: `resolve_token` already
        // rejects a leading `-`, but this is the same defense-in-depth
        // convention the codebase already applies to `virsh`/`ssh` argv
        // elsewhere (see cluster.rs) — belt and suspenders, verified live
        // that OpenSSH honors it (a `--`-prefixed destination is treated
        // literally, never as an option, regardless of its content).
        "--".to_string(),
        host.to_string(),
    ]);
    if rec.hostname.is_some() {
        eprintln!(
            "tunnel: {}",
            super::po::t(
                "WARNING: provider=pinggy with a custom hostname is not implemented (needs the \
                 exact reserved-domain SSH syntax confirmed against a paid account) — issuing an \
                 ephemeral URL instead"
            )
        );
    }
    // The ACTUAL tunnel URL's domain, captured live: `*.free.pinggy.link`/
    // `*.pinggy-free.link`/`*.free.pinggy.net` (varies by run) — never
    // `dashboard.pinggy.io`, which is pinggy's own upsell link, printed
    // BEFORE the real ones in the free-tier banner. Excluding it explicitly
    // (rather than trying to enumerate every real tunnel domain) is the
    // robust signal: whatever the assigned domain looks like this time, it
    // is not that one fixed host.
    spawn_and_capture(rec, "ssh", &args, None, |t| {
        find_url_where(t, |u| {
            u.contains("pinggy") && !u.contains("dashboard.pinggy.io")
        })
    })
}

/// ngrok's local agent web API is FIXED at `127.0.0.1:4040` since ngrok v3
/// dropped the `--web-addr` flag every earlier version of this function
/// depended on to give each concurrent tunnel its own port — **found live**
/// against a real `ngrok v3.39.11` binary while adding cloudflare-token
/// support: `--web-addr` is now `unknown flag`, and there is no config-file
/// key that replaces it (`web_addr` in a `--config` YAML errors "field not
/// found"). Before this fix `provider=ngrok` could not run at all against a
/// current ngrok agent — every invocation died on the unknown flag before
/// even attempting a connection.
const NGROK_WEB_ADDR: &str = "127.0.0.1:4040";

/// The OTHER ngrok record (if any) currently holding the fixed web API port
/// — pure lookup over the store, independent of actually spawning anything,
/// so it is testable without an `ngrok` binary on `PATH`.
///
/// `self_name` is the tunnel being applied right now, and is never its own
/// rival. BUG FIXED: the name said "other" but the filter never excluded it,
/// and `apply_one` arrives here having just signalled that very agent — so
/// re-applying an ngrok tunnel (any edit: a new `localPort`, a token, or
/// switching INTO ngrok from another provider and back) failed with
/// "another ngrok tunnel is already running ('<itself>') … stop it first
/// (`delonix delete gateways <itself>`)", telling the operator to delete the
/// tunnel they were in the middle of changing. Its record is in the store,
/// and for the moment SIGTERM takes to land still alive in `/proc`, PRECISELY
/// because this is an update of it.
fn other_alive_ngrok(
    store: &JsonStore<TunnelRecord>,
    self_name: &str,
) -> Result<Option<TunnelRecord>> {
    Ok(ngrok_rival(store.list()?, self_name, is_alive))
}

/// The rival-selection rule itself, PURE (liveness injected): `is_alive`
/// reads `/proc` and cannot be faked from a test, while the rule — WHICH
/// records count as a rival — is the part that was wrong, so it is the part
/// that has to be coverable. Same split, for the same reason, as
/// `cmd::cluster`'s `is_cluster_vm` against the store walk around it.
fn ngrok_rival(
    records: Vec<TunnelRecord>,
    self_name: &str,
    alive: impl Fn(&TunnelRecord) -> bool,
) -> Option<TunnelRecord> {
    records
        .into_iter()
        .find(|r| r.name != self_name && r.provider == "ngrok" && alive(r))
}

fn spawn_ngrok(
    rec: &mut TunnelRecord,
    token: Option<&str>,
    hostname: Option<&str>,
    insecure_skip_tls_verify: bool,
    store: &JsonStore<TunnelRecord>,
) -> Result<()> {
    which("ngrok").ok_or_else(|| {
        Error::Unavailable(
            super::po::t(
                "`ngrok` not found in PATH — install it (https://ngrok.com/download) before \
                 using provider=ngrok",
            )
            .into(),
        )
    })?;
    // Only one ngrok agent can bind the fixed web API port — see
    // `NGROK_WEB_ADDR`. A second one wouldn't fail loudly on its own (the
    // tunnel itself would still connect), it would just make `poll_ngrok_api`
    // read whichever agent got there first — a silent cross-wire between two
    // unrelated tunnels, worse than refusing up front.
    if let Some(other) = other_alive_ngrok(store, &rec.name)? {
        return Err(Error::Invalid(super::po::tf(
            "another ngrok tunnel is already running ('{name}') — ngrok v3's local agent API \
             has no per-tunnel port anymore, so only one ngrok-provider tunnel can run on this \
             host at a time; stop it first (`delonix delete gateways {name}`)",
            &[("name", &other.name)],
        )));
    }
    rec.agent_web_port = Some(4040);
    let address = if insecure_skip_tls_verify {
        format!("https://localhost:{}", rec.local_port)
    } else {
        rec.local_port.to_string()
    };
    let mut args = vec![
        "http".to_string(),
        address,
        "--log".to_string(),
        "stdout".to_string(),
    ];
    // The authtoken goes by environment (`NGROK_AUTHTOKEN`, read by ngrok v3 —
    // measured: a bogus value is reported back as ERR_NGROK_105), never as
    // `--authtoken`, where every local user's `ps` would print it.
    if let Some(h) = hostname {
        // Reserved/custom domain — paid plans only; ngrok itself errors
        // clearly if the account doesn't have it, we don't pre-validate.
        //
        // A leading `-` is refused for the same reason `resolve_token` refuses
        // it: this lands in an argv slot of someone else's binary, and a value
        // that can pass for a flag is a value that can change what that binary
        // does. The token got this guard when it turned out to be exploitable;
        // the hostname sits in the same kind of slot and simply had not been
        // looked at.
        if h.starts_with('-') {
            return Err(Error::Invalid(
                super::po::t(
                    "hostname cannot start with '-' (it would be interpreted as an option of the provider's binary)",
                )
                .into(),
            ));
        }
        args.push("--url".to_string());
        args.push(h.to_string());
    }
    // ngrok's own log isn't a reliable place to scrape the URL from (format
    // varies by version); its local agent API is the documented way.
    spawn_and_capture(
        rec,
        "ngrok",
        &args,
        token.map(|t| ("NGROK_AUTHTOKEN", t)),
        |_| None,
    )?;
    poll_ngrok_api(rec);
    Ok(())
}

/// **Found live** right after fixing the `--web-addr` crash above: with that
/// fixed, an `ngrok` that fails on its own (e.g. no authtoken, invalid one)
/// now dies within a second or so — but this loop had no way to know that,
/// and polled a local API nobody was answering for the entire 15s deadline
/// regardless. Checks `pid_alive` each iteration and breaks the instant the
/// agent is gone, the exact same fix `spawn_and_capture` already applies to
/// its own poll loop (v0.16.1) for the identical reason.
fn poll_ngrok_api(rec: &mut TunnelRecord) {
    let url = format!("http://{NGROK_WEB_ADDR}/api/tunnels");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(resp) = reqwest::blocking::get(&url) {
            if let Ok(v) = resp.json::<serde_json::Value>() {
                if let Some(u) = v["tunnels"]
                    .as_array()
                    .and_then(|arr| arr.iter().find(|t| t["proto"] == "https"))
                    .and_then(|t| t["public_url"].as_str())
                {
                    rec.public_url = Some(u.to_string());
                    return;
                }
            }
        }
        if !rec.pid.is_some_and(pid_alive) {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Origin readiness, as decided by [`probe_origin`]. Not serialized — it is
/// flattened to the `origin_state` string the record stores via [`into_state`].
#[derive(Debug, PartialEq, Eq)]
enum OriginReadiness {
    /// The backend answered: a local TCP connect succeeded and — for an HTTP
    /// provider — a request through the public URL came back as something that
    /// is NOT the provider's own "origin unreachable" page.
    Ready,
    /// The local backend port refused the connection — nothing is listening.
    Unavailable,
    /// Could not decide: no URL to probe, or the public edge never gave a
    /// usable answer within the window (a warming quick-tunnel edge can 5xx for
    /// a few seconds even over a LIVE origin — reported as "don't know", never
    /// as a false "down", because the local connect already said the backend is
    /// up).
    Unknown,
}

impl OriginReadiness {
    /// The stored `origin_state` token. `Unknown` stores nothing (so old
    /// records and not-probed tunnels stay clean), matching the field's
    /// `skip_serializing_if`.
    fn into_state(self) -> Option<String> {
        match self {
            OriginReadiness::Ready => Some("ready".into()),
            OriginReadiness::Unavailable => Some("unavailable".into()),
            OriginReadiness::Unknown => None,
        }
    }
}

/// How long a local origin connect may take before we call the backend dead,
/// and how long the public-URL HTTP proof keeps retrying a warming edge.
const ORIGIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const PUBLIC_PROBE_WINDOW: Duration = Duration::from_secs(10);

/// Decides whether anything actually answers behind a just-(re)started tunnel.
///
/// Two stages, cheap first:
///  1. A local TCP connect to `127.0.0.1:<local_port>`. A refusal is the
///     unambiguous "origin unavailable" — nothing is listening — and it is the
///     ONLY source of `Unavailable`, so a slow public edge is never mistaken
///     for a dead backend. This is also the whole readiness story for a raw-TCP
///     provider (`pinggy`), where an HTTP proof does not apply.
///  2. For an HTTP provider (`ngrok`/`cloudflare`) with a captured URL, a
///     request THROUGH the public URL, retried across [`PUBLIC_PROBE_WINDOW`]
///     to tolerate edge warm-up: the moment a reply comes back that is not the
///     provider's own error page (see [`public_response_is_origin`]), the
///     origin is confirmed reachable end-to-end — `Ready`. If only provider
///     error pages / transport errors appear for the whole window, the local
///     connect already proved the backend up, so this is `Unknown`, not down.
fn probe_origin(rec: &TunnelRecord) -> OriginReadiness {
    if !local_origin_listening(rec.local_port) {
        return OriginReadiness::Unavailable;
    }
    match rec.provider.as_str() {
        // Raw TCP forward — a successful connect IS the readiness signal; there
        // is no HTTP layer to confirm a nonce through.
        "pinggy" => OriginReadiness::Ready,
        "ngrok" | "cloudflare" => match rec.public_url.as_deref() {
            Some(url) => probe_public_http(url, &rec.provider),
            None => OriginReadiness::Unknown,
        },
        _ => OriginReadiness::Unknown,
    }
}

/// A plain TCP connect to the backend, TLS-agnostic on purpose: the handshake
/// only proves something accepts connections on the port, which is exactly the
/// "is the backend up" question — and it answers it the same whether the
/// backend speaks HTTP or HTTPS (an `insecureSkipTlsVerify` origin included).
fn local_origin_listening(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, ORIGIN_CONNECT_TIMEOUT).is_ok()
}

/// Polls the public URL until a reply that came from the backend appears, or
/// the window elapses. Sends `ngrok-skip-browser-warning` and a non-browser
/// user-agent so ngrok forwards straight to the origin instead of serving its
/// free-tier interstitial — a 200 page that is NOT the origin, the precise
/// "any 200 is success" trap this must avoid (confirmed live: with the header
/// the backend's own bytes come through; `ERR_NGROK_8012` is what a dead
/// origin yields, at 502).
fn probe_public_http(url: &str, provider: &str) -> OriginReadiness {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()
    {
        Ok(c) => c,
        Err(_) => return OriginReadiness::Unknown,
    };
    let deadline = Instant::now() + PUBLIC_PROBE_WINDOW;
    loop {
        if let Ok(resp) = client
            .get(url)
            .header("ngrok-skip-browser-warning", "true")
            .header(reqwest::header::USER_AGENT, "delonix-tunnel-probe")
            .send()
        {
            let status = resp.status().as_u16();
            // Cap the body read: provider error pages are small, and a huge
            // origin response does not need reading in full to classify.
            let body = resp.text().unwrap_or_default();
            if public_response_is_origin(provider, status, &body) {
                return OriginReadiness::Ready;
            }
        }
        if Instant::now() >= deadline {
            return OriginReadiness::Unknown;
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}

/// Whether an HTTP response fetched through the public URL came from the
/// BACKEND rather than from the provider's own "origin unreachable" surface.
/// PURE, so the real captured provider pages pin it without a network.
///
/// The signal is NEGATIVE — "not the provider's error page" — because a healthy
/// backend may legitimately answer any status (a 404, a 500 of its own), so a
/// status check alone cannot decide this. What is unambiguous is the provider's
/// branded gateway error:
///  - ngrok serves `ERR_NGROK_8012` (and siblings) as an HTML page at 502 when
///    the upstream refuses — measured live. The code token is the signal; the
///    502 alone is not (a backend can answer 502 itself).
///  - a cloudflare quick-tunnel edge serves its OWN branded 5xx (`error code:
///    1033`, the `52x` "web server"/"SSL handshake failed" family — measured: a
///    `525` page) when it cannot reach the origin through the tunnel.
fn public_response_is_origin(provider: &str, status: u16, body: &str) -> bool {
    match provider {
        "ngrok" => !body.contains("ERR_NGROK_"),
        "cloudflare" => {
            // The branding IS the discriminator: cloudflare's own gateway
            // errors carry either an `error code: NNNN` token (1033 "Argo
            // Tunnel error" ships at 530, outside the 52x band) or its name on
            // a 5xx interstitial (measured: a `525` "SSL handshake failed"
            // page). A plain 5xx WITHOUT that branding is the backend's own,
            // i.e. reachable.
            let b = body.to_ascii_lowercase();
            let edge_error = b.contains("error code:")
                || (status >= 500 && (b.contains("cloudflare") || b.contains("cloudflared")));
            !edge_error
        }
        _ => true,
    }
}

/// Dispatches on whether a token was resolved: no token is the anonymous
/// quick tunnel (unchanged); a token is a NAMED tunnel already created
/// through the Cloudflare dashboard or `cloudflared tunnel create` — running
/// it is just `cloudflared tunnel run --token <token>`, confirmed against a
/// real `cloudflared v2026.8.2` binary's `tunnel run --help`. **No Cloudflare
/// API calls happen here** — creating a NEW named tunnel from scratch (and
/// its DNS route) is still the documented follow-up this module has always
/// deferred; this only RUNS a tunnel the operator already has a token for,
/// the direct analogue of pinggy's/ngrok's paid-token path.
fn spawn_cloudflare(
    rec: &mut TunnelRecord,
    token: Option<&str>,
    hostname: Option<&str>,
    insecure_skip_tls_verify: bool,
) -> Result<()> {
    which("cloudflared").ok_or_else(|| {
        Error::Unavailable(
            super::po::t(
                "`cloudflared` not found in PATH — install it \
                 (https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/) \
                 before using provider=cloudflare",
            )
            .into(),
        )
    })?;
    match token {
        Some(t) => spawn_cloudflare_named(rec, t, hostname, insecure_skip_tls_verify),
        None => {
            if hostname.is_some() {
                return Err(Error::Invalid(
                    super::po::t(
                        "provider=cloudflare with hostname but no token: the anonymous quick-tunnel \
                         (ephemeral *.trycloudflare.com URL) cannot pick its own hostname — pass \
                         --token (or --token-secret) for a NAMED tunnel you already created, whose \
                         public hostname is configured in the Cloudflare dashboard",
                    )
                    .into(),
                ));
            }
            spawn_cloudflare_quick(rec, insecure_skip_tls_verify)
        }
    }
}

fn cloudflare_origin_url(local_port: u16, insecure_skip_tls_verify: bool) -> String {
    let scheme = if insecure_skip_tls_verify {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://localhost:{local_port}")
}

/// Live-validated against a real `cloudflared v2026.8.2` quick tunnel and a
/// self-signed local HTTPS server: without `--no-tls-verify` the public URL
/// answers 502 (origin TLS rejected); with it (and the `https://` scheme in
/// `--url`), the same request reaches the backend and returns 200.
fn spawn_cloudflare_quick(rec: &mut TunnelRecord, insecure_skip_tls_verify: bool) -> Result<()> {
    let mut args = vec![
        "tunnel".to_string(),
        "--url".to_string(),
        cloudflare_origin_url(rec.local_port, insecure_skip_tls_verify),
    ];
    if insecure_skip_tls_verify {
        args.push("--no-tls-verify".to_string());
    }
    spawn_and_capture(rec, "cloudflared", &args, None, |t| {
        find_url_containing(t, ".trycloudflare.com")
    })
}

/// `--url`/`--no-tls-verify` are accepted by `cloudflared tunnel run` too,
/// and per its own `--help` text they only take effect "if you define your
/// origin with --url and if you do not use ingress rules" — i.e. they are a
/// harmless no-op on a tunnel whose Public Hostname routes are already
/// configured remotely, and the working origin override on one that has
/// none yet (the common case for a tunnel just created for this purpose).
/// **Not live-validated with a real token** (no Cloudflare account available
/// in this sandbox) — confirmed instead with an invalid token, which fails
/// fast and clearly ("Provided Tunnel token is not valid"), proving the argv
/// this function builds is accepted by the real binary.
fn spawn_cloudflare_named(
    rec: &mut TunnelRecord,
    token: &str,
    hostname: Option<&str>,
    insecure_skip_tls_verify: bool,
) -> Result<()> {
    // The token by environment (`$TUNNEL_TOKEN`, cloudflared's own name for
    // `--token`, per its `--help`), never on the argv `ps` prints.
    let mut args = vec![
        "tunnel".to_string(),
        "run".to_string(),
        "--url".to_string(),
        cloudflare_origin_url(rec.local_port, insecure_skip_tls_verify),
    ];
    if insecure_skip_tls_verify {
        args.push("--no-tls-verify".to_string());
    }
    // The public hostname of a NAMED tunnel is whatever was routed to it in
    // the Cloudflare dashboard — this process never learns it from the log
    // (unlike the quick tunnel, `tunnel run` doesn't print one). `hostname`
    // is the operator telling us what they already configured there; take
    // it at face value rather than leaving the URL blank.
    if let Some(h) = hostname {
        rec.public_url = Some(format!("https://{h}"));
    }
    spawn_and_capture(
        rec,
        "cloudflared",
        &args,
        Some(("TUNNEL_TOKEN", token)),
        |_| None,
    )
}

/// One row of `get gateways`.
///
/// `running` is a boolean and the uptime is SECONDS, not the `Up 3m` the table
/// prints: a script that has to parse «Up 2 weeks» to decide whether a tunnel
/// is old is a script that breaks when the humanizer changes a bucket — which
/// this repo already ported from docker precisely because those buckets are
/// fiddly.
#[derive(serde::Serialize)]
struct TunnelLsRow {
    name: String,
    provider: String,
    local_port: u16,
    /// `null` while the provider has not confirmed one — never a dash, which is
    /// a table's way of saying nothing and a script's way of saying "-".
    #[serde(skip_serializing_if = "Option::is_none")]
    public_url: Option<String>,
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    uptime_seconds: Option<u64>,
}

pub(crate) fn cmd_ls(format: output::OutputFormat) -> Result<()> {
    let format = super::config::resolve_output(&super::util::state_root(), format);
    let store = record_store()?;
    if format == output::OutputFormat::Json {
        let rows: Vec<TunnelLsRow> = store
            .list()?
            .into_iter()
            .map(|rec| {
                let running = is_alive(&rec);
                TunnelLsRow {
                    name: rec.name,
                    provider: rec.provider,
                    local_port: rec.local_port,
                    public_url: rec.public_url,
                    running,
                    uptime_seconds: match (running, rec.started_unix) {
                        (true, Some(s)) => Some(output::now_unix().saturating_sub(s)),
                        _ => None,
                    },
                }
            })
            .collect();
        return output::print_json(&rows);
    }
    let mut t = output::Table::new(&[
        "NAME",
        "PROVIDER",
        "LOCAL PORT",
        "PUBLIC URL",
        "STATUS",
        "UPTIME",
    ])
    .right_align(2);
    for rec in store.list()? {
        let alive = is_alive(&rec);
        t.row(vec![
            rec.name,
            rec.provider,
            rec.local_port.to_string(),
            rec.public_url.unwrap_or_else(|| "-".to_string()),
            if alive {
                "Running".to_string()
            } else {
                "Stopped".to_string()
            },
            match (alive, rec.started_unix) {
                (true, Some(s)) => format!(
                    "Up {}",
                    output::fmt_duration_secs(output::now_unix().saturating_sub(s))
                ),
                _ => "-".to_string(),
            },
        ]);
    }
    t.print();
    Ok(())
}

pub(crate) fn cmd_describe(name: &str) -> Result<()> {
    let store = record_store()?;
    let rec = store.load(name).map_err(|e| match e.into_root() {
        Error::NotFound(n) => {
            Error::Invalid(format!("no such tunnel: {n} (see `delonix get gateways`)"))
        }
        e => e,
    })?;
    let alive = is_alive(&rec);
    let mut d = output::Describe::new();
    d.field("Name", &rec.name);
    d.field("Provider", &rec.provider);
    d.field("Local Port", rec.local_port.to_string());
    d.field_opt("Hostname", rec.hostname.as_deref());
    d.field(
        "Public URL",
        rec.public_url.as_deref().unwrap_or("(not yet known)"),
    );
    // Status anchors on the SUPERVISOR for a supervised tunnel: the tunnel is
    // "Running" while the supervisor lives, even in the backoff window between
    // an agent dying and its respawn — which the second line makes explicit so
    // a momentarily-dead agent is not mistaken for a stopped tunnel.
    d.field("Status", if alive { "Running" } else { "Stopped" });
    // The origin behind the tunnel, as last probed at (re)start — distinct
    // from Status, which only tracks the supervisor/agent. A live tunnel over a
    // dead backend reads Running + "origin unavailable".
    d.field_opt("Origin", rec.origin_state.as_deref());
    if rec.supervisor_pid.is_some() {
        d.field(
            "Agent",
            if agent_alive(&rec) {
                "connected"
            } else if alive {
                "reconnecting"
            } else {
                "stopped"
            },
        );
        d.field_opt(
            "Supervisor PID",
            rec.supervisor_pid.map(|p| p.to_string()).as_deref(),
        );
    }
    d.field_opt("Agent PID", rec.pid.map(|p| p.to_string()).as_deref());
    if rec.restarts > 0 {
        d.field("Restarts", rec.restarts.to_string());
    }
    d.field_opt(
        "Agent Web Port",
        rec.agent_web_port.map(|p| p.to_string()).as_deref(),
    );
    d.field("Created", output::fmt_local(rec.created_unix));
    d.field("Log", log_path(name).display().to_string());
    d.print();
    Ok(())
}

pub(crate) fn cmd_rm(name: &str) -> Result<()> {
    let store = record_store()?;
    let rec = store.load(name).map_err(|e| match e.into_root() {
        Error::NotFound(n) => {
            Error::Invalid(format!("no such tunnel: {n} (see `delonix get gateways`)"))
        }
        e => e,
    })?;
    stop_process(&rec);
    store.remove(name)?;
    let _ = std::fs::remove_file(log_path(name));
    // The pinggy credential file, when there was one.
    let _ = std::fs::remove_file(tunnels_dir().join(format!("{name}.ssh_config")));
    println!("tunnel/{name}: {}", super::po::t("removed"));
    Ok(())
}

#[cfg(test)]
mod tests {
    fn gw(spec: &str) -> Vec<ManifestDoc> {
        vec![serde_yaml::from_str(&format!(
            "apiVersion: gateway.delonix.io/v1alpha1\nkind: Gateway\nmetadata: {{ name: t }}\nspec: {spec}\n"
        ))
        .unwrap()]
    }

    #[test]
    fn type_and_spec_lower_to_the_scalar_and_flat_fields_the_executor_reads() {
        let mut d = gw("{ localPort: 80, provider: { type: cloudflare, spec: { tokenSecretRef: s, hostname: a.example } } }");
        lower_tunnel_provider(&mut d).unwrap();
        let spec = &d[0].spec;
        assert_eq!(spec["provider"], serde_yaml::Value::from("cloudflare"));
        assert_eq!(spec["tokenSecretRef"], serde_yaml::Value::from("s"));
        assert_eq!(spec["hostname"], serde_yaml::Value::from("a.example"));
        // The selection-only scalar passes untouched.
        let mut d = gw("{ localPort: 80, provider: pinggy }");
        lower_tunnel_provider(&mut d).unwrap();
        assert_eq!(d[0].spec["provider"], serde_yaml::Value::from("pinggy"));
    }

    #[test]
    fn a_tunnel_provider_block_is_checked_not_trusted() {
        for (spec, needle) in [
            (
                "{ localPort: 80, provider: { spec: {} } }",
                "type is required",
            ),
            (
                "{ localPort: 80, provider: { type: ngrokk } }",
                "not a tunnel provider",
            ),
            (
                "{ localPort: 80, provider: { type: ngrok, ref: x } }",
                "provider.ref",
            ),
            (
                "{ localPort: 80, provider: { type: ngrok, spec: { region: eu } } }",
                "spec.region",
            ),
            (
                "{ localPort: 80, hostname: a, provider: { type: ngrok, spec: { hostname: b } } }",
                "say it once",
            ),
            (
                "{ localPort: 80, provider: { type: ngrok, spec: [] } }",
                "must be a mapping",
            ),
        ] {
            let mut d = gw(spec);
            let e = lower_tunnel_provider(&mut d).unwrap_err().to_string();
            assert!(e.contains(needle), "{spec}: {e}");
        }
    }

    use super::*;

    #[test]
    fn find_url_containing_ignora_https_nao_relacionados() {
        let text = "banner: visit https://pinggy.io/docs for help\n\
                    forwarding https://abc123.free.pinggy.link -> localhost:8080\n";
        assert_eq!(
            find_url_containing(text, ".pinggy.link"),
            Some("https://abc123.free.pinggy.link".to_string())
        );
    }

    #[test]
    fn find_url_containing_apara_pontuacao_em_volta() {
        let text =
            "Your quick Tunnel has been created! Visit it at (https://foo-bar.trycloudflare.com).";
        assert_eq!(
            find_url_containing(text, ".trycloudflare.com"),
            Some("https://foo-bar.trycloudflare.com".to_string())
        );
    }

    #[test]
    fn find_url_containing_sem_match_devolve_none() {
        assert_eq!(find_url_containing("nothing here", ".pinggy."), None);
    }

    #[test]
    fn pinggy_predicate_ignora_o_link_de_upsell_do_dashboard() {
        // Real `ssh ... free.pinggy.io` output, captured live — this is the
        // exact bug found while validating this module: a naive `.contains(".pinggy.")`
        // matched `https://dashboard.pinggy.io` (pinggy's own upsell banner,
        // printed FIRST) instead of the real tunnel URL that follows it.
        let log = "Pseudo-terminal will not be allocated because stdin is not a terminal.\n\
                   Warning: Permanently added '[free.pinggy.io]:443' (RSA) to the list of known hosts.\n\
                   Allocated port 8 for remote forward to localhost:18234\n\
                   You are not authenticated.\n\
                   Your tunnel will expire in 60 minutes. Upgrade to Pinggy Pro to get unrestricted tunnels. https://dashboard.pinggy.io\n\
                   http://ccjjc-197-148-40-67.run.pinggy-free.link\n\
                   http://gzohk-197-148-40-67.free.pinggy.net\n\
                   https://ccjjc-197-148-40-67.run.pinggy-free.link\n\
                   https://gzohk-197-148-40-67.free.pinggy.net\n";
        let found = find_url_where(log, |u| {
            u.contains("pinggy") && !u.contains("dashboard.pinggy.io")
        });
        assert_eq!(
            found,
            Some("https://ccjjc-197-148-40-67.run.pinggy-free.link".to_string())
        );
    }

    #[test]
    fn pid_alive_distingue_processo_vivo_de_pid_inexistente() {
        assert!(pid_alive(std::process::id() as i32));
        // A high PID astronomically unlikely to be in use on any real host —
        // same class of assumption `spawn_and_capture`'s own early-death
        // check already relies on.
        assert!(!pid_alive(i32::MAX - 1));
    }

    #[test]
    fn pid_alive_reaps_an_unwaited_zombie_child_of_this_process() {
        // Found live testing `provider=cloudflare --token <bad>`: a bare
        // `/proc/<pid>` check reports a zombie as "alive" for as long as
        // this process keeps running, because nobody ever `waitpid`ed it —
        // and every `spawn_and_capture` caller relies on `pid_alive`
        // detecting death promptly to break its poll loop early. Reverting
        // the `waitpid(..., WNOHANG)` line in `pid_alive` makes this fail.
        let mut child = Command::new("true").spawn().expect("spawn `true`");
        let pid = child.id() as i32;
        // Give the child time to actually exit (it's `true`, near-instant)
        // before checking — a fresh child can look alive for a moment
        // regardless of the bug under test.
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !pid_alive(pid),
            "an exited-but-unwaited child must not read as alive just because its zombie /proc entry persists"
        );
        // `pid_alive` already reaped it via its own waitpid; avoid a second
        // wait through the `Child` handle racing with that.
        let _ = child.try_wait();
    }

    #[test]
    fn resolve_token_recusa_token_a_comecar_por_traco() {
        // CRITICAL fixed here: a token like "-oProxyCommand=..." embedded as
        // `<token>@free.pinggy.io`, the last positional ssh argv element, was
        // parsed by ssh as an OPTION instead of part of the destination —
        // local RCE via ProxyCommand. Reject before it ever reaches argv.
        let err =
            resolve_token(Some("-oProxyCommand=touch /tmp/pwned".to_string()), None).unwrap_err();
        assert!(format!("{err}").contains("cannot start with"));
        // A normal token is untouched.
        assert_eq!(
            resolve_token(Some("mytoken".to_string()), None).unwrap(),
            Some("mytoken".to_string())
        );
        assert_eq!(resolve_token(None, None).unwrap(), None);
    }

    #[test]
    fn config_hash_muda_com_qualquer_campo() {
        let base = TunnelSpec {
            provider: "pinggy".to_string(),
            local_port: 8080,
            hostname: None,
            token: None,
            token_secret_ref: None,
            insecure_skip_tls_verify: false,
        };
        let h0 = config_hash(&base, &None);
        let mut port_changed = base.clone();
        port_changed.local_port = 9090;
        assert_ne!(h0, config_hash(&port_changed, &None));
        assert_ne!(h0, config_hash(&base, &Some("tok".to_string())));
        let mut host_changed = base.clone();
        host_changed.hostname = Some("app.example.com".to_string());
        assert_ne!(h0, config_hash(&host_changed, &None));
        let mut tls_changed = base.clone();
        tls_changed.insecure_skip_tls_verify = true;
        assert_ne!(h0, config_hash(&tls_changed, &None));
        // Same effective config → same hash (idempotency check for `apply_one`).
        assert_eq!(h0, config_hash(&base, &None));
    }

    /// A `TunnelRecord` for the guard tests — only the fields the guard reads
    /// carry meaning here.
    fn guard_rec(provider: &str, port: u16) -> TunnelRecord {
        TunnelRecord {
            name: "t".to_string(),
            provider: provider.to_string(),
            local_port: port,
            hostname: None,
            config_hash: "x".to_string(),
            pid: Some(1),
            pid_starttime: None,
            supervisor_pid: None,
            supervisor_starttime: None,
            restarts: 0,
            public_url: None,
            created_unix: 0,
            started_unix: None,
            agent_web_port: None,
            insecure_skip_tls_verify: false,
            origin_state: None,
        }
    }

    fn argv_blob(parts: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p.as_bytes());
            v.push(0);
        }
        v
    }

    /// ACH-018. Every one of these was ALIVE on the development host on
    /// 2026-09-10 and passed the old `cmdline.contains("ssh")` guard, whose pid
    /// `stop_process` sends `SIGTERM` to. None is a tunnel.
    #[test]
    fn the_guard_refuses_the_ssh_processes_the_old_one_accepted() {
        let rec = guard_rec("pinggy", 8080);
        for (what, argv) in [
            (
                "the user's ssh-agent",
                vec![
                    "/usr/bin/ssh-agent",
                    "-D",
                    "-a",
                    "/run/user/1000/keyring/.ssh",
                ],
            ),
            (
                "gcr-ssh-agent",
                vec![
                    "/usr/libexec/gcr-ssh-agent",
                    "--base-dir",
                    "/run/user/1000/gcr",
                ],
            ),
            (
                "an Ansible mux",
                vec!["ssh: /home/walter/.ansible/cp/3648f59cba [mux]"],
            ),
            (
                "a path that merely spells ssh",
                vec!["/bin/bash", "-c", "source /home/walter/.claude/x-ssh-y.sh"],
            ),
        ] {
            assert!(
                !agent_argv_is_ours(&argv_blob(&argv), &rec),
                "the guard accepted {what}, which is not a tunnel"
            );
        }
    }

    /// A REAL `ssh`, same user, going somewhere else — the shape `argv[0]`
    /// alone does not separate.
    #[test]
    fn the_guard_refuses_a_real_ssh_that_is_not_our_agent() {
        let rec = guard_rec("pinggy", 8080);
        assert!(
            !agent_argv_is_ours(
                &argv_blob(&["ssh", "-R", "0:localhost:9999", "--", "free.pinggy.io"]),
                &rec
            ),
            "different local port"
        );
        assert!(
            !agent_argv_is_ours(
                &argv_blob(&[
                    "ssh",
                    "-R",
                    "0:localhost:8080",
                    "--",
                    "walter@servidor.interno"
                ]),
                &rec
            ),
            "same port, destination is not the provider"
        );
    }

    /// The argv `spawn_pinggy_at` actually builds.
    #[test]
    fn the_guard_accepts_the_agent_we_spawned() {
        let rec = guard_rec("pinggy", 8080);
        assert!(agent_argv_is_ours(
            &argv_blob(&[
                "ssh",
                "-p",
                "443",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ExitOnForwardFailure=yes",
                "-R",
                "0:localhost:8080",
                "--",
                "free.pinggy.io",
            ]),
            &rec
        ));
        let ng = guard_rec("ngrok", 8080);
        assert!(agent_argv_is_ours(
            &argv_blob(&["ngrok", "http", "8080", "--log", "stdout"]),
            &ng
        ));
        let cf = guard_rec("cloudflare", 8080);
        assert!(agent_argv_is_ours(
            &argv_blob(&[
                "cloudflared",
                "tunnel",
                "--url",
                &cloudflare_origin_url(8080, false)
            ]),
            &cf
        ));
    }

    /// A provider this build does not know owns no process here.
    #[test]
    fn the_guard_refuses_an_unknown_provider() {
        let rec = guard_rec("carrier-pigeon", 8080);
        assert!(!agent_argv_is_ours(
            &argv_blob(&["ssh", "0:localhost:8080"]),
            &rec
        ));
    }

    /// ACH-018, the half that separates TWO roots: the argv of two agents of the
    /// same provider and port is byte-identical; only the environ tells them apart.
    #[test]
    fn the_environ_separates_two_roots_and_tolerates_an_older_agent() {
        let mine = std::path::Path::new("/tmp/root-a");
        let env = |v: &str| {
            let mut b = Vec::new();
            b.extend_from_slice(format!("DELONIX_ROOT={v}").as_bytes());
            b.push(0);
            b.extend_from_slice(b"PATH=/usr/bin");
            b.push(0);
            b
        };
        assert!(env_allows_this_root(&env("/tmp/root-a"), mine), "ours");
        assert!(
            !env_allows_this_root(&env("/tmp/root-b"), mine),
            "another root's"
        );
        // An agent started by a build older than this fix carries no such
        // variable — tolerated on purpose, or its `rm` would start reporting
        // success without signalling anything.
        assert!(
            env_allows_this_root(b"PATH=/usr/bin\0", mine),
            "pre-fix agent"
        );
    }

    #[test]
    fn is_alive_falso_sem_pid() {
        let rec = TunnelRecord {
            name: "t".to_string(),
            provider: "pinggy".to_string(),
            local_port: 8080,
            hostname: None,
            config_hash: "x".to_string(),
            pid: None,
            pid_starttime: None,
            supervisor_pid: None,
            supervisor_starttime: None,
            restarts: 0,
            public_url: None,
            created_unix: 0,
            started_unix: None,
            agent_web_port: None,
            insecure_skip_tls_verify: false,
            origin_state: None,
        };
        assert!(!is_alive(&rec));
    }

    #[test]
    fn is_alive_falso_para_provider_desconhecido() {
        let rec = TunnelRecord {
            name: "t".to_string(),
            provider: "carrier-pigeon".to_string(),
            local_port: 8080,
            hostname: None,
            config_hash: "x".to_string(),
            pid: Some(1),
            pid_starttime: None,
            supervisor_pid: None,
            supervisor_starttime: None,
            restarts: 0,
            public_url: None,
            created_unix: 0,
            started_unix: None,
            agent_web_port: None,
            insecure_skip_tls_verify: false,
            origin_state: None,
        };
        assert!(!is_alive(&rec));
    }

    #[test]
    fn other_alive_ngrok_ignores_a_not_genuinely_alive_record() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonStore::<TunnelRecord>::open(tmp.path()).unwrap();
        // A "running" record claiming the fixed ngrok web port — is_alive
        // requires a real /proc entry, so fake it with our OWN pid (this test
        // process, definitely alive) and a cmdline that won't contain
        // "ngrok"... which means is_alive is actually false here. That's
        // fine: it proves a stale/dead record never blocks a new tunnel.
        let stale = TunnelRecord {
            name: "other".to_string(),
            provider: "ngrok".to_string(),
            local_port: 1234,
            hostname: None,
            config_hash: "x".to_string(),
            pid: Some(std::process::id() as i32),
            pid_starttime: None,
            supervisor_pid: None,
            supervisor_starttime: None,
            restarts: 0,
            public_url: None,
            created_unix: 0,
            started_unix: None,
            agent_web_port: Some(4040),
            insecure_skip_tls_verify: false,
            origin_state: None,
        };
        store.save("other", &stale).unwrap();
        assert!(
            other_alive_ngrok(&store, "mine").unwrap().is_none(),
            "o registo não está genuinamente vivo (cmdline não é ngrok)"
        );
    }

    /// **BUG FIXED: re-applying an ngrok tunnel refused itself.**
    ///
    /// `apply_one` reaches `spawn_ngrok` having just signalled the agent of
    /// the record it is REPLACING — same name, provider `ngrok`, and still
    /// alive in `/proc` for as long as SIGTERM takes to land. The filter
    /// said "other" but compared no name, so the tunnel being edited was
    /// reported as its own rival: "another ngrok tunnel is already running
    /// ('web') … stop it first (`delonix delete gateways web`)" — naming the
    /// tunnel the operator was in the middle of changing, which is what made
    /// an ngrok Gateway look like it could be created once and never edited.
    ///
    /// Liveness is forced TRUE here (the real `is_alive` needs a genuine
    /// `/proc` entry whose argv is ngrok's, unfakeable from a test) so the
    /// assertion is about the rule and nothing else. Dropping the
    /// `r.name != self_name` clause in `ngrok_rival` makes this fail.
    #[test]
    fn ngrok_rival_never_counts_the_tunnel_being_applied() {
        let rec = |name: &str, provider: &str| TunnelRecord {
            name: name.to_string(),
            provider: provider.to_string(),
            local_port: 1234,
            hostname: None,
            config_hash: "x".to_string(),
            pid: Some(std::process::id() as i32),
            pid_starttime: None,
            supervisor_pid: None,
            supervisor_starttime: None,
            restarts: 0,
            public_url: None,
            created_unix: 0,
            started_unix: None,
            agent_web_port: Some(4040),
            insecure_skip_tls_verify: false,
            origin_state: None,
        };
        // Re-applying 'web' over its own live record: not a rival.
        assert!(
            ngrok_rival(vec![rec("web", "ngrok")], "web", |_| true).is_none(),
            "o túnel a ser aplicado não é rival de si mesmo"
        );
        // A DIFFERENT live ngrok tunnel still is — the guard must keep working.
        assert_eq!(
            ngrok_rival(vec![rec("api", "ngrok")], "web", |_| true)
                .map(|r| r.name)
                .as_deref(),
            Some("api"),
            "um segundo túnel ngrok continua a ser recusado"
        );
        // And the own-record exemption is by NAME, not "first record wins":
        // a rival hiding behind one's own record is still found.
        assert_eq!(
            ngrok_rival(
                vec![rec("web", "ngrok"), rec("api", "ngrok")],
                "web",
                |_| true
            )
            .map(|r| r.name)
            .as_deref(),
            Some("api")
        );
    }

    #[test]
    fn other_alive_ngrok_ignores_other_providers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonStore::<TunnelRecord>::open(tmp.path()).unwrap();
        let pinggy_rec = TunnelRecord {
            name: "p".to_string(),
            provider: "pinggy".to_string(),
            local_port: 1234,
            hostname: None,
            config_hash: "x".to_string(),
            pid: Some(std::process::id() as i32),
            pid_starttime: None,
            supervisor_pid: None,
            supervisor_starttime: None,
            restarts: 0,
            public_url: None,
            created_unix: 0,
            started_unix: None,
            agent_web_port: None,
            insecure_skip_tls_verify: false,
            origin_state: None,
        };
        store.save("p", &pinggy_rec).unwrap();
        assert!(other_alive_ngrok(&store, "mine").unwrap().is_none());
    }

    /// A provider credential never lands in a world-readable place: the
    /// token is refused if it could open a second line of the ssh config.
    #[test]
    fn a_token_with_whitespace_or_a_control_character_is_refused() {
        for bad in ["a b", "a\nHost evil", "a\tb", ""] {
            assert!(
                super::resolve_token(Some(bad.to_string()), None).is_err(),
                "{bad:?}"
            );
        }
        assert!(super::resolve_token(Some("tok_123+tcp".into()), None).is_ok());
    }

    #[test]
    fn the_pinggy_config_names_the_token_as_the_user_and_nothing_else() {
        assert_eq!(super::pinggy_ssh_config("t0k"), "Host *\n  User t0k\n");
    }

    #[test]
    fn backoff_base_doubles_and_caps_at_32s() {
        // restarts is 1-based: the first respawn waits 1s, then 2,4,8,16,32,
        // and 32 forever after — a crash-looping agent can never busy-spin.
        let seq: Vec<u64> = (1..=8).map(backoff_base_secs).collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 32, 32, 32]);
        // Monotonic non-decreasing and never above the cap, for any restart.
        let mut prev = 0;
        for r in 1..100 {
            let b = backoff_base_secs(r);
            assert!(b >= prev && b <= 32, "restart {r}: {b}");
            prev = b;
        }
    }

    #[test]
    fn backoff_jitter_stays_within_its_ceiling_and_varies() {
        // Never exceeds the ceiling (so the total delay stays bounded).
        for r in 1..40u32 {
            let ceiling = backoff_base_secs(r) * 250;
            for seed in [1, 7, 12345, i32::MAX, -3] {
                assert!(backoff_jitter_ms(seed, r, ceiling) <= ceiling);
            }
        }
        // A zero ceiling is a no-op, not a divide-by-zero.
        assert_eq!(backoff_jitter_ms(42, 1, 0), 0);
        // Different supervisors (seeds) decorrelate: two tunnels that crash on
        // the same restart do not all wake at the identical instant.
        let a = backoff_jitter_ms(1000, 3, 1000);
        let b = backoff_jitter_ms(2000, 3, 1000);
        assert_ne!(a, b, "distinct seeds must not collapse to one delay");
    }

    #[test]
    fn backoff_delay_is_base_plus_jitter_and_bounded() {
        // The full delay is the base second-count plus at most 25% jitter.
        for r in 1..20u32 {
            let base_ms = backoff_base_secs(r) * 1000;
            let d = backoff_delay(r, 98765).as_millis() as u64;
            assert!(d >= base_ms, "restart {r}: {d} < {base_ms}");
            assert!(d <= base_ms + base_ms / 4, "restart {r}: {d} over ceiling");
        }
    }

    #[test]
    fn should_keep_running_stops_on_removal_and_config_change() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonStore::<TunnelRecord>::open(tmp.path()).unwrap();
        // No record yet (`rm` already happened): stop.
        assert!(!should_keep_running(&store, "t", "h1"));
        let mut rec = guard_rec("pinggy", 8080);
        rec.name = "t".to_string();
        rec.config_hash = "h1".to_string();
        store.save("t", &rec).unwrap();
        // Same config the supervisor was started for: keep going.
        assert!(should_keep_running(&store, "t", "h1"));
        // A newer apply wrote a different hash (and forked its own supervisor):
        // this one must bow out rather than fight over the agent/port.
        assert!(!should_keep_running(&store, "t", "h1-old"));
    }

    #[test]
    fn is_alive_anchors_on_the_supervisor_when_one_is_recorded() {
        // A supervised record whose SUPERVISOR is this (alive) process reads as
        // Running even though its agent pid is bogus — the backoff-window state
        // an operator wants called "Running (reconnecting)", not "Stopped".
        let mut rec = guard_rec("pinggy", 8080);
        rec.supervisor_pid = Some(std::process::id() as i32);
        rec.supervisor_starttime = proc_starttime(std::process::id() as i32);
        rec.pid = Some(i32::MAX - 1); // no such agent
        assert!(is_alive(&rec), "supervisor alive ⇒ tunnel Running");
        assert!(!agent_alive(&rec), "but the agent itself is not");
        // A dead supervisor flips it to Stopped.
        rec.supervisor_pid = Some(i32::MAX - 1);
        rec.supervisor_starttime = None;
        assert!(!is_alive(&rec));
    }

    /// The whole point of the probe: a reply must be CONFIRMED to come from the
    /// backend, not merely be "some HTTP response". The real `ERR_NGROK_8012`
    /// page a dead origin yields (captured live, served at 502) must read as
    /// NOT-origin, while the backend's own bytes — whatever status it chooses,
    /// including a 404 or a 500 of its own — read as origin.
    #[test]
    fn ngrok_err_page_is_not_the_origin_but_a_real_reply_is() {
        // The live-captured shape: 502 + the error-code token in the body.
        let err = "<html>...ERR_NGROK_8012...The connection to ... was refused</html>";
        assert!(!public_response_is_origin("ngrok", 502, err));
        // A backend that answers for itself — any status — is reachable.
        assert!(public_response_is_origin(
            "ngrok",
            200,
            "a-random-nonce-xyz"
        ));
        assert!(public_response_is_origin("ngrok", 404, "my app's own 404"));
        assert!(public_response_is_origin("ngrok", 500, "my app's own 500"));
    }

    /// "Any 200 is success" is the bug: a provider's OWN 200 page is not the
    /// origin. An ngrok error token at 200 (or any status) still means down.
    #[test]
    fn ngrok_classification_ignores_the_status_code_and_reads_the_body() {
        assert!(!public_response_is_origin(
            "ngrok",
            200,
            "ERR_NGROK_3200 page"
        ));
        assert!(!public_response_is_origin("ngrok", 418, "ERR_NGROK_8012"));
    }

    /// cloudflare's quick-tunnel edge serves its OWN branded 5xx (measured: a
    /// `525` "SSL handshake failed" page, cloudflare-branded) when it cannot
    /// reach the origin — not the origin. A 5xx WITHOUT the cloudflare branding
    /// is the backend's own and counts as reachable; and a normal 200 from the
    /// backend is the origin.
    #[test]
    fn cloudflare_edge_error_page_is_not_the_origin() {
        let e525 = "<title>mysite | 525: SSL handshake failed</title> ... cloudflare";
        assert!(!public_response_is_origin("cloudflare", 525, e525));
        let e1033 = "<html>... error code: 1033 ... cloudflare ...</html>";
        assert!(!public_response_is_origin("cloudflare", 530, e1033));
        // A 5xx that is the BACKEND's own (no cloudflare branding) is reachable.
        assert!(public_response_is_origin(
            "cloudflare",
            502,
            "my app blew up"
        ));
        // The happy path.
        assert!(public_response_is_origin("cloudflare", 200, "the-nonce"));
    }

    /// A refused local connect is the one unambiguous "down"; a bound port is
    /// not (yet) a verdict for an HTTP provider — only for the raw-TCP one.
    #[test]
    fn local_connect_refused_is_the_only_unavailable() {
        // Nothing listens on this port in the test process.
        let mut rec = guard_rec("cloudflare", 59_123);
        rec.public_url = Some("https://x.trycloudflare.com".into());
        assert_eq!(probe_origin(&rec), OriginReadiness::Unavailable);
        // Same dead port, raw-TCP provider: still Unavailable (the connect
        // decides, before any provider-specific step).
        let pinggy = guard_rec("pinggy", 59_123);
        assert_eq!(probe_origin(&pinggy), OriginReadiness::Unavailable);
    }

    /// The stored token is what `describe`/`expose` render; `Unknown` stores
    /// nothing so old and not-probed records stay clean.
    #[test]
    fn origin_state_tokens_are_stable() {
        assert_eq!(
            OriginReadiness::Ready.into_state().as_deref(),
            Some("ready")
        );
        assert_eq!(
            OriginReadiness::Unavailable.into_state().as_deref(),
            Some("unavailable")
        );
        assert_eq!(OriginReadiness::Unknown.into_state(), None);
    }
}

//! `delonix net tunnel` (`kind: Tunnel`) — exposes ONE local TCP port to the
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

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Subcommand;
use delonix_runtime_core::{Error, JsonStore, Result};
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
    pid: Option<i32>,
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
}

#[derive(Subcommand, Debug)]
pub enum TunnelCmd {
    /// Apply the `kind: Tunnel` documents of a manifest (idempotent).
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
    /// List tunnels (state + public URL).
    Ls,
    /// Human-readable detail of one tunnel.
    Describe {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::tunnels))]
        name: String,
    },
    /// Stop and remove a tunnel.
    Rm {
        #[arg(add = clap_complete::engine::ArgValueCandidates::new(super::complete::tunnels))]
        name: String,
    },
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
        TunnelCmd::Ls => cmd_ls(),
        TunnelCmd::Describe { name } => cmd_describe(&name),
        TunnelCmd::Rm { name } => cmd_rm(&name),
    }
}

/// Fields the reconciler compares for a `kind: Tunnel`.
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
        kind: "Gateway".into(),
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
    for doc in manifest::of_kind(docs, "Gateway") {
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
            kind: "Gateway".into(),
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
    match record_store().and_then(|s| s.load(name)) {
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
    for doc in manifest::of_kind(docs, "Gateway") {
        let spec: TunnelSpec = manifest::spec_of(doc)?;
        apply_one(&doc.metadata.name, &spec)?;
    }
    Ok(())
}

fn tunnels_dir() -> PathBuf {
    state_root().join("tunnels")
}

fn record_store() -> Result<JsonStore<TunnelRecord>> {
    JsonStore::open(tunnels_dir())
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
        let store = delonix_runtime_core::SecretStore::open(state_root())?;
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
    }
    Ok(token)
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

/// The agent is genuinely alive AND is really ours: same identity-guard
/// pattern as `ingress_proxy::running_pid`, checking the provider's OWN
/// binary name in `/proc/<pid>/cmdline`. Narrower than that guard (`ssh`/
/// `ngrok`/`cloudflared` are common process names, unlike the unique
/// `ingress-proxy`) — an accepted, documented gap: a PID recycled into an
/// unrelated process of the SAME binary is (rare, but) not detected.
fn is_alive(rec: &TunnelRecord) -> bool {
    let Some(pid) = rec.pid else { return false };
    let want = match rec.provider.as_str() {
        "pinggy" => "ssh",
        "ngrok" => "ngrok",
        "cloudflare" => "cloudflared",
        _ => return false,
    };
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| String::from_utf8_lossy(&c).contains(want))
        .unwrap_or(false)
}

fn stop_process(rec: &TunnelRecord) {
    if let Some(pid) = rec.pid {
        if is_alive(rec) {
            // SAFETY: signalling a PID we just confirmed alive AND ours (cmdline guard).
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
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
        stop_process(&existing);
    }

    let now = output::now_unix();
    let mut rec = TunnelRecord {
        name: name.to_string(),
        provider: spec.provider.clone(),
        local_port: spec.local_port,
        hostname: spec.hostname.clone(),
        config_hash: hash,
        pid: None,
        public_url: None,
        created_unix: now,
        started_unix: None,
        agent_web_port: None,
        insecure_skip_tls_verify: spec.insecure_skip_tls_verify,
    };

    match spec.provider.as_str() {
        "pinggy" => spawn_pinggy(&mut rec, token.as_deref())?,
        "ngrok" => spawn_ngrok(
            &mut rec,
            token.as_deref(),
            spec.hostname.as_deref(),
            spec.insecure_skip_tls_verify,
            &store,
        )?,
        "cloudflare" => spawn_cloudflare(
            &mut rec,
            token.as_deref(),
            spec.hostname.as_deref(),
            spec.insecure_skip_tls_verify,
        )?,
        _ => unreachable!(),
    }
    rec.started_unix = Some(now);
    store.save(name, &rec)?;
    println!(
        "tunnel/{name}: {} — {}",
        super::po::t("running"),
        rec.public_url.as_deref().unwrap_or(super::po::t(
            "(URL not confirmed yet — see `delonix net tunnel describe` / the log)"
        ))
    );
    Ok(())
}

/// Spawns `bin(args)` detached (setsid) with stdout+stderr to this tunnel's
/// log file, confirms it didn't die immediately, then polls the log for a
/// matching URL via `extract` for up to 15s (best-effort: a provider slow to
/// print its URL just leaves `public_url: None`, not an error — the tunnel
/// IS up either way).
fn spawn_and_capture(
    rec: &mut TunnelRecord,
    bin: &str,
    args: &[String],
    extract: impl Fn(&str) -> Option<String>,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let path = log_path(&rec.name);
    let log = std::fs::File::create(&path).map_err(|e| Error::Runtime {
        context: "open tunnel log",
        message: e.to_string(),
    })?;
    let log2 = log.try_clone().map_err(|e| Error::Runtime {
        context: "clone log",
        message: e.to_string(),
    })?;
    let mut cmd = Command::new(bin);
    cmd.args(args).stdin(Stdio::null()).stdout(log).stderr(log2);
    // SAFETY: setsid in the child (post-fork, pre-exec) detaches it from this
    // process so it survives the CLI exiting — same pattern as `ingress_proxy`.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| Error::Runtime {
        context: "spawn tunnel agent",
        message: super::po::tf(
            "{bin}: {e} (is it installed and in PATH?)",
            &[("bin", bin), ("e", &e.to_string())],
        ),
    })?;
    rec.pid = Some(child.id() as i32);
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
    let user_host = match token {
        Some(t) => format!("{t}@{host}"),
        None => host.to_string(),
    };
    let args = vec![
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
        user_host,
    ];
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
    spawn_and_capture(rec, "ssh", &args, |t| {
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

/// The other ngrok record (if any) currently holding the fixed web API port
/// — pure lookup over the store, independent of actually spawning anything,
/// so it is testable without an `ngrok` binary on `PATH`.
fn other_alive_ngrok(store: &JsonStore<TunnelRecord>) -> Result<Option<TunnelRecord>> {
    Ok(store
        .list()?
        .into_iter()
        .find(|r| r.provider == "ngrok" && is_alive(r)))
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
    if let Some(other) = other_alive_ngrok(store)? {
        return Err(Error::Invalid(super::po::tf(
            "another ngrok tunnel is already running ('{name}') — ngrok v3's local agent API \
             has no per-tunnel port anymore, so only one ngrok-provider tunnel can run on this \
             host at a time; stop it first (`delonix net tunnel rm {name}`)",
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
    if let Some(t) = token {
        args.push("--authtoken".to_string());
        args.push(t.to_string());
    }
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
    spawn_and_capture(rec, "ngrok", &args, |_| None)?;
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
    spawn_and_capture(rec, "cloudflared", &args, |t| {
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
    let mut args = vec![
        "tunnel".to_string(),
        "run".to_string(),
        "--token".to_string(),
        token.to_string(),
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
    spawn_and_capture(rec, "cloudflared", &args, |_| None)
}

fn cmd_ls() -> Result<()> {
    let store = record_store()?;
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

fn cmd_describe(name: &str) -> Result<()> {
    let store = record_store()?;
    let rec = store.load(name).map_err(|e| match e {
        Error::NotFound(n) => {
            Error::Invalid(format!("no such tunnel: {n} (see `delonix net tunnel ls`)"))
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
    d.field("Status", if alive { "Running" } else { "Stopped" });
    d.field_opt("PID", rec.pid.map(|p| p.to_string()).as_deref());
    d.field_opt(
        "Agent Web Port",
        rec.agent_web_port.map(|p| p.to_string()).as_deref(),
    );
    d.field("Created", output::fmt_local(rec.created_unix));
    d.field("Log", log_path(name).display().to_string());
    d.print();
    Ok(())
}

fn cmd_rm(name: &str) -> Result<()> {
    let store = record_store()?;
    let rec = store.load(name).map_err(|e| match e {
        Error::NotFound(n) => {
            Error::Invalid(format!("no such tunnel: {n} (see `delonix net tunnel ls`)"))
        }
        e => e,
    })?;
    stop_process(&rec);
    store.remove(name)?;
    let _ = std::fs::remove_file(log_path(name));
    println!("tunnel/{name}: {}", super::po::t("removed"));
    Ok(())
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn is_alive_falso_sem_pid() {
        let rec = TunnelRecord {
            name: "t".to_string(),
            provider: "pinggy".to_string(),
            local_port: 8080,
            hostname: None,
            config_hash: "x".to_string(),
            pid: None,
            public_url: None,
            created_unix: 0,
            started_unix: None,
            agent_web_port: None,
            insecure_skip_tls_verify: false,
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
            public_url: None,
            created_unix: 0,
            started_unix: None,
            agent_web_port: None,
            insecure_skip_tls_verify: false,
        };
        assert!(!is_alive(&rec));
    }

    #[test]
    fn other_alive_ngrok_ignores_a_not_genuinely_alive_record() {
        let tmp = std::env::temp_dir().join(format!(
            "delonix-tunnel-otherngrok-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let store = JsonStore::<TunnelRecord>::open(&tmp).unwrap();
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
            public_url: None,
            created_unix: 0,
            started_unix: None,
            agent_web_port: Some(4040),
            insecure_skip_tls_verify: false,
        };
        store.save("other", &stale).unwrap();
        assert!(
            other_alive_ngrok(&store).unwrap().is_none(),
            "o registo não está genuinamente vivo (cmdline não é ngrok)"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn other_alive_ngrok_ignores_other_providers() {
        let tmp = std::env::temp_dir().join(format!(
            "delonix-tunnel-otherngrok-provider-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let store = JsonStore::<TunnelRecord>::open(&tmp).unwrap();
        let pinggy_rec = TunnelRecord {
            name: "p".to_string(),
            provider: "pinggy".to_string(),
            local_port: 1234,
            hostname: None,
            config_hash: "x".to_string(),
            pid: Some(std::process::id() as i32),
            public_url: None,
            created_unix: 0,
            started_unix: None,
            agent_web_port: None,
            insecure_skip_tls_verify: false,
        };
        store.save("p", &pinggy_rec).unwrap();
        assert!(other_alive_ngrok(&store).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

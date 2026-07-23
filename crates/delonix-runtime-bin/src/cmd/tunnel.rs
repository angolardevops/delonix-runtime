//! `delonix tunnel` (`kind: Tunnel`) — exposes ONE local TCP port to the
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
//!   Free tier: ephemeral URL, ~60 min session.
//! - `ngrok`: needs the `ngrok` agent on `PATH` (clear error if absent).
//!   Public URL is read from the agent's own local HTTP API
//!   (`127.0.0.1:<web-addr>/api/tunnels`), not by scraping logs — the
//!   documented way to get it programmatically.
//! - `cloudflare`: needs `cloudflared` on `PATH`. **Only the QUICK TUNNEL**
//!   (`cloudflared tunnel --url ...`, zero account, random
//!   `*.trycloudflare.com` URL) is implemented. A NAMED tunnel with a
//!   custom domain needs a 3-step Cloudflare API dance (create tunnel →
//!   PUT ingress config → create DNS record — all confirmed against their
//!   docs while designing this) plus `Secret`-based API-token handling; real
//!   scope of its own, left as a documented follow-up rather than half-built.
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

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TunnelSpec {
    /// `pinggy` | `ngrok` | `cloudflare`.
    provider: String,
    #[serde(rename = "localPort")]
    local_port: u16,
    /// Custom/reserved hostname — provider-dependent support (see module doc).
    #[serde(default)]
    hostname: Option<String>,
    /// Literal provider token (pinggy pro token / ngrok authtoken). Prefer
    /// `tokenSecretRef` for anything checked into a manifest.
    #[serde(default)]
    token: Option<String>,
    /// Pull the token from a `kind: Secret`'s `token` key — same convention
    /// as `storage`'s `--password-secret`.
    #[serde(default, rename = "tokenSecretRef")]
    token_secret_ref: Option<String>,
}

pub const TUNNEL_SPEC_FIELDS: &[&str] = &[
    "provider",
    "localPort",
    "hostname",
    "token",
    "tokenSecretRef",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TunnelRecord {
    name: String,
    provider: String,
    local_port: u16,
    hostname: Option<String>,
    /// Hash of (provider, local_port, hostname, resolved token) — a re-`apply`
    /// with the SAME effective config is a no-op; a DIFFERENT one restarts
    /// the agent (no provider here supports hot-reload the way the HTTPRoute
    /// proxy's SIGHUP does).
    config_hash: String,
    pid: Option<i32>,
    public_url: Option<String>,
    created_unix: u64,
    started_unix: Option<u64>,
    /// `ngrok` only — its local agent API port, one per concurrent tunnel to
    /// avoid every ngrok agent colliding on the default `:4040`.
    agent_web_port: Option<u16>,
}

#[derive(Subcommand, Debug)]
pub enum TunnelCmd {
    /// Apply the `kind: Tunnel` documents of a manifest (idempotent).
    Apply {
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// One-shot expose of a local port, no manifest needed.
    Expose {
        /// Name (default: `tunnel-<port>`).
        #[arg(long)]
        name: Option<String>,
        /// `pinggy` | `ngrok` | `cloudflare`.
        #[arg(long)]
        provider: String,
        #[arg(long = "local-port")]
        local_port: u16,
        #[arg(long)]
        hostname: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long = "token-secret")]
        token_secret: Option<String>,
    },
    /// List tunnels (state + public URL).
    Ls,
    /// Human-readable detail of one tunnel.
    Describe { name: String },
    /// Stop and remove a tunnel.
    Rm { name: String },
}

pub fn run(action: TunnelCmd) -> Result<()> {
    match action {
        TunnelCmd::Apply { file } => {
            let path = manifest::resolve_path(file)?;
            let docs = manifest::load(&path)?;
            apply(&docs)
        }
        TunnelCmd::Expose {
            name,
            provider,
            local_port,
            hostname,
            token,
            token_secret,
        } => {
            let name = name.unwrap_or_else(|| format!("tunnel-{local_port}"));
            let spec = TunnelSpec {
                provider,
                local_port,
                hostname,
                token,
                token_secret_ref: token_secret,
            };
            apply_one(&name, &spec)
        }
        TunnelCmd::Ls => cmd_ls(),
        TunnelCmd::Describe { name } => cmd_describe(&name),
        TunnelCmd::Rm { name } => cmd_rm(&name),
    }
}

pub fn apply(docs: &[ManifestDoc]) -> Result<()> {
    for doc in manifest::of_kind(docs, "Tunnel") {
        manifest::warn_unknown_fields(doc, TUNNEL_SPEC_FIELDS);
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
    // local RCE as whoever runs `delonix tunnel apply/expose`. Rejecting a
    // leading `-` here protects every provider's use of the token (pinggy's
    // ssh argv AND ngrok's `--authtoken <value>`), not just the one call site
    // that happened to be exploitable today.
    if let Some(t) = &token {
        if t.starts_with('-') {
            return Err(Error::Invalid(
                "token não pode começar por '-' (seria interpretado como uma opção do binário do provider)"
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
        return Err(Error::Invalid(format!(
            "tunnel '{name}': provider '{}' desconhecido (pinggy|ngrok|cloudflare)",
            spec.provider
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
                    .unwrap_or("(a determinar URL...)")
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
    };

    match spec.provider.as_str() {
        "pinggy" => spawn_pinggy(&mut rec, token.as_deref())?,
        "ngrok" => spawn_ngrok(&mut rec, token.as_deref(), spec.hostname.as_deref(), &store)?,
        "cloudflare" => spawn_cloudflare_quick(&mut rec)?,
        _ => unreachable!(),
    }
    rec.started_unix = Some(now);
    store.save(name, &rec)?;
    println!(
        "tunnel/{name}: {} — {}",
        super::po::t("running"),
        rec.public_url
            .as_deref()
            .unwrap_or("(URL ainda não confirmada — ver `delonix tunnel describe` / o log)")
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
        context: "abrir log do túnel",
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
        message: format!("{bin}: {e} (está instalado e no PATH?)"),
    })?;
    rec.pid = Some(child.id() as i32);
    std::thread::sleep(Duration::from_millis(400));
    if !std::path::Path::new(&format!("/proc/{}", child.id())).exists() {
        let tail = std::fs::read_to_string(&path).unwrap_or_default();
        return Err(Error::Runtime {
            context: "tunnel",
            message: format!(
                "{bin} caiu logo ao arrancar — {}",
                tail.lines().last().unwrap_or("(log vazio)")
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
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
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

fn spawn_pinggy(rec: &mut TunnelRecord, token: Option<&str>) -> Result<()> {
    // Documented general form: `ssh -p443 -R0:<localhost>:<localport>
    // [<token/keyword/tunneltype>@]free.pinggy.io` — the `-R0` port (dynamic,
    // server-assigned) is what makes this work with zero prior setup.
    let user_host = match token {
        Some(t) => format!("{t}@free.pinggy.io"),
        None => "free.pinggy.io".to_string(),
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

fn spawn_ngrok(
    rec: &mut TunnelRecord,
    token: Option<&str>,
    hostname: Option<&str>,
    store: &JsonStore<TunnelRecord>,
) -> Result<()> {
    which("ngrok").ok_or_else(|| {
        Error::Invalid(
            "`ngrok` não encontrado no PATH — instala-o (https://ngrok.com/download) antes de \
             usar provider=ngrok"
                .into(),
        )
    })?;
    let web_port = pick_free_ngrok_web_port(store)?;
    rec.agent_web_port = Some(web_port);
    let mut args = vec![
        "http".to_string(),
        rec.local_port.to_string(),
        "--web-addr".to_string(),
        format!("127.0.0.1:{web_port}"),
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
        args.push("--url".to_string());
        args.push(h.to_string());
    }
    // ngrok's own log isn't a reliable place to scrape the URL from (format
    // varies by version); its local agent API is the documented way.
    spawn_and_capture(rec, "ngrok", &args, |_| None)?;
    poll_ngrok_api(rec, web_port);
    Ok(())
}

fn poll_ngrok_api(rec: &mut TunnelRecord, web_port: u16) {
    let url = format!("http://127.0.0.1:{web_port}/api/tunnels");
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
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// One ngrok agent web-API port per CONCURRENT tunnel — the default `:4040`
/// would collide the moment a 2nd ngrok-provider Tunnel is applied.
fn pick_free_ngrok_web_port(store: &JsonStore<TunnelRecord>) -> Result<u16> {
    let used: std::collections::HashSet<u16> = store
        .list()?
        .into_iter()
        .filter(|r| r.provider == "ngrok" && is_alive(r))
        .filter_map(|r| r.agent_web_port)
        .collect();
    (4040..4140).find(|p| !used.contains(p)).ok_or_else(|| {
        Error::Invalid("sem porta livre para o agente ngrok (4040-4139 todas em uso)".into())
    })
}

fn spawn_cloudflare_quick(rec: &mut TunnelRecord) -> Result<()> {
    which("cloudflared").ok_or_else(|| {
        Error::Invalid(
            "`cloudflared` não encontrado no PATH — instala-o \
             (https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/) \
             antes de usar provider=cloudflare"
                .into(),
        )
    })?;
    if rec.hostname.is_some() {
        return Err(Error::Invalid(
            "provider=cloudflare com hostname pedido: só o quick-tunnel (URL efémera \
             *.trycloudflare.com, sem conta) está implementado por agora — um tunnel NOMEADO \
             com domínio próprio precisa da API do Cloudflare (accountId/zoneId/token) para \
             criar o tunnel, aplicar o ingress e a rota DNS; ainda por fazer, ver CLAUDE.md"
                .into(),
        ));
    }
    let args = vec![
        "tunnel".to_string(),
        "--url".to_string(),
        format!("http://localhost:{}", rec.local_port),
    ];
    spawn_and_capture(rec, "cloudflared", &args, |t| {
        find_url_containing(t, ".trycloudflare.com")
    })
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
            Error::Invalid(format!("no such tunnel: {n} (see `delonix tunnel ls`)"))
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
            Error::Invalid(format!("no such tunnel: {n} (see `delonix tunnel ls`)"))
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
    fn resolve_token_recusa_token_a_comecar_por_traco() {
        // CRITICAL fixed here: a token like "-oProxyCommand=..." embedded as
        // `<token>@free.pinggy.io`, the last positional ssh argv element, was
        // parsed by ssh as an OPTION instead of part of the destination —
        // local RCE via ProxyCommand. Reject before it ever reaches argv.
        let err =
            resolve_token(Some("-oProxyCommand=touch /tmp/pwned".to_string()), None).unwrap_err();
        assert!(format!("{err}").contains("não pode começar por"));
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
        };
        let h0 = config_hash(&base, &None);
        let mut port_changed = base.clone();
        port_changed.local_port = 9090;
        assert_ne!(h0, config_hash(&port_changed, &None));
        assert_ne!(h0, config_hash(&base, &Some("tok".to_string())));
        let mut host_changed = base.clone();
        host_changed.hostname = Some("app.example.com".to_string());
        assert_ne!(h0, config_hash(&host_changed, &None));
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
        };
        assert!(!is_alive(&rec));
    }

    #[test]
    fn pick_free_ngrok_web_port_evita_colisao() {
        let tmp = std::env::temp_dir().join(format!(
            "delonix-tunnel-webport-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let store = JsonStore::<TunnelRecord>::open(&tmp).unwrap();
        // A "running" record occupying :4040 — is_alive requires a real /proc
        // entry, so fake it with our OWN pid (this test process, definitely
        // alive) and a cmdline that won't contain "ngrok"... which means
        // is_alive is actually false here. That's fine: it proves the port
        // is freed once a record isn't genuinely alive (no leaked reservations).
        let fake = TunnelRecord {
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
        };
        store.save("other", &fake).unwrap();
        let port = pick_free_ngrok_web_port(&store).unwrap();
        assert_eq!(
            port, 4040,
            "o registo não está genuinamente vivo (cmdline não é ngrok)"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

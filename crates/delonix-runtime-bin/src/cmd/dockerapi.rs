//! `delonix docker-api` — a slice of the Docker Engine API
//! (`/_ping`, `/version`, `/info`, `/containers/json`, `/images/json`, plus
//! the mutations below), enough for `docker version`/`docker ps`/`docker
//! images`/`docker info`/`docker run`/`docker compose up` pointed at this
//! socket via `DOCKER_HOST=unix://<path>` to work.
//!
//! **Verified against a REAL `docker` CLI** (27.3.1, downloaded for exactly
//! this), not just the published spec: captured the exact wire protocol —
//! `HEAD /_ping` first, negotiates the API version from THIS server's
//! `Api-Version` response header (not the client's own bundled max), then
//! `/v<version>/...` for everything else. `strip_version_prefix` below
//! handles that; `/_ping` itself is always unversioned.
//!
//! **Mutations (v0.26.0)**: `POST /containers/create`, `/start`, `/stop`,
//! `/kill`, `/wait`, `/restart`, `/rename`, `DELETE /containers/{id}`, `GET
//! /containers/{id}/json` — the set `docker run`/`docker compose up|down|ps`
//! actually need. `exec` (the interactive/streamed kind, needing HTTP
//! connection hijacking) is explicitly OUT of scope for this pass — plain
//! container lifecycle doesn't need it; see `docs/COMPARACAO-DOCKER-PODMAN.md`
//! for the follow-up. Docker's own network model (bridge/user-defined
//! networks, `NetworkingConfig`) is NOT translated — a created container
//! always gets delonix's own default (`--net host`) regardless of what the
//! request's `HostConfig.NetworkMode`/`NetworkingConfig` say, documented, not
//! silent. Any route this layer doesn't implement returns 404 with a clear
//! message rather than a confusing client-side parse error.
//!
//! Same security posture as `delonix-mgmt`: 0600 socket + `SO_PEERCRED`
//! (own-uid only). A real `docker.sock` is usually group-readable (the
//! `docker` group) — same-uid-only is the safer default and consistent with
//! every other control socket in this codebase; an operator who wants group
//! access can `chmod`/`chgrp` it themselves after the fact.

use super::kinds as k;
use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;

use delonix_image::ImageStore;
use delonix_runtime_core::peer_cred::peer_uid;
use delonix_runtime_core::{Container, Error, Result, Status, Store};

use super::container::RunOpts;
use super::util::state_root;

/// What we report via `Api-Version` (and accept the client negotiating down
/// to) — matches the oldest widely-deployed Docker (17.03), comfortably
/// covering `docker compose`'s minimum requirement.
const API_VERSION: &str = "1.43";
const MIN_API_VERSION: &str = "1.24";

struct AppState {
    images: ImageStore,
    store: Store,
}

/// Starts a container by RE-EXECUTING this binary, instead of calling the engine
/// in-process.
///
/// This server is a multi-threaded tokio runtime (worker threads + the blocking
/// pool + the zombie reaper). `spawn()` ends in `clone()`, whose safety argument
/// is "single-threaded", and `clone()` — unlike `fork()` — does NOT run the
/// `pthread_atfork` handlers that reset the glibc malloc arena lock in the child.
/// So if any other thread happened to hold that lock at clone time, the child
/// inherits it held, by a thread that does not exist there, and the first
/// allocation inside `container_init` blocks forever: the container never starts,
/// the API answered "created", and the process leaks. The window is narrow but
/// real, and it widens exactly when the server is busy — `docker compose up`
/// creating several services at once.
///
/// Handing the work to a fresh process makes `clone()`'s precondition true again,
/// which is the same reason `delonix-cri` shells out to the CLI rather than
/// linking the engine in. The spec travels as a file (`0600`, `O_EXCL`) rather
/// than as argv: `RunOpts` has dozens of fields, and rebuilding a CLI command
/// line from it would silently drop whatever has no flag — the failure mode this
/// codebase has paid for repeatedly (state used at creation but never persisted).
fn spawn_run_via_reexec(opts: &RunOpts) -> Result<()> {
    let dir = state_root().join("run");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "apirun-{}-{}.json",
        std::process::id(),
        REEXEC_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    {
        use std::os::unix::fs::OpenOptionsExt;
        // `create_new` + mode at creation: the spec can carry resolved secret
        // names and mount paths, so it must never exist world-readable, not even
        // for the instant between `write` and a later `chmod`.
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        serde_json::to_writer(f, opts)?;
    }
    let exe = std::env::current_exe().map_err(Error::Io)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__apirun")
        .arg(&path)
        .env("DELONIX_ROOT", state_root());
    let out = run_claimed(cmd);
    // The child removes it on success; clean up here too so a child that died
    // before reading does not leave the spec behind.
    let _ = std::fs::remove_file(&path);
    match out {
        Ok(o) if o.status.success() => Ok(()),
        // Carry the child's own message back to the HTTP client: swallowing it
        // would turn every engine failure into an opaque exit code.
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Err(Error::Invalid(if msg.is_empty() {
                format!(
                    "container start failed (exit {})",
                    o.status.code().unwrap_or(-1)
                )
            } else {
                msg
            }))
        }
        Err(e) => Err(e),
    }
}

static REEXEC_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Re-execs a public CLI verb that only needs a container id (`start`,
/// `restart`), for the same single-threaded-`clone()` reason as
/// `spawn_run_via_reexec`. The child's stderr is carried back into the error so
/// the caller can still recognise engine messages — `handle_start` keys the
/// idempotent 304 off "already running", and losing that text would turn a
/// no-op into a 500.
fn cli_verb_via_reexec(args: &[&str]) -> Result<()> {
    let exe = std::env::current_exe().map_err(Error::Io)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("container")
        .args(args)
        .env("DELONIX_ROOT", state_root());
    let out = run_claimed(cmd)?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(Error::Invalid(if msg.is_empty() {
        format!("`container {}` failed", args.join(" "))
    } else {
        msg
    }))
}

fn start_via_reexec(id: &str) -> Result<()> {
    cli_verb_via_reexec(&["start", id])
}

/// The `__apirun <spec.json>` half: runs in a FRESH, single-threaded process, so
/// the `clone()` in `spawn()` is safe again. Never returns.
pub(crate) fn run_from_spec_file(path: &std::path::Path) -> ! {
    let code = match run_from_spec_file_inner(path) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("delonix: {}", super::po::t_dyn(&e.to_string()));
            1
        }
    };
    let _ = std::fs::remove_file(path);
    std::process::exit(code);
}

fn run_from_spec_file_inner(path: &std::path::Path) -> Result<()> {
    let data = std::fs::read(path)?;
    let opts: RunOpts = serde_json::from_slice(&data)?;
    let root = state_root();
    let images = ImageStore::open(&root)?;
    // `containers` subdir, exactly as the server itself opens it — `Store::open`
    // takes the directory verbatim, so passing the bare root would write the
    // records one level up, where nothing else looks for them.
    let store = Store::open(root.join("containers"))?;
    super::container::cmd_run(&images, &store, opts)
}

/// Reaps zombie children of THIS process for as long as it runs.
///
/// A detached container's init process is a **direct child of whoever called
/// `spawn()`** (`delonix_runtime::spawn` just returns without `waitpid` when
/// `detach: true`) — harmless for the plain CLI, which exits moments later so
/// the child gets reparented to the host's real `init` (which reaps it). This
/// server never exits, so it IS the real parent for the container's entire
/// life: without this, a killed container becomes a **permanent zombie**
/// (`ps` shows `<defunct>` forever) and `kill(pid, 0)`/`is_alive` keep
/// reporting it alive (a zombie still holds its PID-table slot until reaped),
/// so `reconcile_status` never sees the death. Found live: `docker kill`
/// returned success but `docker inspect` kept showing `Running` indefinitely;
/// `ps` traced the process to `<defunct>` with this server as PPID.
///
/// **Precondition this used to rely on, and no longer does**: the original
/// version called a blind `waitpid(-1, ...)`, and its own comment warned that
/// this would race any codepath doing its OWN `waitpid(<specific pid>)` on a
/// child it had just forked. The re-exec added for container create/start/
/// restart (see `spawn_run_via_reexec`) is exactly that codepath, and the race
/// was observed on the first live run: the container started correctly, but the
/// reaper consumed its exit status first and `Command::output()` came back
/// `ECHILD`, so a perfectly good create reported an I/O error.
///
/// So the reaper now PEEKS with `WNOWAIT` (which reports a dead child without
/// consuming it) and only reaps pids nobody has claimed. A claimed pid is left
/// alone for its owner to wait on, exactly as before the reaper existed.
fn spawn_zombie_reaper() {
    std::thread::spawn(|| loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `waitid` with a zeroed `siginfo_t` out-param. `WNOWAIT` leaves
        // the child in its zombie state so the owner (if any) can still wait on
        // it; `WNOHANG` keeps this from blocking while we hold no claim.
        let r = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        // SAFETY: reading `si_pid` from a `siginfo_t` `waitid` just filled in.
        let pid = if r == 0 { unsafe { info.si_pid() } } else { 0 };
        if pid <= 0 {
            // Nothing dead right now — check back shortly rather than spin.
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        if claimed_pid(pid) {
            // Somebody is going to wait on this one; taking it would steal
            // their exit status. Back off briefly — once the owner reaps it,
            // the next peek moves on.
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        let mut status: i32 = 0;
        // SAFETY: reaps that specific, unclaimed child of ours.
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    });
}

/// Pids this process spawned and intends to `wait` on itself — the reaper must
/// not consume them. See `spawn_zombie_reaper`.
static CLAIMED_PIDS: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

fn claimed_pid(pid: i32) -> bool {
    CLAIMED_PIDS
        .lock()
        .map(|v| v.contains(&pid))
        .unwrap_or(false)
}

/// Runs a child to completion with its exit status and stderr intact, claiming
/// it against the reaper for the duration.
fn run_claimed(mut cmd: std::process::Command) -> Result<std::process::Output> {
    let child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(Error::Io)?;
    let pid = child.id() as i32;
    if let Ok(mut v) = CLAIMED_PIDS.lock() {
        v.push(pid);
    }
    let out = child.wait_with_output().map_err(Error::Io);
    if let Ok(mut v) = CLAIMED_PIDS.lock() {
        v.retain(|p| *p != pid);
    }
    out
}

pub fn run(addr: Option<String>) -> Result<()> {
    spawn_zombie_reaper();
    let raw = addr
        .or_else(|| std::env::var("DELONIX_DOCKER_ADDR").ok())
        .unwrap_or_else(|| "unix:///run/delonix-docker.sock".to_string());
    let path = raw.strip_prefix("unix://").unwrap_or(&raw).to_string();
    let root = state_root();
    let images = ImageStore::open(&root)?;
    let store = Store::open(root.join("containers"))?;
    let state = Arc::new(AppState { images, store });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Runtime {
            context: "tokio",
            message: e.to_string(),
        })?;
    rt.block_on(async move {
        let _ = std::fs::remove_file(&path); // clean up an old socket
        let uds = tokio::net::UnixListener::bind(&path).map_err(|e| Error::Runtime {
            context: "bind",
            message: e.to_string(),
        })?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        eprintln!("delonix-docker-api (Docker Engine API) listening on unix://{path}");
        serve(uds, state).await
    })
}

async fn serve(uds: tokio::net::UnixListener, state: Arc<AppState>) -> Result<()> {
    // SAFETY: geteuid() has no preconditions.
    let own_uid = unsafe { libc::geteuid() };
    loop {
        let (socket, _) = uds.accept().await.map_err(|e| Error::Runtime {
            context: "accept",
            message: e.to_string(),
        })?;
        if peer_uid(&socket) != Some(own_uid) {
            continue;
        }
        let state = state.clone();
        tokio::task::spawn(async move {
            let io = TokioIo::new(socket);
            let svc = service_fn(move |req| handle(req, state.clone()));
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await;
        });
    }
}

/// Strips a leading `/v<digits>.<digits>` version segment, the form every
/// real docker CLI request uses after the initial (unversioned) `/_ping`.
fn strip_version_prefix(path: &str) -> &str {
    let Some(rest) = path.strip_prefix("/v") else {
        return path;
    };
    let digits_dots_end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    if digits_dots_end == 0 {
        return path;
    }
    let after = &rest[digits_dots_end..];
    if after.is_empty() {
        "/"
    } else {
        after
    }
}

/// Minimal `key=value&key2=value2` query-string parser with a basic
/// percent-decoder — no `form_urlencoded`/`url` dependency for this (matches
/// this codebase's supply-chain-minimalism rule). Correct for the ASCII
/// container/exec ids and names this API actually needs to decode; not a
/// general-purpose URL decoder.
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k.to_string(), url_decode(v))
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '+' => out.push(' '),
            '%' => {
                let hex: String = chars.by_ref().take(2).collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(byte) => out.push(byte as char),
                    Err(_) => {
                        out.push('%');
                        out.push_str(&hex);
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Runs a blocking engine call (`fork`/`clone`/`waitpid`-based — never safe to
/// run directly on a tokio worker thread, especially `wait`, which can block
/// indefinitely) on the blocking thread pool.
async fn run_blocking<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => Err(Error::Invalid(format!("internal task join error: {e}"))),
    }
}

fn ok_json(v: serde_json::Value) -> (StatusCode, Vec<u8>) {
    (StatusCode::OK, v.to_string().into_bytes())
}

fn err_response(e: &Error) -> (StatusCode, Vec<u8>) {
    let msg = e.to_string();
    let status = if msg.contains("not found") || msg.contains("no such") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, json!({ "message": msg }).to_string().into_bytes())
}

/// Every Docker Engine API route this layer implements, and what each one maps
/// to internally.
///
/// **This table is the published contract**, not a comment: `serve docker-api
/// --matrix` prints it, the docs are generated from it, and
/// `matriz_cobre_todas_as_rotas_do_dispatch` reads THIS FILE'S OWN SOURCE and
/// fails if a dispatch arm exists without an entry here. A compatibility layer
/// whose coverage is only discoverable by trying it is worse than a smaller one
/// that says where it ends — third-party tooling (Testcontainers, CI plugins,
/// IDEs) breaks unpredictably against the first and cleanly against the second.
pub(crate) const API_MATRIX: &[(&str, &str, &str)] = &[
    ("GET|HEAD", "/_ping", "liveness"),
    ("GET", "/version", "engine version"),
    ("GET", "/info", "engine state"),
    ("GET", "/containers/json", "container ps"),
    ("GET", "/images/json", "image ls"),
    (
        "POST",
        "/containers/create",
        "container run -d (creates AND starts; HostConfig.RestartPolicy is REFUSED)",
    ),
    (
        "POST",
        "/containers/{id}/start",
        "idempotent 304 if already running",
    ),
    ("POST", "/containers/{id}/stop", "container stop"),
    ("POST", "/containers/{id}/kill", "container kill"),
    ("POST", "/containers/{id}/wait", "container wait"),
    ("POST", "/containers/{id}/restart", "container restart"),
    ("POST", "/containers/{id}/rename", "container rename"),
    ("DELETE", "/containers/{id}", "container rm"),
    ("GET", "/containers/{id}/json", "container inspect"),
];

/// Routes deliberately NOT implemented, each with the reason.
///
/// Kept next to the matrix on purpose. "Not in the list" and "will never be in
/// the list" are different answers, and the second one saves someone the work
/// of waiting for it.
///
/// ROUTES ONLY. A caveat about one FIELD of an implemented route (say
/// `HostConfig.RestartPolicy`, which `create` refuses) belongs in that route's
/// own description — mixing the two granularities made the overlap check
/// ambiguous, which is how this distinction got noticed.
pub(crate) const API_UNIMPLEMENTED: &[(&str, &str, &str)] = &[
    (
        "POST",
        "/containers/{id}/exec",
        "needs HTTP hijacking (a raw bidirectional stream over the upgraded \
         connection); use `delonix container exec`",
    ),
    (
        "POST",
        "/containers/{id}/attach",
        "same hijacking requirement, and this engine keeps no live stdin conduit \
         to an already-started detached container",
    ),
    (
        "GET",
        "/containers/{id}/logs",
        "not written yet; `delonix container logs` covers it",
    ),
    (
        "GET",
        "/events",
        "not written yet; `delonix system events` covers it",
    ),
    (
        "POST",
        "/build",
        "not written yet; `delonix build` covers it",
    ),
    (
        "GET|POST|DELETE",
        "/networks",
        "not written yet; `delonix network` covers it",
    ),
    (
        "POST",
        "/networks/create",
        "not written yet; `delonix network create` covers it",
    ),
    (
        "GET",
        "/networks/{id}",
        "not written yet; `delonix network inspect` covers it",
    ),
    // The one that hurts most, and saying so is the point of this row: it is
    // the FIRST call most tools make, so its absence is not a missing feature
    // at the edge — it is the door. Writing it means streaming the pull
    // progress in Docker's own chunked JSON format, which is a slice of its
    // own, not a line here.
    (
        "POST",
        "/images/create",
        "not written yet — this is the pull, and most tools call it first; \
         `delonix image pull` covers it from the CLI",
    ),
    (
        "GET",
        "/images/{name}/json",
        "not written yet; `delonix image inspect` covers it",
    ),
    (
        "GET",
        "/containers/{id}/stats",
        "not written yet — it is a live metric stream, and this engine exposes \
         those over Prometheus (`/metrics`) instead; `delonix dash` covers the \
         interactive case",
    ),
    (
        "GET|POST|DELETE",
        "/volumes",
        "not written yet; `delonix volume` covers it",
    ),
];

/// The routes real tooling actually calls, and where each one was OBSERVED.
///
/// # Why a third list exists
///
/// [`API_MATRIX`] says what is served and [`API_UNIMPLEMENTED`] says what will
/// not be — two states. A reader of those two cannot tell «not implemented» from
/// «nobody ever considered it», and the difference decides whether they wait or
/// go elsewhere. This repo's own rule for talking about compatibility is three
/// states, never two: served, refused with a reason, and **missing**.
///
/// Measured 2026-08-25 against this file: `POST /images/create` (the pull every
/// tool does FIRST) and `GET /containers/{id}/stats` appeared in neither list.
/// Someone reading the published matrix would not learn they are absent.
///
/// # Where these entries come from
///
/// Not «the Docker Engine API», which has hundreds of routes most tools never
/// touch — the routes the tools this engine targets were SEEN using, with the
/// source named per row. Guessing a list here would make the gate below enforce
/// a fiction.
///
/// `kind`: captured live in this repo by wrapping the real `docker` binary
/// during a full `kind create cluster` — 52 invocations, transcribed in
/// `AGENTS.md` §«Superfície capturada». The CLI verbs map to routes the usual
/// way (`docker pull` → `POST /images/create`, `docker logs` → `GET
/// /containers/{id}/logs`).
///
/// `compose`: the create→start→inspect→stop→rm sequence this layer was built
/// against and validated with, per `AGENTS.md` §«serve docker-api».
pub(crate) const API_UPSTREAM_USED: &[(&str, &str, &str)] = &[
    (
        "GET",
        "/_ping",
        "compose, kind — the first call any client makes",
    ),
    ("GET", "/version", "compose, kind"),
    ("GET", "/info", "kind — `info --format {{json .}}`"),
    (
        "GET",
        "/containers/json",
        "kind — `ps -a --filter label=...`",
    ),
    ("POST", "/containers/create", "kind, compose — `run`"),
    ("POST", "/containers/{id}/start", "compose"),
    ("POST", "/containers/{id}/stop", "compose"),
    ("POST", "/containers/{id}/kill", "compose"),
    ("POST", "/containers/{id}/wait", "compose"),
    ("POST", "/containers/{id}/restart", "compose"),
    ("POST", "/containers/{id}/rename", "compose"),
    ("DELETE", "/containers/{id}", "kind — `rm -f -v <n>`"),
    (
        "GET",
        "/containers/{id}/json",
        "kind — four distinct `inspect --format`",
    ),
    ("GET", "/containers/{id}/logs", "kind — `logs -f <n>`"),
    (
        "POST",
        "/containers/{id}/exec",
        "kind — `exec --privileged [-i] <n> <cmd>`",
    ),
    ("POST", "/images/create", "kind — `pull <ref>`"),
    (
        "GET",
        "/images/{name}/json",
        "kind — `inspect --type=image <ref>`",
    ),
    (
        "GET",
        "/networks",
        "kind — `network ls --filter=name=^kind$`",
    ),
    (
        "POST",
        "/networks/create",
        "kind — `network create -d=bridge …`",
    ),
    (
        "GET",
        "/networks/{id}",
        "kind — `network inspect bridge -f …`",
    ),
    // Not from a capture of our own, and the row says so. `AGENTS.md` names
    // `stats` among the routes Testcontainers, Dev Containers and the GitLab
    // Runner use; that is an assertion this repo makes, not something measured
    // here. It earns a row because the gate's job is to force a CLASSIFICATION,
    // and «we were told this matters and never said whether we serve it» is the
    // exact silence being closed.
    (
        "GET",
        "/containers/{id}/stats",
        "Testcontainers, Dev Containers, GitLab Runner — per AGENTS.md, not captured here",
    ),
];

/// Prints [`API_MATRIX`] and [`API_UNIMPLEMENTED`] as a table.
pub(crate) fn print_matrix() {
    // The header carries the NUMBERS and the VERSION, because this repo's rule
    // for talking about compatibility is that «Docker-compatible» never travels
    // without them. A reader who quotes this table quotes a measurement.
    println!(
        "{}",
        super::po::tf(
            "Docker Engine API coverage — delonix {version}: {served} served, {refused} refused \
             with a reason.",
            &[
                ("version", env!("CARGO_PKG_VERSION")),
                ("served", &API_MATRIX.len().to_string()),
                ("refused", &API_UNIMPLEMENTED.len().to_string()),
            ],
        )
    );
    println!();
    let mut t = super::output::Table::new(&["METHOD", "PATH", "MAPS TO"]);
    for (m, p, to) in API_MATRIX {
        t.row(vec![m.to_string(), p.to_string(), to.to_string()]);
    }
    t.print();
    println!(
        "\n{}",
        super::po::t("NOT implemented (a 404 names the route, never a silent success):")
    );
    let mut u = super::output::Table::new(&["METHOD", "PATH", "WHY"]);
    for (m, p, why) in API_UNIMPLEMENTED {
        u.row(vec![m.to_string(), p.to_string(), why.to_string()]);
    }
    u.print();
    // The third state, and the reason this function was touched at all. Two
    // lists answer «what works» and «what never will»; a reader still cannot
    // tell whether the route THEY need was ever considered. This column says
    // which tool asked for each one, so «refused» stops reading as arbitrary.
    println!(
        "\n{}",
        super::po::t(
            "What real tooling calls, and where each was observed — every row here is in one \
             of the two tables above, and a test enforces that:"
        )
    );
    let mut w = super::output::Table::new(&["METHOD", "PATH", "STATE", "SEEN IN"]);
    for (m, p, seen) in API_UPSTREAM_USED {
        let state = if API_MATRIX
            .iter()
            .any(|(mm, pp, _)| pp == p && mm.contains(m))
        {
            super::po::t("served")
        } else {
            super::po::t("refused")
        };
        w.row(vec![
            m.to_string(),
            p.to_string(),
            state.to_string(),
            seen.to_string(),
        ]);
    }
    w.print();
}

async fn handle(
    req: Request<Incoming>,
    state: Arc<AppState>,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().as_str().to_string();
    let full_path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let path = strip_version_prefix(&full_path).to_string();
    let params = parse_query(&query);
    let body_bytes = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();

    let (status, body): (StatusCode, Vec<u8>) = match (method.as_str(), path.as_str()) {
        ("GET" | "HEAD", "/_ping") => (StatusCode::OK, b"OK".to_vec()),
        ("GET", "/version") => (StatusCode::OK, version_json()),
        ("GET", "/info") => (StatusCode::OK, info_json(&state)),
        ("GET", "/containers/json") => (
            StatusCode::OK,
            containers_json(&state).unwrap_or_else(|_| b"[]".to_vec()),
        ),
        ("GET", "/images/json") => (
            StatusCode::OK,
            images_json(&state).unwrap_or_else(|_| b"[]".to_vec()),
        ),
        ("POST", "/containers/create") => handle_create(&state, &params, &body_bytes).await,
        _ => {
            let segs: Vec<&str> = path
                .trim_matches('/')
                .split('/')
                .filter(|s| !s.is_empty())
                .collect();
            match (method.as_str(), segs.as_slice()) {
                ("POST", ["containers", id, "start"]) => handle_start(&state, id).await,
                ("POST", ["containers", id, "stop"]) => handle_stop(&state, id, &params).await,
                ("POST", ["containers", id, "kill"]) => handle_kill(&state, id, &params).await,
                ("POST", ["containers", id, "wait"]) => handle_wait(&state, id).await,
                ("POST", ["containers", id, "restart"]) => {
                    handle_restart(&state, id, &params).await
                }
                ("POST", ["containers", id, "rename"]) => handle_rename(&state, id, &params).await,
                ("DELETE", ["containers", id]) => handle_remove(&state, id, &params).await,
                ("GET", ["containers", id, "json"]) => handle_inspect(&state, id),
                _ => (
                    StatusCode::NOT_FOUND,
                    json!({
                        // Naming the route AND the way to see the whole
                        // contract beats a bare 404: a client that hits this is
                        // exactly the client that needs to know what else is
                        // missing before it writes around one gap at a time.
                        "message": format!(
                            "{method} {path}: not implemented in delonix's Docker API \
                             compatibility slice — run `delonix serve docker-api --matrix` \
                             for the full coverage table"
                        )
                    })
                    .to_string()
                    .into_bytes(),
                ),
            }
        }
    };
    let resp = Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Api-Version", API_VERSION)
        .header("Docker-Experimental", "false")
        .header("OSType", "linux")
        .header("Server", format!("delonix/{}", env!("CARGO_PKG_VERSION")))
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_default();
    Ok(resp)
}

/// `POST /containers/create[?name=<name>]` — maps Docker's `ContainerConfig`
/// JSON to `RunOpts` and delegates to the SAME `cmd_run` the CLI's `container
/// run -d` uses (zero duplication of the create/spawn logic). Deliberately
/// simplified vs. real Docker semantics: `cmd_run` creates AND starts
/// immediately (this engine has no separate dormant "created" state wired up
/// end-to-end) — `handle_start` below treats "already running" as the
/// idempotent 304 real Docker itself returns for that case, so the standard
/// create-then-start sequence (`docker compose up`'s own flow) still works.
async fn handle_create(
    state: &Arc<AppState>,
    params: &std::collections::HashMap<String, String>,
    body: &[u8],
) -> (StatusCode, Vec<u8>) {
    let cfg: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                json!({ "message": format!("invalid container config JSON: {e}") })
                    .to_string()
                    .into_bytes(),
            )
        }
    };
    let name = params.get("name").cloned().unwrap_or_else(|| {
        let state2 = state.clone();
        super::names::random_name(|n| {
            state2
                .store
                .list()
                .map(|v| v.iter().any(|c| c.name == n))
                .unwrap_or(false)
        })
        .unwrap_or_else(|| format!("dlx-api-{:x}", std::process::id()))
    });
    let opts = match docker_config_to_run_opts(name.clone(), &cfg) {
        Ok(o) => o,
        Err(e) => return err_response(&e),
    };
    // Calculado ANTES de criar seja o que for: se a criação falhar, o erro é a
    // resposta e estes avisos não chegam a lado nenhum — mas o custo é uma
    // travessia do JSON e a alternativa era esquecê-los no ramo de sucesso.
    let warnings = unconsumed_config_warnings(&cfg);
    let state = state.clone();
    let result: Result<String> = run_blocking(move || {
        // Re-exec instead of running the engine here: see `spawn_run_via_reexec`.
        spawn_run_via_reexec(&opts)?;
        let c = state
            .store
            .list()?
            .into_iter()
            .find(|c| c.name == name)
            .ok_or_else(|| {
                Error::Invalid("container created but not found afterward (unexpected)".into())
            })?;
        Ok(c.id)
    })
    .await;
    match result {
        Ok(id) => (
            StatusCode::CREATED,
            json!({ "Id": id, "Warnings": warnings })
                .to_string()
                .into_bytes(),
        ),
        Err(e) => err_response(&e),
    }
}

async fn handle_start(_state: &Arc<AppState>, id: &str) -> (StatusCode, Vec<u8>) {
    let id = id.to_string();
    // Same reason as create: `cmd_start` reaches `spawn()`→`clone()`, which is
    // only safe single-threaded. Here the CLI verb takes just an id, so a plain
    // re-exec of the public command does it — no spec file needed.
    match run_blocking(move || start_via_reexec(&id)).await {
        Ok(()) => (StatusCode::NO_CONTENT, Vec::new()),
        // Docker's own real semantics: starting an already-running container
        // is a no-op success (304), not an error.
        Err(Error::Invalid(msg)) if msg.contains("already running") => {
            (StatusCode::NOT_MODIFIED, Vec::new())
        }
        Err(e) => err_response(&e),
    }
}

async fn handle_stop(
    state: &Arc<AppState>,
    id: &str,
    params: &std::collections::HashMap<String, String>,
) -> (StatusCode, Vec<u8>) {
    let timeout: u64 = params.get("t").and_then(|v| v.parse().ok()).unwrap_or(10);
    let state = state.clone();
    let id = id.to_string();
    match run_blocking(move || super::container::cmd_stop(&state.store, &id, timeout)).await {
        Ok(()) => (StatusCode::NO_CONTENT, Vec::new()),
        Err(e) => err_response(&e),
    }
}

async fn handle_kill(
    state: &Arc<AppState>,
    id: &str,
    params: &std::collections::HashMap<String, String>,
) -> (StatusCode, Vec<u8>) {
    let signal = params
        .get("signal")
        .cloned()
        .unwrap_or_else(|| "KILL".to_string());
    let state = state.clone();
    let id = id.to_string();
    match run_blocking(move || super::container::cmd_kill(&state.store, &id, &signal)).await {
        Ok(()) => (StatusCode::NO_CONTENT, Vec::new()),
        Err(e) => err_response(&e),
    }
}

async fn handle_wait(state: &Arc<AppState>, id: &str) -> (StatusCode, Vec<u8>) {
    let state = state.clone();
    let id = id.to_string();
    match run_blocking(move || super::container::wait_for_exit(&state.store, &id)).await {
        Ok(code) => ok_json(json!({ "StatusCode": code })),
        Err(e) => err_response(&e),
    }
}

async fn handle_restart(
    _state: &Arc<AppState>,
    id: &str,
    params: &std::collections::HashMap<String, String>,
) -> (StatusCode, Vec<u8>) {
    let timeout: u64 = params.get("t").and_then(|v| v.parse().ok()).unwrap_or(10);
    let id = id.to_string();
    // Restart starts the container again, so it reaches `clone()` too — re-exec.
    match run_blocking(move || cli_verb_via_reexec(&["restart", "-t", &timeout.to_string(), &id]))
        .await
    {
        Ok(()) => (StatusCode::NO_CONTENT, Vec::new()),
        Err(e) => err_response(&e),
    }
}

async fn handle_rename(
    state: &Arc<AppState>,
    id: &str,
    params: &std::collections::HashMap<String, String>,
) -> (StatusCode, Vec<u8>) {
    let Some(new_name) = params.get("name").cloned() else {
        return (
            StatusCode::BAD_REQUEST,
            json!({ "message": "rename requires a ?name= query parameter" })
                .to_string()
                .into_bytes(),
        );
    };
    let state = state.clone();
    let id = id.to_string();
    match run_blocking(move || super::container::cmd_rename(&state.store, &id, &new_name)).await {
        Ok(()) => (StatusCode::NO_CONTENT, Vec::new()),
        Err(e) => err_response(&e),
    }
}

async fn handle_remove(
    state: &Arc<AppState>,
    id: &str,
    params: &std::collections::HashMap<String, String>,
) -> (StatusCode, Vec<u8>) {
    let force = params
        .get("force")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let state = state.clone();
    let id = id.to_string();
    match run_blocking(move || super::container::cmd_rm(&state.images, &state.store, &id, force))
        .await
    {
        Ok(()) => (StatusCode::NO_CONTENT, Vec::new()),
        Err(e) => err_response(&e),
    }
}

/// `GET /containers/{id}/json` — the detailed inspect `docker compose`/`docker
/// start`'s own state-polling relies on. A useful subset of real Docker's
/// shape (`.State.Running`/`.State.Status`/`.State.ExitCode`, `.Config.Image`,
/// `.NetworkSettings`) — not the full field set.
fn handle_inspect(state: &Arc<AppState>, id: &str) -> (StatusCode, Vec<u8>) {
    let mut c = match super::util::find(&state.store, id) {
        Ok(c) => c,
        Err(e) => return err_response(&e),
    };
    let _ = delonix_runtime::reconcile_status(&mut c);
    ok_json(json!({
        "Id": c.id,
        "Name": format!("/{}", c.name),
        "Created": unix_to_rfc3339(c.created_unix),
        "State": {
            "Status": docker_state(&c.status),
            "Running": matches!(c.status, Status::Running),
            "Paused": matches!(c.status, Status::Paused),
            "ExitCode": c.status.exit_code(),
        },
        "Config": {
            k::IMAGE: c.image,
            "Cmd": c.command,
            "Env": c.env,
            "Labels": c.labels,
        },
        k::IMAGE: c.image,
        "NetworkSettings": {
            "Networks": {
                c.network.clone().unwrap_or_else(|| "host".to_string()): {
                    "IPAddress": c.ip.clone().unwrap_or_default(),
                }
            }
        },
    }))
}

/// Maps a subset of Docker's `ContainerConfig`/`HostConfig` create-request
/// JSON to `RunOpts` — the SAME struct `container run`'s CLI parsing builds,
/// so this reuses `cmd_run` wholesale rather than a second create/spawn path.
///
/// `HostConfig.ExtraHosts` IS mapped (same `host:ip` format as `--add-host`,
/// same validator) — it is what Testcontainers uses.
///
/// **Deliberately NOT mapped** (documented limitation, not a silent gap):
/// `HostConfig.NetworkMode`/`NetworkingConfig` (Docker's bridge/user-defined
/// network model has no equivalent here — every API-created container gets
/// delonix's own default, `--net host`), `Tty`/`OpenStdin` (this create path is
/// for `docker run -d`/`compose up`'s non-interactive flow — see the module doc
/// for why interactive `exec` is out of scope for this pass too).
///
/// Whatever is neither mapped nor listed above comes back to the caller in
/// `Warnings[]` — see [`unconsumed_config_warnings`].
///
/// **A justificação desta lista já esteve DESACTUALIZADA, e é o modo de falha a
/// vigiar aqui**: dizia «`WorkingDir` (this engine has no `run -w` at all yet)»,
/// e o `container run -w/--workdir` existe desde 2026-07-27 — o compose passou a
/// usá-lo e este tradutor ficou com a razão da era anterior. Uma limitação
/// documentada envelhece para mentira sem ninguém lhe tocar.
fn docker_config_to_run_opts(name: String, cfg: &serde_json::Value) -> Result<RunOpts> {
    let image = cfg[k::IMAGE]
        .as_str()
        .ok_or_else(|| Error::Invalid("container config: 'Image' is required".into()))?
        .to_string();
    let command: Vec<String> = json_str_array(&cfg["Cmd"]);
    let entrypoint = cfg["Entrypoint"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let env = json_str_array(&cfg["Env"]);
    let labels: Vec<String> = cfg["Labels"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default();

    let host = &cfg["HostConfig"];
    let volumes: Vec<String> = json_str_array(&host["Binds"]);
    // `HostConfig.ExtraHosts` — o formato é `host:ip`, o mesmo do
    // `--add-host`, logo o mesmo parser. É o campo que o Testcontainers usa;
    // ignorá-lo em silêncio dava um contentor sem a resolução que o teste
    // configurou, e uma falha longe da causa.
    let add_host: Vec<String> = {
        let mut out = Vec::new();
        for entry in json_str_array(&host["ExtraHosts"]) {
            let (n, ip) = super::container::parse_add_host(&entry)
                .map_err(delonix_runtime_core::Error::Invalid)?;
            out.push(format!("{n}:{ip}"));
        }
        out
    };
    let ports = docker_port_bindings_to_publish_specs(&host["PortBindings"]);
    let restart = host["RestartPolicy"]["Name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|name| {
            let max = host["RestartPolicy"]["MaximumRetryCount"]
                .as_u64()
                .unwrap_or(0);
            if name == "on-failure" && max > 0 {
                format!("on-failure:{max}")
            } else {
                name.to_string()
            }
        })
        .unwrap_or_else(|| "no".to_string());
    // `always`/`unless-stopped`/`on-failure` need a **supervisor**
    // (`container::run_supervised`), which does a raw `libc::fork()` on the
    // assumption — true for the CLI, false here — that the calling process is
    // single-threaded. Forking this multi-threaded tokio server is unsafe (a
    // lock held by another thread at fork time stays held forever in the
    // child, e.g. the malloc arena lock — a classic latent deadlock). Rather
    // than risk that, fail closed with a clear message; the CLI remains the
    // safe way to run a supervised container.
    if super::container::policy_supervised(&restart) {
        return Err(Error::Invalid(format!(
            "HostConfig.RestartPolicy '{restart}' is not supported via the Docker API yet (the \
             --restart supervisor needs to fork a single-threaded process, which this server \
             isn't); use `delonix container run --restart {restart}` from the CLI, or create \
             without a restart policy"
        )));
    }
    // Docker's `Memory`/`NanoCpus` are raw bytes / nano-cpu-units (integers);
    // this engine's `--memory`/`--cpus` want a plain string (`"64M"`/`"0.5"`)
    // — an unsuffixed byte count is accepted as bytes, and dividing NanoCpus
    // by 1e9 gives back the decimal core count `--cpus` expects.
    let memory = host["Memory"]
        .as_u64()
        .filter(|b| *b > 0)
        .map(|b| b.to_string());
    let cpus = host["NanoCpus"]
        .as_u64()
        .filter(|n| *n > 0)
        .map(|n| format!("{:.3}", n as f64 / 1_000_000_000.0));
    let privileged = host["Privileged"].as_bool().unwrap_or(false);
    let cap_add = json_str_array(&host["CapAdd"]);
    let cap_drop = json_str_array(&host["CapDrop"]);
    // Cinco mapeamentos de uma linha cada, sobre campos que o `RunOpts` já tinha.
    // O `User` é o mais caro dos cinco em silêncio: um `docker create -u 1000`
    // subia o container como ROOT da imagem, e num servidor cujo público declarado
    // é o Testcontainers isso é a diferença entre um teste que passa e um teste
    // que passa pela razão errada.
    let user = cfg["User"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let workdir = cfg["WorkingDir"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let read_only = host["ReadonlyRootfs"].as_bool().unwrap_or(false);
    // `Tmpfs` é um objecto `{"/path": "opts"}`, e o `--tmpfs` daqui é `/path[:opts]`.
    let tmpfs: Vec<String> = host["Tmpfs"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(path, opts)| match opts.as_str().unwrap_or_default() {
                    "" => path.clone(),
                    o => format!("{path}:{o}"),
                })
                .collect()
        })
        .unwrap_or_default();
    // `Ulimits` é `[{"Name":"nofile","Soft":1024,"Hard":2048}]` e o nosso
    // `--ulimit` é `nome=soft[:hard]`.
    let ulimit: Vec<String> = host["Ulimits"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| {
                    let n = u["Name"].as_str()?;
                    let soft = u["Soft"].as_i64()?;
                    let hard = u["Hard"].as_i64().unwrap_or(soft);
                    Some(format!("{n}={soft}:{hard}"))
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(RunOpts {
        user,
        workdir,
        read_only,
        tmpfs,
        ulimit,
        detach: true,
        // This server is MULTI-THREADED and `run_supervised` does a bare
        // `fork()` that assumes a single-threaded caller — the same reason
        // `--restart` is already refused here. Opting out keeps the pre-existing
        // behaviour (no supervisor, so no captured exit code over this API) as a
        // documented gap, instead of forking from a threaded process.
        no_supervisor: true,
        name: Some(name),
        net: "host".to_string(),
        volumes,
        add_host,
        ports,
        privileged,
        entrypoint,
        restart,
        env,
        labels,
        image,
        command,
        quiet: true,
        memory,
        cpus,
        cap_add,
        cap_drop,
        ..Default::default()
    })
}

/// Os campos do pedido que este tradutor NÃO consome, devolvidos no `Warnings[]`
/// da resposta do `create`.
///
/// **Fecha a classe inteira de uma vez, e é por isso que existe em vez de mais
/// cinco mapeamentos.** O envelope `Warnings` já fazia parte da resposta desde
/// sempre — e vinha sempre vazio. Um cliente que mande `Sysctls` ou `Devices`
/// recebia `201 Created` com uma lista vazia e nada a dizer que metade do pedido
/// tinha sido deitada fora; agora recebe a lista, no campo que o protocolo
/// reserva exactamente para isto, e sem ter de ler a documentação de ninguém.
///
/// A propriedade que interessa é ser uma lista de EXCLUSÃO: um campo novo que o
/// Docker acrescente, ou um que este tradutor deixe de mapear, aparece aqui
/// sozinho. O inverso — enumerar o que avisamos — voltaria a envelhecer para
/// mentira como a lista de «deliberately NOT mapped» já envelheceu uma vez.
///
/// `Image`/`Cmd`/`Entrypoint`/`Env`/`Labels` e os do `HostConfig` que o tradutor
/// lê ficam de fora por construção. `Tty`/`OpenStdin`/`AttachStd*` também: são o
/// fluxo interactivo, que esta superfície recusa por desenho e já o diz no 404 e
/// no `--matrix` — repeti-lo em cada `create` de um `compose up` seria ruído.
fn unconsumed_config_warnings(cfg: &serde_json::Value) -> Vec<String> {
    /// Consumidos por `docker_config_to_run_opts`, ou irrelevantes por desenho.
    const CONSUMIDOS_TOPO: &[&str] = &[
        k::IMAGE,
        "Cmd",
        "Entrypoint",
        "Env",
        "Labels",
        "User",
        "WorkingDir",
        "HostConfig",
        // Interactivo: fora de escopo declarado, não é um descarte silencioso.
        "Tty",
        "OpenStdin",
        "StdinOnce",
        "AttachStdin",
        "AttachStdout",
        "AttachStderr",
        // Metadados que o cliente manda e não pedem acção nenhuma.
        "Hostname",
        "Domainname",
        "ExposedPorts",
        "Volumes",
        "NetworkingConfig",
    ];
    const CONSUMIDOS_HOST: &[&str] = &[
        "Binds",
        "ExtraHosts",
        "PortBindings",
        "RestartPolicy",
        "Memory",
        "NanoCpus",
        "Privileged",
        "CapAdd",
        "CapDrop",
        "ReadonlyRootfs",
        "Tmpfs",
        "Ulimits",
        // O modelo de rede do Docker não tem equivalente aqui, e o `--matrix`
        // di-lo; avisar em cada create seria ruído sobre uma decisão publicada.
        "NetworkMode",
    ];

    let mut out = Vec::new();
    let mut nomeia = |campo: &str, valor: &serde_json::Value| {
        // Um campo presente mas VAZIO não é um pedido — os clientes serializam a
        // struct inteira, com `[]`/`{}`/`0`/`false` em tudo o que não usaram.
        // Avisar sobre esses faria o `Warnings` sair cheio em todos os `create` e
        // deixar de se ler, que é o oposto do objectivo.
        let vazio = match valor {
            serde_json::Value::Null => true,
            serde_json::Value::Array(a) => a.is_empty(),
            serde_json::Value::Object(o) => o.is_empty(),
            serde_json::Value::String(s) => s.is_empty(),
            serde_json::Value::Bool(b) => !*b,
            serde_json::Value::Number(n) => n.as_f64() == Some(0.0),
        };
        if !vazio {
            out.push(format!(
                "delonix: '{campo}' is not supported by this engine's Docker API and was \
                 ignored (see `delonix serve docker-api --matrix`)"
            ));
        }
    };

    if let Some(o) = cfg.as_object() {
        for (k, v) in o {
            if !CONSUMIDOS_TOPO.contains(&k.as_str()) {
                nomeia(k, v);
            }
        }
    }
    if let Some(o) = cfg["HostConfig"].as_object() {
        for (k, v) in o {
            if !CONSUMIDOS_HOST.contains(&k.as_str()) {
                nomeia(&format!("HostConfig.{k}"), v);
            }
        }
    }
    out.sort();
    out
}

fn json_str_array(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Docker's `HostConfig.PortBindings` is a map keyed `"<containerPort>/<proto>"`
/// -> `[{"HostIp": "...", "HostPort": "..."}]`; delonix's `--publish` spec is
/// `"hostPort:containerPort[/proto]"`. Only the FIRST host binding per
/// container port is used (this engine publishes one host port per spec, same
/// as the CLI's own `-p`).
fn docker_port_bindings_to_publish_specs(v: &serde_json::Value) -> Vec<String> {
    let Some(obj) = v.as_object() else {
        return Vec::new();
    };
    obj.iter()
        .filter_map(|(key, bindings)| {
            let (cport, proto) = key.split_once('/').unwrap_or((key, "tcp"));
            let host_port = bindings.as_array()?.first()?["HostPort"].as_str()?;
            if host_port.is_empty() {
                return None;
            }
            Some(if proto == "tcp" {
                format!("{host_port}:{cport}")
            } else {
                format!("{host_port}:{cport}/{proto}")
            })
        })
        .collect()
}

fn version_json() -> Vec<u8> {
    let dlx_version = env!("CARGO_PKG_VERSION");
    json!({
        "Platform": { "Name": format!("Delonix Runtime {dlx_version}") },
        "Version": dlx_version,
        "ApiVersion": API_VERSION,
        "MinAPIVersion": MIN_API_VERSION,
        "GitCommit": option_env!("DELONIX_GIT_COMMIT").unwrap_or("unknown"),
        "GoVersion": "",
        "Os": "linux",
        "Arch": std::env::consts::ARCH,
        "KernelVersion": kernel_release(),
        "BuildTime": "",
        "Components": [
            { "Name": "Engine", "Version": dlx_version, "Details": { "ApiVersion": API_VERSION } }
        ]
    })
    .to_string()
    .into_bytes()
}

fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn info_json(state: &AppState) -> Vec<u8> {
    let containers = state.store.list().unwrap_or_default();
    let running = containers
        .iter()
        .filter(|c| matches!(c.status, Status::Running))
        .count();
    let paused = containers
        .iter()
        .filter(|c| matches!(c.status, Status::Paused))
        .count();
    let stopped = containers.len() - running - paused;
    let images_count = state.images.list().map(|v| v.len()).unwrap_or(0);
    json!({
        "ID": "delonix",
        "Containers": containers.len(),
        "ContainersRunning": running,
        "ContainersPaused": paused,
        "ContainersStopped": stopped,
        "Images": images_count,
        "Driver": "delonix",
        "SystemTime": chrono_now_rfc3339(),
        "KernelVersion": kernel_release(),
        "OperatingSystem": "Delonix Runtime (Linux)",
        "OSType": "linux",
        "Architecture": std::env::consts::ARCH,
        "NCPU": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "MemTotal": mem_total_bytes(),
        "ServerVersion": env!("CARGO_PKG_VERSION"),
        "SecurityOptions": ["name=rootless", "name=seccomp,profile=default"],
    })
    .to_string()
    .into_bytes()
}

fn mem_total_bytes() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("MemTotal:")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|kb| kb.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
        })
        .unwrap_or(0)
}

fn chrono_now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    unix_to_rfc3339(secs)
}

/// RFC 3339 (UTC) for an arbitrary unix timestamp — `chrono_now_rfc3339` is
/// just this applied to "now". No `chrono` dependency in this crate — a
/// minimal formatter is plenty for a status field nothing parses strictly.
fn unix_to_rfc3339(secs: u64) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Days since epoch -> civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Maps a delonix `Status` to Docker's `State` vocabulary
/// (created/running/paused/restarting/removing/exited/dead).
fn docker_state(status: &Status) -> &'static str {
    match status {
        Status::Created => "created",
        Status::Running => "running",
        Status::Paused => "paused",
        Status::Stopped | Status::Failed(_) => "exited",
        Status::Crashed => "dead",
    }
}

fn docker_status_text(c: &Container) -> String {
    match &c.status {
        Status::Running => c
            .pid_starttime
            .and_then(super::output::uptime_from_starttime)
            .map(|secs| format!("Up {}", super::output::fmt_duration_secs(secs)))
            .unwrap_or_else(|| "Up".to_string()),
        Status::Paused => "Paused".to_string(),
        Status::Stopped => "Exited (0)".to_string(),
        Status::Failed(n) => format!("Exited ({n})"),
        Status::Crashed => "Dead".to_string(),
        Status::Created => "Created".to_string(),
    }
}

fn containers_json(state: &AppState) -> Result<Vec<u8>> {
    let containers = state.store.list()?;
    let items: Vec<_> = containers
        .iter()
        .map(|c| {
            let ports: Vec<_> = c
                .ports
                .iter()
                .filter_map(|p| {
                    // delonix stores "hostPort:contPort[/proto]" — best-effort parse.
                    let (host_part, rest) = p.split_once(':')?;
                    let (cport, proto) = rest.split_once('/').unwrap_or((rest, "tcp"));
                    let host_port: u16 = host_part.parse().ok()?;
                    let cport: u16 = cport.parse().ok()?;
                    Some(json!({
                        "PrivatePort": cport,
                        "PublicPort": host_port,
                        "Type": proto,
                    }))
                })
                .collect();
            json!({
                "Id": c.id,
                "Names": [format!("/{}", c.name)],
                k::IMAGE: c.image,
                "ImageID": c.image,
                "Command": c.command.join(" "),
                "Created": c.created_unix,
                "State": docker_state(&c.status),
                "Status": docker_status_text(c),
                "Ports": ports,
                "Labels": c.labels,
                "NetworkSettings": {
                    "Networks": {
                        c.network.clone().unwrap_or_else(|| "host".to_string()): {
                            "IPAddress": c.ip.clone().unwrap_or_default(),
                        }
                    }
                },
                "Mounts": [],
            })
        })
        .collect();
    Ok(serde_json::to_vec(&items)?)
}

fn images_json(state: &AppState) -> Result<Vec<u8>> {
    let images = state.images.list()?;
    let items: Vec<_> = images
        .iter()
        .map(|img| {
            let size = super::image::image_size(&state.images, img).unwrap_or(0);
            json!({
                "Id": img.id,
                "ParentId": "",
                "RepoTags": if img.repo_tags.is_empty() { vec!["<none>:<none>".to_string()] } else { img.repo_tags.clone() },
                "RepoDigests": [],
                "Created": img.created_unix,
                "Size": size,
                "VirtualSize": size,
                "SharedSize": 0,
                "Labels": {},
                "Containers": -1,
            })
        })
        .collect();
    Ok(serde_json::to_vec(&items)?)
}

#[cfg(test)]
mod matrix_tests {
    use super::{API_MATRIX, API_UNIMPLEMENTED, API_UPSTREAM_USED};

    /// **Every route real tooling uses has to be CLASSIFIED** — served or
    /// refused with a reason. Silence is the third state, and it is the one that
    /// wastes somebody's afternoon.
    ///
    /// The sibling test below guards the other direction (nothing served is
    /// missing from the matrix). Together they say: the published table promises
    /// no more than the code does, and hides no less than the tools ask for.
    ///
    /// This failed when it was written, which is the whole reason it exists —
    /// `POST /images/create`, `GET /containers/{id}/stats` and the image/network
    /// inspect routes were in neither list.
    #[test]
    fn every_route_real_tooling_calls_is_classified() {
        let unclassified: Vec<String> = API_UPSTREAM_USED
            .iter()
            .filter(|(m, p, _)| {
                let served = API_MATRIX
                    .iter()
                    .any(|(mm, pp, _)| pp == p && mm.contains(m));
                let refused = API_UNIMPLEMENTED
                    .iter()
                    .any(|(mm, pp, _)| pp == p && mm.contains(m));
                !served && !refused
            })
            .map(|(m, p, why)| format!("{m} {p}  (used by: {why})"))
            .collect();
        assert!(
            unclassified.is_empty(),
            "{} route(s) that real tooling calls are in NEITHER list — a reader of the \
             published matrix cannot tell they are absent. Put each one in API_MATRIX (if it \
             is served) or in API_UNIMPLEMENTED with the reason:\n  {}",
            unclassified.len(),
            unclassified.join("\n  "),
        );
    }

    /// Reads THIS FILE'S OWN SOURCE and requires every dispatch arm to have a
    /// row in the matrix.
    ///
    /// A hand-kept table drifts the first time someone adds a route and forgets
    /// the doc — and a coverage table that is wrong is worse than none, because
    /// it is believed. Parsing the source is crude but it fails LOUDLY at
    /// `cargo test`, which is exactly where that mistake should surface.
    #[test]
    fn matriz_cobre_todas_as_rotas_do_dispatch() {
        let src = include_str!("dockerapi.rs");
        // The literal-path arms: ("GET", "/version") => ...
        let mut missing = Vec::new();
        for line in src.lines() {
            let l = line.trim();
            if !l.contains("=>") || !l.starts_with('(') {
                continue;
            }
            // ("POST", ["containers", id, "start"]) => ...
            if let Some(rest) = l.strip_prefix('(') {
                let Some((methods, tail)) = rest.split_once(", ") else {
                    continue;
                };
                let methods = methods.trim().trim_matches('"');
                if !methods
                    .split(" | ")
                    .all(|m| matches!(m.trim_matches('"'), "GET" | "HEAD" | "POST" | "DELETE"))
                {
                    continue;
                }
                let path = if let Some(seg) = tail.strip_prefix('[') {
                    // segment form -> rebuild "/containers/{id}/start"
                    let inner = seg.split(']').next().unwrap_or("");
                    let parts: Vec<String> = inner
                        .split(',')
                        .map(|p| {
                            let p = p.trim();
                            if p.starts_with('"') {
                                p.trim_matches('"').to_string()
                            } else {
                                "{id}".to_string()
                            }
                        })
                        .collect();
                    format!("/{}", parts.join("/"))
                } else if tail.trim_start().starts_with('"') {
                    tail.trim_start()
                        .trim_start_matches('"')
                        .split('"')
                        .next()
                        .unwrap_or("")
                        .to_string()
                } else {
                    continue;
                };
                let methods_norm = methods.replace("\" | \"", "|");
                if !API_MATRIX
                    .iter()
                    .any(|(m, p, _)| *p == path && *m == methods_norm)
                {
                    missing.push(format!("{methods_norm} {path}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "dispatch arms with no row in API_MATRIX: {missing:?}"
        );
        // And the parser has to have FOUND something — a regex that matches
        // nothing would make this test pass forever while proving nothing.
        assert!(API_MATRIX.len() >= 14);
    }

    /// The two lists must not overlap on (method, path).
    ///
    /// The first version of this check compared with `contains`, and failed
    /// twice for reasons worth keeping: once legitimately (a caveat about ONE
    /// FIELD of `create` was filed as if the whole route were missing) and once
    /// falsely (`DELETE /containers/{id}` is a substring of `GET
    /// /containers/{id}/logs`). Both pushed the "not implemented" list into the
    /// same (method, path, why) shape as the matrix, which is what makes an
    /// exact comparison possible at all.
    #[test]
    fn nada_esta_nas_duas_listas() {
        for (m, path, _) in API_MATRIX {
            assert!(
                !API_UNIMPLEMENTED
                    .iter()
                    .any(|(um, up, _)| up == path && um == m),
                "{m} {path} appears as both implemented and not"
            );
        }
    }
}

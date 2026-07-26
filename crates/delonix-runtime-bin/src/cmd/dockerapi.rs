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
/// **Precondition this relies on**: `waitpid(-1, ...)` reaps ANY child of this
/// process, so it would race a codepath that does its OWN blocking
/// `waitpid(<specific pid>, ...)` on a child it just forked (e.g.
/// `reexec_mapped`/`remove_tree_mapped` in this same engine crate, used by
/// `build`/`volsnap`/`prune`) — this server's routes never reach those today
/// (only container create/start/stop/kill/wait/restart/rename/remove/inspect,
/// none of which fork-and-waitpid directly; they all poll liveness via
/// `kill(pid, 0)`). If a future route ever wires in `build`/`volsnap`/`prune`,
/// revisit this — a blind `waitpid(-1)` would silently corrupt their exit
/// status.
fn spawn_zombie_reaper() {
    std::thread::spawn(|| loop {
        let mut status: i32 = 0;
        // SAFETY: waitpid(-1, ...) only reaps this process's own children.
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            // ECHILD (nothing to reap right now) or a transient error — a
            // container gets created whenever a request comes in, so just
            // check back shortly instead of busy-looping.
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
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

/// uid of the peer of a unix connection (via `SO_PEERCRED`). Same mechanism as
/// `delonix-mgmt::peer_uid`/`delonix-net::infra::peer_uid` — duplicated here
/// rather than shared, matching how each of those two already duplicate it
/// instead of a common crate for three call sites.
fn peer_uid(stream: &tokio::net::UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt on SO_PEERCRED with a correctly-sized ucred buffer.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if r == 0 {
        Some(cred.uid)
    } else {
        None
    }
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
                        "message": format!(
                            "{method} {path}: not implemented in delonix's Docker API \
                             compatibility layer yet — see docs/COMPARACAO-DOCKER-PODMAN.md"
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
    let state = state.clone();
    let result: Result<String> = run_blocking(move || {
        super::container::cmd_run(&state.images, &state.store, opts)?;
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
            json!({ "Id": id, "Warnings": [] }).to_string().into_bytes(),
        ),
        Err(e) => err_response(&e),
    }
}

async fn handle_start(state: &Arc<AppState>, id: &str) -> (StatusCode, Vec<u8>) {
    let state = state.clone();
    let id = id.to_string();
    match run_blocking(move || super::container::cmd_start(&state.images, &state.store, &id)).await
    {
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
    state: &Arc<AppState>,
    id: &str,
    params: &std::collections::HashMap<String, String>,
) -> (StatusCode, Vec<u8>) {
    let timeout: u64 = params.get("t").and_then(|v| v.parse().ok()).unwrap_or(10);
    let state = state.clone();
    let id = id.to_string();
    match run_blocking(move || {
        super::container::cmd_restart(&state.images, &state.store, &id, timeout)
    })
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
            "Image": c.image,
            "Cmd": c.command,
            "Env": c.env,
            "Labels": c.labels,
        },
        "Image": c.image,
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
/// **Deliberately NOT mapped** (documented limitation, not a silent gap):
/// `HostConfig.NetworkMode`/`NetworkingConfig` (Docker's bridge/user-defined
/// network model has no equivalent here — every API-created container gets
/// delonix's own default, `--net host`), `WorkingDir` (this engine has no
/// `run -w` at all yet — the image's own configured workdir is always used,
/// same as a plain CLI `run`), `Tty`/`OpenStdin` (this create path is for
/// `docker run -d`/`compose up`'s non-interactive flow — see the module doc
/// for why interactive `exec` is out of scope for this pass too).
fn docker_config_to_run_opts(name: String, cfg: &serde_json::Value) -> Result<RunOpts> {
    let image = cfg["Image"]
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

    Ok(RunOpts {
        detach: true,
        name: Some(name),
        net: "host".to_string(),
        volumes,
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
                "Image": c.image,
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

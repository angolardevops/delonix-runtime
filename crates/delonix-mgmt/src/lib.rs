//! **Local management API of the Delonix Runtime** (HTTP+JSON over a unix socket).
//!
//! This is the surface that an external control-plane (the `delonix-paas`, via its
//! `RemoteRuntime`) consumes to operate the engine **without a direct link to the
//! crates** — it speaks only HTTP with this socket on the same host. It complements
//! the CRI (`delonix-cri`, which serves the kubelet): this serves the product's
//! *management* (volumes/containers/…).
//!
//! Exposed surfaces: **volumes** (CRUD), **containers** (list/get + run/rm/
//! action/logs/exec + partial reconfig), **images** (list/rmi/pull/build/scan/
//! sbom), **networks** (create/rm) and **VMs** (only stop/rm — divergent subsystem).
//! The READ contract is each resource's own serde type (`Volume`,
//! `Container`, `Image`, `Package`); the MUTATIONS return `{ok, output}`.

pub mod dashstats;

use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use delonix_image::ImageStore;
use delonix_runtime_core::peer_cred::peer_uid;
use delonix_runtime_core::{Error, Store};
use delonix_volume::VolumeStore;

/// Shared state of the handlers.
#[derive(Clone)]
struct AppState {
    /// The root of the runtime state (`$DELONIX_ROOT`).
    base: PathBuf,
    /// The runtime CLI binary (`delonix`) for the MUTATIONS. Unlike the
    /// reads (library calls to the Store), a container mutation
    /// (rm/stop/start/…) must reuse the engine's REAL path — kill the process,
    /// clean up cgroups/namespaces, unpublish ports, disconnect networks — which
    /// lives in the CLI. Calling the CLI itself guarantees full parity, rather than
    /// reimplementing that cleanup here. It is the same decision the PaaS's
    /// `InProcessRuntime` already took; the Runtime-as-a-service architecture only
    /// MOVES that shell-out here.
    bin: PathBuf,
}

/// Starts the management API listening on a unix socket (blocking). `addr` accepts
/// a path or `unix:///path`. Same pattern as `delonix-cri::serve_blocking`.
pub fn serve_blocking(base: PathBuf, addr: &str) -> Result<(), Error> {
    // The binary for the mutations is the executable ITSELF (this process IS the
    // `delonix api`); fall back to "delonix" in PATH if `current_exe` fails.
    let bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("delonix"));
    serve_blocking_with(base, bin, addr)
}

/// Like [`serve_blocking`], but with the CLI binary explicit (for tests).
pub fn serve_blocking_with(base: PathBuf, bin: PathBuf, addr: &str) -> Result<(), Error> {
    let path = addr.strip_prefix("unix://").unwrap_or(addr).to_string();
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
        // SECURITY: this is the highest-privilege surface in the runtime — every
        // route (including `/v1/containers/:id/exec`, arbitrary code execution
        // inside any container) was reachable by ANY local process, gated only by
        // the ambient umask at bind time. Mirror the holder's control socket
        // (`delonix-net::infra::holder_main`): 0600 file mode + `SO_PEERCRED` on
        // every accepted connection, checked in `serve_over_uds` below.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        eprintln!("delonix-mgmt (management API) listening on unix://{path}");
        tokio::spawn(spawn_expensive_metrics_refresh(base.clone()));
        serve_over_uds(uds, router(AppState { base, bin })).await
    })
}

/// Periodically re-publishes the EXPENSIVE `dashstats` gauges (per-container
/// network reads + the full disk-usage walk) in the background, decoupled
/// from any single `/metrics` request. MEASURED on a real host (49
/// containers, several full `kindest/node` rootfs copies): the storage walk
/// alone took over a minute — computing it inline inside a request handler
/// would blow past Prometheus's default 10s scrape timeout on every scrape.
/// The cheap fields (container/VM counts, cgroup memory) are still collected
/// fresh on every `/metrics` request (see `metrics()`) — only this slow half
/// is decoupled. Runs for the lifetime of the process; best-effort (a
/// collection panic/slow disk on one tick never stops the next one).
async fn spawn_expensive_metrics_refresh(base: PathBuf) {
    // BUG FOUND (code review): this used to `.await` `dashstats::collect`
    // with no ceiling at all — a genuinely stuck disk/netns operation (not
    // just "slow", an actual hang) would freeze this loop, and every
    // expensive gauge, for the remaining lifetime of the process. Bounded
    // with `collect_with_timeout`; see its own doc comment for why the
    // worker thread is leaked rather than cancelled on a timeout.
    const COLLECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    loop {
        let base = base.clone();
        let summary = tokio::task::spawn_blocking(move || {
            dashstats::collect_with_timeout(&base, true, true, COLLECT_TIMEOUT)
        })
        .await;
        match summary {
            Ok(dashstats::Bounded::Done(summary)) => dashstats::publish_to_metrics(&summary),
            Ok(dashstats::Bounded::TimedOut) => eprintln!(
                "delonix-mgmt: expensive metrics collection did not finish within {}s — \
                 network/storage gauges stay at their last known value this cycle",
                COLLECT_TIMEOUT.as_secs()
            ),
            // Distinct from a timeout, and the distinction is the whole point:
            // nothing was attempted this cycle because the PREVIOUS collection
            // is still wedged in the same underlying operation (a hung network
            // volume, a stuck netns read). Saying "did not finish within 120s"
            // here would be a lie — we never started. See `run_bounded`.
            Ok(dashstats::Bounded::Skipped) => eprintln!(
                "delonix-mgmt: skipping expensive metrics collection — a previous one is \
                 still stuck (hung volume or netns read?); not starting another until it \
                 returns. Network/storage gauges stay at their last known value."
            ),
            Err(_) => {} // the spawn_blocking task itself panicked — already logged by tokio.
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

/// Serves an axum `Router` over a `UnixListener` (`axum::serve` only accepts TCP;
/// here we use the accept loop + hyper-util, the pattern from axum's unix example).
async fn serve_over_uds(uds: tokio::net::UnixListener, app: Router) -> Result<(), Error> {
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tower::Service;
    // SAFETY: geteuid() has no preconditions.
    let own_uid = unsafe { libc::geteuid() };
    let mut make = app.into_make_service();
    loop {
        // BUG FIXED: `?` used to propagate ANY accept() error out of this loop,
        // tearing down the whole management API process — including on
        // EMFILE/ENFILE/ECONNABORTED, all transient and self-clearing conditions
        // `accept(2)` explicitly documents as "retry", not "give up". A brief fd
        // exhaustion elsewhere on the host used to kill every in-flight request
        // this server was handling. Now it's logged and the loop keeps accepting.
        let (socket, _) = match uds.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("delonix-mgmt: accept() error (transient, retrying): {e}");
                continue;
            }
        };
        // Reject any peer that isn't this process's own euid BEFORE it ever reaches
        // the router — a mismatched umask/`/run` placement must not turn into a
        // full control-plane RCE for any local user.
        if peer_uid(&socket) != Some(own_uid) {
            continue;
        }
        // `into_make_service` is infallible → the connection service never fails here.
        let tower_service = match make.call(&socket).await {
            Ok(svc) => svc,
            Err(never) => match never {},
        };
        tokio::spawn(async move {
            let io = TokioIo::new(socket);
            let hyper_service = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                tower_service.clone().call(req)
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, hyper_service)
                .await;
        });
    }
}

/// The API routes. Exposed for tests (no socket, via `oneshot`).
fn router(state: AppState) -> Router {
    Router::new()
        .route("/_ping", get(ping))
        // `GET /metrics` — the SAME shared Prometheus registry that `delonix-cri`
        // exposes (defined in `delonix-runtime-core::metrics`), refreshed with a
        // fresh `dashstats::collect` snapshot on every scrape (see `metrics()`).
        // The Grafana-native path: point a Prometheus server at this endpoint.
        .route("/metrics", get(metrics))
        // `GET /v1/dash` — the SAME snapshot as JSON, for anything that isn't
        // Prometheus (a custom SRE tool, `curl`, a JSON-datasource panel).
        .route("/v1/dash", get(dash_summary))
        .route("/v1/volumes", get(list_volumes).post(create_volume))
        .route("/v1/volumes/:name", get(get_volume).delete(delete_volume))
        // Containers: read (list/get) via library; mutation (below) via CLI.
        // `POST` = `run` (receives the spec in JSON and rebuilds the CLI args).
        .route("/v1/containers", get(list_containers).post(run_container))
        .route(
            "/v1/containers/:id",
            get(get_container).delete(delete_container),
        )
        // Container mutation: rm/start/stop/restart/pause/unpause — shell-out to the
        // runtime CLI (full cleanup parity), not a call to the Store.
        .route("/v1/containers/:id/action", post(container_action_ep))
        // Logs (request/response, not streaming) + non-interactive exec.
        .route("/v1/containers/:id/logs", get(container_logs_ep))
        .route("/v1/containers/:id/exec", post(container_exec_ep))
        // Images: list + rmi. The reference (`nginx:latest`, `library/nginx`,
        // `sha256:…`) does NOT fit in a path segment (it has `/` and `:`) → it goes
        // by query (`?ref=…`). No traversal risk: `ImageStore::remove`
        // resolves by linear scan (compares tags/id prefix) and the file
        // it deletes uses the sanitized `id`, never the raw `ref`.
        .route("/v1/images", get(list_images).delete(delete_image))
        // Pull (optionally with a CVE scan afterwards) — shell-out to the CLI.
        .route("/v1/images/pull", post(pull_image))
        // Build from a pasted Delonixfile (materializes + `delonix build`).
        .route("/v1/images/build", post(build_image))
        // CVE scan (text, via CLI) + SBOM (structured, via library).
        .route("/v1/images/scan", get(scan_image))
        .route("/v1/images/sbom", get(sbom_image))
        // Networks: create/rm (network lifecycle) — shell-out to the CLI. publish/
        // unpublish (DNAT) do NOT go here — `Net::`/`infra::` debt in the PaaS.
        .route("/v1/networks", get(list_networks).post(create_network))
        .route(
            "/v1/networks/:name",
            get(get_network).delete(delete_network),
        )
        .route("/v1/net/status", get(net_status))
        // Publicação de portos (DNAT + hostfwd do slirp). Ao contrário do
        // create/rm de redes, NÃO passa pelo binário: o `publish_port` recebe o
        // IP do container, e a CLI recebe o NOME — quem chama esta API (o
        // control-plane) tem o IP e não tem forma de resolver o nome do lado de
        // cá sem uma segunda volta. Chamar a biblioteca é o caminho curto e o
        // honesto.
        .route("/v1/net/publish", post(publish_port))
        .route(
            "/v1/net/publish/:host_port",
            axum::routing::delete(unpublish_port),
        )
        // Firewall por workload e política de saída. Mesma razão do publish para
        // não passar pelo binário: o mecanismo é endereçado por IP e por bridge,
        // e a CLI por nome.
        .route(
            "/v1/net/firewall/:ip",
            put(apply_firewall).delete(clear_firewall),
        )
        .route("/v1/net/egress", put(set_egress_global))
        .route("/v1/net/egress/:bridge", put(set_egress_net))
        .route("/v1/containers/:id/rate", put(set_net_rate))
        // Endereços e ligação a redes: as perguntas que o control-plane faz
        // antes de publicar um porto ou escrever uma regra.
        .route("/v1/net/dhcp/:net/:mac", get(dhcp_ip))
        .route("/v1/net/dhcp6/:net/:mac", get(dhcp_ip6))
        .route("/v1/net/container-ip/:id", get(container_ip))
        .route("/v1/net/attach-extra", post(attach_extra))
        .route(
            "/v1/net/attach-extra/:id/:idx/:ip",
            axum::routing::delete(detach_extra),
        )
        .route("/v1/net/attach/:id/:ip", axum::routing::delete(detach))
        // Hot reconfig of a container: ONLY the subset that the runtime's `container
        // update` supports (publish-add/publish-rm). The fields the PaaS's
        // `ContainerUpdateSpec` has but the runtime does NOT (memory/cpus/restart/
        // dns/hosts) are rejected on the PaaS side and never reach here.
        .route("/v1/containers/:id/reconfig", post(reconfig_container))
        // VMs (delonix-vm subsystem): ONLY stop/rm (the runtime has no `vm run`/
        // `vm start`; `vm create` is another model). See the note in the PaaS.
        .route("/v1/vms/:name/action", post(vm_action_ep))
        .with_state(state)
}

async fn ping() -> &'static str {
    "delonix-mgmt ok"
}

/// `GET /metrics` — OpenMetrics body of the SHARED Prometheus registry in
/// `delonix-runtime-core` (the same one `delonix-cri` serves). Refreshes only
/// the CHEAP gauges (container/VM counts, cgroup memory) inline on every
/// request — Prometheus's default scrape timeout (10s) does not leave room
/// for the per-container netns reads, let alone the disk-usage walk (measured
/// over a minute on a host with heavy containers). Those two are published by
/// `spawn_expensive_metrics_refresh`'s background loop instead — the gauges it
/// owns are stale by up to its refresh interval, never absent.
async fn metrics(State(s): State<AppState>) -> impl IntoResponse {
    let base = s.base.clone();
    let summary =
        tokio::task::spawn_blocking(move || dashstats::collect(&base, false, false)).await;
    if let Ok(summary) = summary {
        dashstats::publish_to_metrics(&summary);
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        delonix_runtime_core::metrics::encode(),
    )
}

/// `GET /v1/dash` — the same [`dashstats::DashSummary`] as JSON, WITH the
/// expensive fields (unlike `/metrics`, which stays fast for Prometheus's
/// scrape timeout) — this is an on-demand call, not a periodic scrape, so it
/// can afford the full network+storage collection. Can take well over a
/// minute on a host with many/large containers (see `dashstats::collect`'s
/// doc comment) — a client with a request timeout shorter than that should
/// use `/metrics` instead, which is always fast.
async fn dash_summary(State(s): State<AppState>) -> Response {
    match tokio::task::spawn_blocking(move || dashstats::collect(&s.base, true, true)).await {
        Ok(summary) => Json(summary).into_response(),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// Safe volume name at the API BOUNDARY (defense against path traversal). It is
/// deliberately STRICTER than the `VolumeStore`: that one accepts `..` (only `.`
/// characters) and `inspect`/`remove` don't even validate the name — a `remove("..")`
/// coming from the URL path would delete the parent directory. Here any lone
/// `..`/`/`/`.` is rejected.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains("..")
        && !name.contains('/')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Standard 400 error for an invalid volume name.
fn invalid_name() -> Response {
    err_response(Error::Invalid("invalid volume name".to_string()))
}

/// Maps an engine `Error` to (HTTP code, JSON body) — the client
/// reconstructs its own `RuntimeError` from the code + message.
///
/// The body carries `code` (the stable `DX_*` identity, see
/// [`Error::code`]) beside the message. **Additive on purpose**: the field was
/// added, `error` was not touched. A client outside this repo already reads
/// `.error`, and the discipline ADR-0005 fixed for `-o json` — fields may be
/// ADDED, never removed nor retyped — is the same one that applies here. The
/// nested `{"error": {"code", "message"}}` envelope the CLI restructuring
/// specifies is a DIFFERENT surface (the CLI's own `-o json`), and building it
/// there costs nothing here.
///
/// Why the message alone was not enough: it is the half that gets translated,
/// so a client that classifies by matching text works on the machine it was
/// written on and stops classifying on a node with another locale. That is the
/// same reason the exit codes exist.
fn err_response(e: Error) -> Response {
    // Read BEFORE the match: it destructures `e` by value.
    let dx = e.code();
    let (code, msg) = match e {
        Error::NotFound(m) => (StatusCode::NOT_FOUND, m),
        Error::Invalid(m) => (StatusCode::BAD_REQUEST, m),
        Error::Conflict(m) => (StatusCode::CONFLICT, m),
        // The same two classes the exit codes publish, in the transport that
        // already has words for them. Without these they fell into the catch-all
        // 500, which tells a caller «this server is broken» when what happened
        // is «this host lacks a tool» or «it is still coming up».
        Error::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
        Error::Timeout(m) => (StatusCode::GATEWAY_TIMEOUT, m),
        other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    };
    (code, Json(serde_json::json!({ "error": msg, "code": dx }))).into_response()
}

/// Runs a synchronous `VolumeStore` operation on a blocking thread (the store
/// is synchronous; it must not block the async executor).
async fn with_store<T, F>(base: PathBuf, f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(&VolumeStore) -> Result<T, Error> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let store = VolumeStore::open(&base)?;
        f(&store)
    })
    .await
    .map_err(|e| Error::Runtime {
        context: "join",
        message: e.to_string(),
    })?
}

async fn list_volumes(State(s): State<AppState>) -> Response {
    match with_store(s.base, |store| store.list()).await {
        Ok(vols) => Json(vols).into_response(),
        Err(e) => err_response(e),
    }
}

async fn get_volume(State(s): State<AppState>, Path(name): Path<String>) -> Response {
    if !valid_name(&name) {
        return invalid_name();
    }
    match with_store(s.base, move |store| store.inspect(&name)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

/// Body of `POST /v1/volumes`.
#[derive(serde::Deserialize)]
struct CreateVolumeBody {
    name: String,
    #[serde(default)]
    driver: Option<String>,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    options: Option<String>,
}

async fn create_volume(State(s): State<AppState>, Json(b): Json<CreateVolumeBody>) -> Response {
    if !valid_name(&b.name) {
        return invalid_name();
    }
    let driver = b.driver.unwrap_or_else(|| "local".to_string());
    match with_store(s.base, move |store| {
        store.create_with(&b.name, &driver, b.device, b.options)
    })
    .await
    {
        Ok(v) => (StatusCode::CREATED, Json(v)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn delete_volume(State(s): State<AppState>, Path(name): Path<String>) -> Response {
    if !valid_name(&name) {
        return invalid_name();
    }
    match with_store(s.base, move |store| store.remove(&name)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_response(e),
    }
}

// ---- Containers (read) -----------------------------------------------------

/// Runs a synchronous container `Store` operation on a blocking thread.
/// The store lives at `<base>/containers` (same resolution the CLI uses in
/// `util::open_stores`).
async fn with_container_store<T, F>(base: PathBuf, f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> Result<T, Error> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let store = Store::open(base.join("containers"))?;
        f(&store)
    })
    .await
    .map_err(|e| Error::Runtime {
        context: "join",
        message: e.to_string(),
    })?
}

async fn list_containers(State(s): State<AppState>) -> Response {
    match with_container_store(s.base, |store| store.list()).await {
        Ok(cs) => Json(cs).into_response(),
        Err(e) => err_response(e),
    }
}

async fn get_container(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    // Same boundary defense as volumes: `Store::load` does `root.join(id)`
    // before falling into the scan by name/prefix — a `..` in the path would escape.
    if !valid_name(&id) {
        return err_response(Error::Invalid("invalid container id".to_string()));
    }
    match with_container_store(s.base, move |store| store.load(&id)).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => err_response(e),
    }
}

/// Safe argument to pass to the CLI: besides `valid_name` (no `..`/`/`), it refuses
/// a leading `-` — otherwise the CLI's `clap` would interpret the id as a flag (e.g. an
/// id `--rm`). The CLI args do not suffer shell injection (`Command::args`, not a
/// string), but they can be read as options — hence the barrier against `-`.
fn valid_arg(s: &str) -> bool {
    valid_name(s) && !s.starts_with('-')
}

/// Runs the runtime CLI (`delonix …`) with `DELONIX_ROOT` at the base, and returns
/// `(success, combined output)`. Blocking → runs in `spawn_blocking`.
async fn run_cli(bin: PathBuf, base: PathBuf, args: Vec<String>) -> Result<(bool, String), Error> {
    tokio::task::spawn_blocking(move || {
        let out = std::process::Command::new(&bin)
            .env("DELONIX_ROOT", &base)
            .args(&args)
            .output()
            .map_err(|e| Error::Runtime {
                context: "cli",
                message: e.to_string(),
            })?;
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.success(), s.trim().to_string()))
    })
    .await
    .map_err(|e| Error::Runtime {
        context: "join",
        message: e.to_string(),
    })?
}

/// Query of `DELETE /v1/containers/:id?force=<bool>`.
#[derive(serde::Deserialize)]
struct ForceQuery {
    #[serde(default)]
    force: bool,
}

async fn delete_container(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ForceQuery>,
) -> Response {
    if !valid_arg(&id) {
        return err_response(Error::Invalid("invalid container id".to_string()));
    }
    let mut args = vec!["container".to_string(), "rm".to_string()];
    if q.force {
        args.push("-f".to_string());
    }
    args.push(id);
    match run_cli(s.bin, s.base, args).await {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

/// Body of `POST /v1/containers/:id/action`.
#[derive(serde::Deserialize)]
struct ActionBody {
    action: String,
}

async fn container_action_ep(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<ActionBody>,
) -> Response {
    if !valid_arg(&id) {
        return err_response(Error::Invalid("invalid container id".to_string()));
    }
    // Only known actions (allowlist) reach the CLI. `remove` = `rm -f`.
    let sub = match b.action.as_str() {
        "start" | "stop" | "restart" | "pause" | "unpause" => b.action.clone(),
        "remove" | "rm" => {
            match run_cli(
                s.bin,
                s.base,
                vec![
                    "container".to_string(),
                    "rm".to_string(),
                    "-f".to_string(),
                    id,
                ],
            )
            .await
            {
                Ok((ok, out)) => {
                    return Json(serde_json::json!({ "ok": ok, "output": out })).into_response()
                }
                Err(e) => return err_response(e),
            }
        }
        other => return err_response(Error::Invalid(format!("unknown action: {other}"))),
    };
    match run_cli(s.bin, s.base, vec!["container".to_string(), sub, id]).await {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn container_logs_ep(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    if !valid_arg(&id) {
        return err_response(Error::Invalid("invalid container id".to_string()));
    }
    // `logs` request/response (not streaming); the output comes as-is, even if the
    // container does not exist (the client ignores the `ok`, like the InProcessRuntime).
    match run_cli(
        s.bin,
        s.base,
        vec!["container".to_string(), "logs".to_string(), id],
    )
    .await
    {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

/// Body of `POST /v1/containers/:id/exec`.
#[derive(serde::Deserialize)]
struct ExecBody {
    cmd: String,
}

async fn container_exec_ep(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<ExecBody>,
) -> Response {
    if !valid_arg(&id) {
        return err_response(Error::Invalid("invalid container id".to_string()));
    }
    // `exec <id> sh -c <cmd>`: the `cmd` is passed as ONE argument to `sh -c` INSIDE
    // the container — runs in the container, never in the host's shell (it is
    // `Command::args`, no shell of ours). Exec is, by nature, arbitrary exec in the container.
    let args = vec![
        "container".to_string(),
        "exec".to_string(),
        id,
        "sh".to_string(),
        "-c".to_string(),
        b.cmd,
    ];
    match run_cli(s.bin, s.base, args).await {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

/// Body of `POST /v1/containers` (run). Mirrors the PaaS's `ContainerRunSpec` — the
/// contract is the field names (the PaaS serializes its spec, this deserializes it).
#[derive(serde::Deserialize)]
struct RunSpecBody {
    image: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    network: String,
    #[serde(default)]
    memory: String,
    #[serde(default)]
    restart: String,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    volumes: Vec<String>,
    #[serde(default)]
    knows: Vec<String>,
    #[serde(default)]
    knows_none: bool,
}

/// Rebuilds the `delonix container run -d …` args from the spec — a PURE function
/// (testable without a kernel). The filters are the SAME the PaaS's `InProcessRuntime`
/// already used; the only flag-name difference is deliberate: the runtime binary
/// uses `--net` (the PaaS one, with the docker shim, used `--network`).
fn build_run_args(spec: RunSpecBody) -> Vec<String> {
    let mut args: Vec<String> = vec!["container".into(), "run".into(), "-d".into()];
    if !spec.name.is_empty() {
        args.push("--name".into());
        args.push(spec.name);
    }
    if !spec.network.is_empty() && spec.network != "none" {
        // The runtime CLI uses `--net` (not `--network`). Form `--net=<v>` so the
        // value never escapes into a new token.
        args.push(format!("--net={}", spec.network));
    }
    for p in &spec.ports {
        if p.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ':' | '/'))
        {
            args.push("-p".into());
            args.push(p.clone());
        }
    }
    for e in spec.env {
        args.push("-e".into());
        args.push(e);
    }
    for v in spec.volumes {
        if !v.is_empty() && !v.contains("..") {
            args.push("-v".into());
            args.push(v);
        }
    }
    if spec.knows_none {
        args.push("--knows-none".into());
    } else {
        for k in spec.knows {
            if !k.is_empty() {
                args.push("--knows".into());
                args.push(k);
            }
        }
    }
    if !spec.memory.is_empty() {
        args.push("-m".into());
        args.push(spec.memory);
    }
    if !spec.restart.is_empty() {
        args.push("--restart".into());
        args.push(spec.restart);
    }
    args.push(spec.image);
    args.extend(spec.command);
    args
}

async fn run_container(State(s): State<AppState>, Json(spec): Json<RunSpecBody>) -> Response {
    // `image` is required and a value starting with `-` would be read by clap as
    // a flag (it is the final POSITIONAL argument) — refuse at the boundary. Same for
    // `name` (value of `--name`). The remaining fields either have their own charset
    // (ports) or are option values with no positional ambiguity.
    if spec.image.is_empty() || spec.image.starts_with('-') {
        return err_response(Error::Invalid("invalid image".to_string()));
    }
    if spec.name.starts_with('-') {
        return err_response(Error::Invalid("invalid name".to_string()));
    }
    match run_cli(s.bin, s.base, build_run_args(spec)).await {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

// ---- Images (list + rmi) ---------------------------------------------------

/// Runs a synchronous `ImageStore` operation on a blocking thread. The store
/// resolves `<base>/images` internally (it receives the base, like the `VolumeStore`).
async fn with_image_store<T, F>(base: PathBuf, f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(&ImageStore) -> Result<T, Error> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let store = ImageStore::open(&base)?;
        f(&store)
    })
    .await
    .map_err(|e| Error::Runtime {
        context: "join",
        message: e.to_string(),
    })?
}

async fn list_images(State(s): State<AppState>) -> Response {
    match with_image_store(s.base, |store| store.list()).await {
        Ok(imgs) => Json(imgs).into_response(),
        Err(e) => err_response(e),
    }
}

/// Query of `DELETE /v1/images?ref=…`. `ref` is a reserved word in Rust.
#[derive(serde::Deserialize)]
struct RefQuery {
    #[serde(rename = "ref")]
    reference: String,
}

async fn delete_image(State(s): State<AppState>, Query(q): Query<RefQuery>) -> Response {
    if q.reference.is_empty() {
        return err_response(Error::Invalid("empty image reference".to_string()));
    }
    match with_image_store(s.base, move |store| store.remove(&q.reference)).await {
        // `remove` returns "untagged: …" or "deleted: …" — return it as-is.
        Ok(result) => Json(serde_json::json!({ "result": result })).into_response(),
        Err(e) => err_response(e),
    }
}

/// Body of `POST /v1/images/pull`.
#[derive(serde::Deserialize)]
struct PullBody {
    #[serde(rename = "ref")]
    reference: String,
    /// Also runs a CVE scan after the pull (and appends the output).
    #[serde(default)]
    scan_after: bool,
}

async fn pull_image(State(s): State<AppState>, Json(b): Json<PullBody>) -> Response {
    // The reference is the POSITIONAL argument of `image pull` — a leading `-` would
    // be read as a flag. Refuse at the boundary. (Valid refs have `/`/`:`, never `-`
    // at the start.)
    if b.reference.is_empty() || b.reference.starts_with('-') {
        return err_response(Error::Invalid("invalid image reference".to_string()));
    }
    let (ok, mut out) = match run_cli(
        s.bin.clone(),
        s.base.clone(),
        vec!["image".into(), "pull".into(), b.reference.clone()],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return err_response(e),
    };
    // Optional auto-scan after the pull (only if the pull succeeded).
    if ok && b.scan_after {
        if let Ok((_so, sout)) = run_cli(
            s.bin,
            s.base,
            vec!["image".into(), "scan".into(), b.reference],
        )
        .await
        {
            out.push_str("\n--- scan (CVE) ---\n");
            out.push_str(&sout);
        }
    }
    Json(serde_json::json!({ "ok": ok, "output": out })).into_response()
}

/// Body of `POST /v1/images/build`.
#[derive(serde::Deserialize)]
struct BuildBody {
    /// Content of the Delonixfile (pasted; the `RUN`s run during the build).
    delonixfile: String,
    /// Tag of the resulting image (`repo:tag`).
    tag: String,
}

/// Per-process monotonic counter to name unique build work dirs.
static BUILD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn build_image(State(s): State<AppState>, Json(b): Json<BuildBody>) -> Response {
    // `-t <tag>`: a value starting with `-` would be read as a flag. Refuse at the boundary.
    if b.tag.is_empty() || b.tag.starts_with('-') {
        return err_response(Error::Invalid("invalid tag".to_string()));
    }
    if b.delonixfile.trim().is_empty() {
        return err_response(Error::Invalid("empty Delonixfile".to_string()));
    }
    // UNIQUE work dir per-build (the context where `COPY` resolves): `pid-seq` isolates
    // concurrent builds — without it, two parallel builds would share the
    // `Delonixfile`/context (TOCTOU: one builds the other's Delonixfile). Cleaned up at
    // the end. The name derives only from `s.base`+pid+counter — never from user input.
    let seq = BUILD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = s
        .base
        .join("_mgmt_build")
        .join(format!("{}-{}", std::process::id(), seq));
    let file = dir.join("Delonixfile");
    let (dir_w, file_w, content) = (dir.clone(), file.clone(), b.delonixfile);
    let prep = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir_w)?;
        std::fs::write(&file_w, content)
    })
    .await;
    match prep {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return err_response(Error::Runtime {
                context: "build-prep",
                message: e.to_string(),
            })
        }
        Err(e) => {
            return err_response(Error::Runtime {
                context: "join",
                message: e.to_string(),
            })
        }
    }
    let args = vec![
        "build".to_string(),
        "-t".to_string(),
        b.tag,
        "-f".to_string(),
        file.to_string_lossy().into_owned(),
        dir.to_string_lossy().into_owned(),
    ];
    let result = run_cli(s.bin, s.base, args).await;
    // Clean up the work dir (best-effort) — don't leave Delonixfiles/contexts piling up.
    let dir_c = dir.clone();
    let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&dir_c)).await;
    match result {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn scan_image(State(s): State<AppState>, Query(q): Query<RefQuery>) -> Response {
    // `image scan <ref>` (text). The ref is positional — a leading `-` would become a flag.
    if q.reference.is_empty() || q.reference.starts_with('-') {
        return err_response(Error::Invalid("invalid image reference".to_string()));
    }
    match run_cli(
        s.bin,
        s.base,
        vec!["image".into(), "scan".into(), q.reference],
    )
    .await
    {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn sbom_image(State(s): State<AppState>, Query(q): Query<RefQuery>) -> Response {
    if q.reference.is_empty() {
        return err_response(Error::Invalid("empty image reference".to_string()));
    }
    // SBOM is a LIBRARY call (it reads the layers from the CAS, runs nothing) — like
    // the volume/container reads. 404 if the image doesn't exist locally.
    let out = with_image_store(s.base, move |store| {
        let img = store.resolve(&q.reference)?;
        // `extract` fails → the image exists but has no readable package manager (empty
        // list), just as the old handler distinguished it from "not found".
        Ok(delonix_scan::extract_sbom(store, &img).unwrap_or_default())
    })
    .await;
    match out {
        Ok(pkgs) => Json(pkgs).into_response(),
        Err(e) => err_response(e),
    }
}

/// Body of `POST /v1/networks`.
#[derive(serde::Deserialize)]
struct NetworkBody {
    name: String,
}

/// `GET /v1/net/dhcp/:net/:mac` — o IP que o DHCP dá a um MAC nesta rede.
///
/// Determinístico a partir do MAC (`<prefix>.254.<10 + fnv32(mac)%240>`), e não
/// lido da tabela ARP: o `ip neigh` só mostra o endereço depois de tráfego
/// recente, e devolvia «nenhum» para uma VM viva mas calada — caso reportado a
/// sério. Aqui o IP existe assim que a rede existe, que é o que serve para
/// abrir um SSH.
///
/// `None` → 404: a rede não existe. Não é o mesmo que «a VM ainda não arrancou»,
/// e por isso não se responde 200 com corpo vazio.
async fn dhcp_ip(Path((net, mac)): Path<(String, String)>) -> Response {
    if !valid_arg(&net) || !valid_mac(&mac) {
        return err_response(Error::Invalid("invalid network or MAC".to_string()));
    }
    match tokio::task::spawn_blocking(move || delonix_net::infra::dhcp_ip_for_mac(&net, &mac)).await
    {
        Ok(Some(ip)) => Json(serde_json::json!({ "ip": ip })).into_response(),
        Ok(None) => err_response(Error::NotFound("network".to_string())),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// `GET /v1/net/dhcp6/:net/:mac` — o equivalente IPv6.
///
/// Rota própria e não um campo na de cima: o v6 está DESLIGADO por decisão de
/// segurança neste motor, e quem o pede tem de o pedir. Um campo opcional numa
/// resposta partilhada faria parecer que vem de graça.
async fn dhcp_ip6(Path((net, mac)): Path<(String, String)>) -> Response {
    if !valid_arg(&net) || !valid_mac(&mac) {
        return err_response(Error::Invalid("invalid network or MAC".to_string()));
    }
    match tokio::task::spawn_blocking(move || delonix_net::infra::dhcp_ip6_for_mac(&net, &mac))
        .await
    {
        Ok(Some(ip)) => Json(serde_json::json!({ "ip": ip })).into_response(),
        Ok(None) => err_response(Error::NotFound("ipv6 lease".to_string())),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// `GET /v1/net/container-ip/:id` — o IP que um container tem na rede por omissão.
///
/// Derivado do id, sem tocar em estado: é a pergunta que o control-plane faz
/// antes de publicar um porto ou de escrever uma regra, e fazê-la por HTTP evita
/// que ele reimplemente a fórmula — que teria de dar exactamente o mesmo
/// resultado que esta, sempre.
async fn container_ip(Path(id): Path<String>) -> Response {
    if !valid_arg(&id) {
        return err_response(Error::Invalid("invalid container id".to_string()));
    }
    match tokio::task::spawn_blocking(move || delonix_net::infra::container_ip(&id)).await {
        Ok(ip) => Json(serde_json::json!({ "ip": ip })).into_response(),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// Corpo de `POST /v1/net/attach-extra`.
#[derive(serde::Deserialize)]
struct AttachExtraBody {
    id: String,
    /// Índice da interface adicional (0 é a primária, que não passa por aqui).
    idx: u32,
    net: String,
    #[serde(default)]
    namespace: String,
}

/// `POST /v1/net/attach-extra` — liga um container a uma rede ADICIONAL.
///
/// Devolve `{ ifname, ip }`: quem chama precisa dos dois para o que vem a
/// seguir (a regra de firewall é endereçada pelo IP, a de shaping pela
/// interface), e obrigá-lo a uma segunda volta para os descobrir seria pagar
/// duas viagens por uma operação.
async fn attach_extra(Json(b): Json<AttachExtraBody>) -> Response {
    if !valid_arg(&b.id) || !valid_arg(&b.net) {
        return err_response(Error::Invalid(
            "invalid container id or network".to_string(),
        ));
    }
    if !b.namespace.is_empty() && !valid_arg(&b.namespace) {
        return err_response(Error::Invalid("invalid namespace".to_string()));
    }
    let r = tokio::task::spawn_blocking(move || {
        delonix_net::infra::attach_extra_container(&b.id, b.idx, &b.net, &b.namespace)
    })
    .await;
    match r {
        Ok(Ok((ifname, ip))) => {
            Json(serde_json::json!({ "ifname": ifname, "ip": ip })).into_response()
        }
        Ok(Err(e)) => err_response(e),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// `DELETE /v1/net/attach-extra/:id/:idx/:ip` — desliga uma interface adicional.
///
/// Best-effort, como os outros `detach`: o mecanismo não devolve resultado.
async fn detach_extra(Path((id, idx, ip)): Path<(String, u32, String)>) -> Response {
    if !valid_arg(&id) || delonix_net::Cidr::parse_addr(&ip).is_none() {
        return err_response(Error::Invalid("invalid container id or IP".to_string()));
    }
    match tokio::task::spawn_blocking(move || {
        delonix_net::infra::detach_extra_container(&id, idx, &ip)
    })
    .await
    {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// `DELETE /v1/net/attach/:id/:ip` — desliga um container da rede primária.
async fn detach(Path((id, ip)): Path<(String, String)>) -> Response {
    if !valid_arg(&id) || delonix_net::Cidr::parse_addr(&ip).is_none() {
        return err_response(Error::Invalid("invalid container id or IP".to_string()));
    }
    match tokio::task::spawn_blocking(move || delonix_net::infra::detach_container(&id, &ip)).await
    {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// Um MAC é `aa:bb:cc:dd:ee:ff` — hexadecimal e dois-pontos, e mais nada.
///
/// Validado à parte do `valid_arg` porque este ACEITA os dois-pontos e aquele
/// não; sem isto, um MAC legítimo era recusado e a alternativa (afrouxar o
/// `valid_arg`) alargaria o crivo de todos os outros argumentos.
fn valid_mac(mac: &str) -> bool {
    !mac.is_empty()
        && mac.len() <= 17
        && mac
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '-')
}

/// Corpo de `PUT /v1/net/firewall/:ip`.
#[derive(serde::Deserialize)]
struct FirewallBody {
    /// Id do container — o mecanismo usa-o para nomear a cadeia.
    id: String,
    /// A política INTEIRA, tal como o `kind:Application` a exprime.
    fw: delonix_runtime_core::ContainerFw,
}

/// `PUT /v1/net/firewall/:ip` — aplica a firewall de um workload.
///
/// **Substitui, não acumula.** O `apply_firewall` escreve a cadeia inteira a
/// partir do que recebe; mandar metade das regras apaga a outra metade. É por
/// isso que o corpo leva a `ContainerFw` completa e não um delta — uma API de
/// deltas sobre uma cadeia que é reescrita de cada vez daria a ilusão de somar
/// e o efeito de substituir.
async fn apply_firewall(Path(ip): Path<String>, Json(b): Json<FirewallBody>) -> Response {
    if delonix_net::Cidr::parse_addr(&ip).is_none() {
        return err_response(Error::Invalid(format!("invalid IP: '{ip}'")));
    }
    if !valid_arg(&b.id) {
        return err_response(Error::Invalid("invalid container id".to_string()));
    }
    let r =
        tokio::task::spawn_blocking(move || delonix_net::infra::apply_firewall(&b.id, &ip, &b.fw))
            .await;
    match r {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(e)) => err_response(e),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// `DELETE /v1/net/firewall/:ip` — retira a firewall de um workload.
///
/// Best-effort, como o `unpublish`: o `clear_firewall` não devolve resultado. Um
/// 200 diz que a operação correu, não que havia cadeia para remover.
async fn clear_firewall(Path(ip): Path<String>) -> Response {
    if delonix_net::Cidr::parse_addr(&ip).is_none() {
        return err_response(Error::Invalid(format!("invalid IP: '{ip}'")));
    }
    match tokio::task::spawn_blocking(move || delonix_net::infra::clear_firewall(&ip)).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// Corpo das duas rotas de egress.
#[derive(serde::Deserialize)]
struct EgressBody {
    /// `true` corta a saída para a Internet.
    deny: bool,
}

/// `PUT /v1/net/egress` — política de saída de TODO o nó.
///
/// Rota separada da por-rede de propósito. As duas assinaturas do mecanismo
/// diferem por um argumento, e um único endpoint com `bridge` opcional faria
/// «cortar a saída de uma rede» e «cortar a saída do nó inteiro» distarem um
/// campo esquecido. O raio de dano é diferente de mais para depender disso.
async fn set_egress_global(Json(b): Json<EgressBody>) -> Response {
    match tokio::task::spawn_blocking(move || delonix_net::infra::set_egress_policy(b.deny)).await {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true, "deny": b.deny })).into_response(),
        Ok(Err(e)) => err_response(e),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// `PUT /v1/net/egress/:bridge` — política de saída de UMA rede.
async fn set_egress_net(Path(bridge): Path<String>, Json(b): Json<EgressBody>) -> Response {
    if !valid_arg(&bridge) {
        return err_response(Error::Invalid(format!("invalid bridge: '{bridge}'")));
    }
    let deny = b.deny;
    let r = tokio::task::spawn_blocking(move || {
        delonix_net::infra::set_egress_policy_net(&bridge, deny)
    })
    .await;
    match r {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true, "deny": deny })).into_response(),
        Ok(Err(e)) => err_response(e),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// Corpo de `PUT /v1/containers/:id/rate`.
#[derive(serde::Deserialize)]
struct RateBody {
    /// Débito em bits/segundo.
    rate_bit: u64,
    /// Balde do TBF, em bytes.
    burst_bytes: u64,
}

/// `PUT /v1/containers/:id/rate` — limita a largura de banda de um container a correr.
///
/// Sob `/v1/containers` e não sob `/v1/net`: quem o lê está a mudar uma
/// propriedade DAQUELE workload, e é lá que a vai procurar. O mecanismo é de
/// rede; o recurso não é.
async fn set_net_rate(Path(id): Path<String>, Json(b): Json<RateBody>) -> Response {
    if !valid_arg(&id) {
        return err_response(Error::Invalid("invalid container id".to_string()));
    }
    let r = tokio::task::spawn_blocking(move || {
        delonix_net::infra::set_net_rate(&id, b.rate_bit, b.burst_bytes)
    })
    .await;
    match r {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(e)) => err_response(e),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// Corpo de `POST /v1/net/publish`.
#[derive(serde::Deserialize)]
struct PublishBody {
    /// IP do container que recebe o tráfego.
    container_ip: String,
    /// `hostPort:contPort[/tcp|udp]` — a mesma forma que o `-p` da CLI.
    spec: String,
}

/// `POST /v1/net/publish` — publica um porto através do ingress.
///
/// Fecha a maior divida da fronteira: `publish_port`/`unpublish_port` eram 13 dos
/// ~153 sítios em que o control-plane chamava o `delonix-net` directamente, e o
/// comentário por cima das rotas de rede já os nomeava como tal («publish/
/// unpublish (DNAT) do NOT go here — `Net::`/`infra::` debt in the PaaS»).
///
/// O `container_ip` é validado antes de chegar ao mecanismo: é o que acaba numa
/// regra de DNAT, e uma string arbitrária ali é uma regra arbitrária.
async fn publish_port(State(s): State<AppState>, Json(b): Json<PublishBody>) -> Response {
    if delonix_net::Cidr::parse_addr(&b.container_ip).is_none() {
        return err_response(Error::Invalid(format!(
            "invalid container IP: '{}'",
            b.container_ip
        )));
    }
    if !valid_arg(&b.spec) {
        return err_response(Error::Invalid("invalid publish spec".to_string()));
    }
    // BUG FIXED HERE: this route used to call `infra::publish_port` and STOP —
    // it wrote the actual state and skipped the desired one. A port published
    // this way lived only in the slirp's `hostfwd` table and in the nft rules
    // inside the infra netns, both of which die with the ingress. The ONLY
    // durable copy of a publication is the container record's `ports`, and it is
    // exclusively from there that `cmd_start` and `reconcile_after_respawn`
    // replay them — so a port published here worked until the next ingress
    // restart and then vanished with nothing left to rebuild it from.
    //
    // Measured on a live host: 18 container records all with `ports: []` while
    // 127.0.0.1:8077 and :8079 were serving HTTP — publications no record knew
    // about. The engine could not have restored them; it did not know they were
    // wanted.
    //
    // Persist FIRST, publish second. The other order loses the record when the
    // process dies between the two, and an unpublished-but-recorded port is the
    // recoverable failure (the next start republishes it) while a
    // published-but-unrecorded one is exactly the bug being fixed.
    let ip = b.container_ip.clone();
    let spec = b.spec.clone();
    if let Err(e) = record_published_port(s.base.clone(), ip.clone(), spec.clone()).await {
        return err_response(e);
    }
    let r = tokio::task::spawn_blocking(move || {
        delonix_net::infra::publish_port(&b.container_ip, &b.spec)
    })
    .await;
    match r {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(e)) => {
            // The publication failed, so the record must not keep claiming it —
            // otherwise the next start would replay a port the operator never got.
            let _ = forget_published_port(s.base, Some(ip), spec).await;
            err_response(e)
        }
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// Adds `spec` to the `ports` of the container that holds `container_ip`.
///
/// Silent when no container matches: the ingress addresses workloads by IP and
/// nothing guarantees one of OUR records owns it (a VM, a foreign netns). The
/// publication itself still proceeds — refusing it would break a caller that
/// works today, and this function exists to record what it can, not to become a
/// second admission gate.
async fn record_published_port(base: PathBuf, ip: String, spec: String) -> Result<(), Error> {
    with_container_store(base, move |store| {
        for mut c in store.list()? {
            if c.ip.as_deref() == Some(ip.as_str()) {
                if !c.ports.contains(&spec) {
                    c.ports.push(spec.clone());
                    store.save(&c)?;
                }
                return Ok(());
            }
        }
        Ok(())
    })
    .await
}

/// Drops a publication from whichever record claims it.
///
/// `ip` narrows the search when the caller knows it (the rollback path); the
/// DELETE route knows only the host port, so it matches on that alone — which is
/// correct, because a host port can only be published once at a time.
async fn forget_published_port(
    base: PathBuf,
    ip: Option<String>,
    spec_or_port: String,
) -> Result<(), Error> {
    with_container_store(base, move |store| {
        let wanted_port = delonix_net::parse_publish(&spec_or_port)
            .map(|(hp, _, _)| hp)
            .unwrap_or_else(|_| spec_or_port.clone());
        for mut c in store.list()? {
            if ip.is_some() && c.ip != ip {
                continue;
            }
            let before = c.ports.len();
            c.ports.retain(|s| {
                delonix_net::parse_publish(s)
                    .map(|(hp, _, _)| hp != wanted_port)
                    .unwrap_or(true)
            });
            if c.ports.len() != before {
                store.save(&c)?;
            }
        }
        Ok(())
    })
    .await
}

/// `DELETE /v1/net/publish/:host_port` — retira a publicação de um porto.
///
/// **Best-effort, e a API não finge o contrário**: o `unpublish_port` não
/// devolve resultado — tira a regra se lá estiver e cala-se se não estiver. Um
/// 200 aqui significa «a operação correu», não «havia o que remover». Inventar
/// um 404 exigiria uma leitura que o mecanismo não oferece, e um 404 adivinhado
/// é pior do que a verdade.
async fn unpublish_port(State(s): State<AppState>, Path(host_port): Path<String>) -> Response {
    if host_port.is_empty() || !host_port.chars().all(|c| c.is_ascii_digit()) {
        return err_response(Error::Invalid(format!("invalid host port: '{host_port}'")));
    }
    // Mirror of the publish path: drop the desired state too, or the next start
    // would faithfully republish a port the operator has just asked to remove.
    let _ = forget_published_port(s.base, None, host_port.clone()).await;
    let r =
        tokio::task::spawn_blocking(move || delonix_net::infra::unpublish_port(&host_port)).await;
    match r {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// `GET /v1/networks` — as redes DECLARADAS neste nó.
///
/// Leitura pela biblioteca, como o resto dos `GET` deste módulo: passar pelo
/// binário só para ler seria pagar um `fork`+`exec` por uma leitura de ficheiros.
///
/// **Declaradas, não realizadas.** Uma rede aparece aqui a partir do
/// `network create`, mas a bridge só nasce no netns do holder ao primeiro
/// attach — ver `/v1/net/status` para saber o que está de pé. A distinção é do
/// modelo, não desta rota, e esconde-la aqui seria fazer a API mentir sobre ela.
async fn list_networks() -> Response {
    match tokio::task::spawn_blocking(delonix_net::infra::network_list).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// `GET /v1/networks/:name` — uma rede pelo nome, ou 404.
///
/// O 404 é a resposta certa e não um detalhe: quem chama isto está a decidir se
/// cria a rede ou se a reutiliza, e um corpo vazio com 200 fá-lo-ia criar por
/// cima de uma rede que existe.
async fn get_network(Path(name): Path<String>) -> Response {
    if !valid_arg(&name) {
        return err_response(Error::Invalid("invalid network name".to_string()));
    }
    let achada = tokio::task::spawn_blocking(move || delonix_net::infra::network_get(&name)).await;
    match achada {
        Ok(Some(def)) => Json(def).into_response(),
        Ok(None) => err_response(Error::NotFound("network".to_string())),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

/// `GET /v1/net/status` — o estado da infra de rede REALIZADA neste nó.
///
/// A contraparte do `list_networks`: aquele diz o que está declarado, este diz
/// o que está de pé (holder, slirp, bridge, ref-count). Um control-plane que só
/// leia o primeiro conclui que a rede existe quando ainda não existe.
async fn net_status() -> Response {
    match tokio::task::spawn_blocking(delonix_net::infra::status).await {
        Ok(st) => Json(st).into_response(),
        Err(e) => err_response(Error::Runtime {
            context: "join",
            message: e.to_string(),
        }),
    }
}

async fn create_network(State(s): State<AppState>, Json(b): Json<NetworkBody>) -> Response {
    if !valid_arg(&b.name) {
        return err_response(Error::Invalid("invalid network name".to_string()));
    }
    match run_cli(
        s.bin,
        s.base,
        vec!["network".into(), "create".into(), b.name],
    )
    .await
    {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn delete_network(State(s): State<AppState>, Path(name): Path<String>) -> Response {
    if !valid_arg(&name) {
        return err_response(Error::Invalid("invalid network name".to_string()));
    }
    match run_cli(s.bin, s.base, vec!["network".into(), "rm".into(), name]).await {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

/// Body of `POST /v1/containers/:id/reconfig`. Only the subset that the runtime's
/// `container update` supports — the PaaS refuses the remaining fields before calling.
#[derive(serde::Deserialize)]
struct ReconfigBody {
    #[serde(default)]
    publish_add: Vec<String>,
    #[serde(default)]
    publish_rm: Vec<String>,
}

async fn reconfig_container(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<ReconfigBody>,
) -> Response {
    if !valid_arg(&id) {
        return err_response(Error::Invalid("invalid container id".to_string()));
    }
    let mut args = vec!["container".to_string(), "update".to_string(), id];
    // The ports have their own charset (digits/`:`/`/`) — they can't become a flag.
    for p in &b.publish_add {
        if p.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ':' | '/'))
        {
            args.push("--publish-add".into());
            args.push(p.clone());
        }
    }
    for p in &b.publish_rm {
        if p.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ':' | '/'))
        {
            args.push("--publish-rm".into());
            args.push(p.clone());
        }
    }
    match run_cli(s.bin, s.base, args).await {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

/// Body of `POST /v1/vms/:name/action`. Only `stop`/`rm` (the runtime has no
/// `vm start`; `vm run`/`vm create` are another subsystem — refused in the PaaS).
#[derive(serde::Deserialize)]
struct VmActionBody {
    action: String,
}

async fn vm_action_ep(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(b): Json<VmActionBody>,
) -> Response {
    if !valid_arg(&name) {
        return err_response(Error::Invalid("invalid VM name".to_string()));
    }
    let sub = match b.action.as_str() {
        "stop" => "stop",
        "rm" | "remove" => "rm",
        other => return err_response(Error::Invalid(format!("unsupported VM action: {other}"))),
    };
    match run_cli(s.bin, s.base, vec!["vm".into(), sub.into(), name]).await {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

#[cfg(test)]
mod tests {
    /// Serialises the two tests below, which both write the PROCESS-GLOBAL
    /// `DELONIX_ROOT` while cargo runs them on parallel threads.
    ///
    /// The mechanism is not a guess — the doc-comment on the first of them
    /// already states it: those routes read through `infra`, which resolves the
    /// root from the ENVIRONMENT and not from the `AppState`, so each test
    /// points the global at a temp dir of its own. What that reasoning did not
    /// cover is the two of them doing it AT THE SAME TIME. Two tests writing
    /// one process-global is a hazard on its own terms, and that is the whole
    /// justification for this lock.
    ///
    /// **It is not claimed to fix the flake that led here, because that flake
    /// was never reproduced.** What was observed: under
    /// `cargo test --workspace`, the first test failed twice with the LIST
    /// returning one network and the GET of that network returning 404 — a list
    /// from one root, a lookup in another, which is the shape the shared global
    /// would produce. Run alone it passes 29/29, ten times over; with a 300 ms
    /// window opened right after the `set_var`, it still passes five times
    /// over, with and without this lock. So: plausible mechanism, documented in
    /// this very file, and no measurement. This comment will not pretend
    /// otherwise.
    ///
    /// A `tokio::sync::Mutex` and not a `std` one: the guard is held across
    /// `.await`, which `clippy::await_holding_lock` refuses — and this repo runs
    /// clippy with `-D warnings`.
    static ROOT_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt; // oneshot

    fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (
            AppState {
                base: dir.path().to_path_buf(),
                // `/bin/false`: the real mutations are proven in the cross-process e2e;
                // the unit tests cover only the validation (refusal BEFORE the exec).
                bin: PathBuf::from("/bin/false"),
            },
            dir,
        )
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    /// Builds a saved container record with an IP, for the publish-persistence tests.
    fn ctr_com_ip(base: &std::path::Path, id: &str, ip: &str) -> Store {
        let store = Store::open(base.join("containers")).unwrap();
        let mut c = delonix_runtime_core::Container::new(
            id.to_string(),
            id.to_string(),
            "alpine:3.20".to_string(),
            vec!["sleep".to_string()],
            "256m".to_string(),
        );
        c.ip = Some(ip.to_string());
        c.network = Some("rede".to_string());
        store.save(&c).unwrap();
        store
    }

    /// THE REGRESSION GUARD. Publishing through the API used to touch only the
    /// slirp/nft (the ACTUAL state) and leave `ports` empty, so nothing could
    /// replay the publication after an ingress restart. Measured on a live host:
    /// 18 records with `ports: []` while two of those ports were serving HTTP.
    #[tokio::test]
    async fn publicar_pela_api_fica_no_registo() {
        let (st, _d) = test_state();
        let store = ctr_com_ip(&st.base, "aaaa1111bbbb2222", "10.210.0.5");

        record_published_port(
            st.base.clone(),
            "10.210.0.5".to_string(),
            "18099:80".to_string(),
        )
        .await
        .unwrap();

        let c = store.list().unwrap().pop().unwrap();
        assert_eq!(
            c.ports,
            vec!["18099:80".to_string()],
            "a publicação não chegou ao registo — nada a poderia repor num reinício"
        );
    }

    /// Publishing the same spec twice must not stack duplicates: the replay on
    /// start walks `ports` and would try to publish the same host port twice,
    /// the second failing as already in use.
    #[tokio::test]
    async fn publicar_duas_vezes_nao_duplica() {
        let (st, _d) = test_state();
        let store = ctr_com_ip(&st.base, "cccc3333dddd4444", "10.210.0.6");
        for _ in 0..3 {
            record_published_port(
                st.base.clone(),
                "10.210.0.6".to_string(),
                "18100:80".to_string(),
            )
            .await
            .unwrap();
        }
        assert_eq!(store.list().unwrap().pop().unwrap().ports.len(), 1);
    }

    /// An IP that belongs to no record of ours (a VM, a foreign netns) must not
    /// be an error: the ingress addresses workloads by IP and this function
    /// records what it can — it is not a second admission gate.
    #[tokio::test]
    async fn ip_desconhecido_nao_falha_nem_toca_em_ninguem() {
        let (st, _d) = test_state();
        let store = ctr_com_ip(&st.base, "eeee5555ffff6666", "10.210.0.7");
        record_published_port(
            st.base.clone(),
            "10.210.99.99".to_string(),
            "18101:80".to_string(),
        )
        .await
        .expect("um IP alheio não é erro");
        assert!(store.list().unwrap().pop().unwrap().ports.is_empty());
    }

    /// Un-publishing must drop the DESIRED state too, or the next start would
    /// faithfully republish a port the operator just removed.
    #[tokio::test]
    async fn despublicar_tira_do_registo() {
        let (st, _d) = test_state();
        let store = ctr_com_ip(&st.base, "7777aaaa8888bbbb", "10.210.0.8");
        record_published_port(
            st.base.clone(),
            "10.210.0.8".to_string(),
            "18102:80".to_string(),
        )
        .await
        .unwrap();

        let resp = router(st)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/net/publish/18102")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            store.list().unwrap().pop().unwrap().ports.is_empty(),
            "o registo continuou a pedir um porto que já foi retirado"
        );
    }

    #[tokio::test]
    async fn ping_responde() {
        let (st, _d) = test_state();
        let resp = router(st)
            .oneshot(
                Request::builder()
                    .uri("/_ping")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_expoe_o_registo_partilhado() {
        let (st, _d) = test_state();
        let resp = router(st)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/openmetrics-text; version=1.0.0; charset=utf-8"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes);
        // The shared registry's `delonix_build_info` must always be present.
        assert!(body.contains("delonix_build_info"), "corpo: {body}");
    }

    #[tokio::test]
    async fn ciclo_de_vida_de_um_volume() {
        let (st, _d) = test_state();
        let app = router(st);

        // Empty list initially.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/volumes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await.as_array().unwrap().len(), 0);

        // Create a volume.
        let create = Request::builder()
            .method("POST")
            .uri("/v1/volumes")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"dados"}"#))
            .unwrap();
        let resp = app.clone().oneshot(create).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = body_json(resp).await;
        assert_eq!(v["name"], "dados");
        assert_eq!(v["driver"], "local");

        // Shows up in the listing and in the individual GET.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/volumes/dados")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["name"], "dados");

        // Delete.
        let del = Request::builder()
            .method("DELETE")
            .uri("/v1/volumes/dados")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(del).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // GET of a nonexistent volume → 404.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/volumes/nada")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn valid_name_rejeita_traversal() {
        assert!(valid_name("dados"));
        assert!(valid_name("bd-1.snap_2"));
        // Traversal / separators / dot-segments → rejected.
        for bad in ["", ".", "..", "../x", "a/b", "a..b", "..\u{0000}", "/etc"] {
            assert!(!valid_name(bad), "devia rejeitar {bad:?}");
        }
    }

    #[tokio::test]
    async fn delete_com_dot_dot_da_400_e_nao_apaga_nada() {
        let (st, _d) = test_state();
        // A DELETE with `..` in the path must be refused at the boundary (it doesn't
        // reach the store's remove_dir_all — otherwise it would delete the parent dir).
        let del = Request::builder()
            .method("DELETE")
            .uri("/v1/volumes/..")
            .body(Body::empty())
            .unwrap();
        let resp = router(st).oneshot(del).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn containers_lista_vazia_e_get_inexistente_da_404() {
        let (st, _d) = test_state();
        let app = router(st);

        // No containers created → empty list.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/containers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await.as_array().unwrap().len(), 0);

        // GET of a nonexistent container → 404.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/containers/nada")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn containers_devolve_container_populado() {
        use delonix_runtime_core::Container;
        let (st, dir) = test_state();
        // Persist a real container in the store (`<base>/containers`), as the CLI does.
        let store = Store::open(dir.path().join("containers")).unwrap();
        let c = Container::new(
            "abc123def456".to_string(),
            "web".to_string(),
            "nginx:latest".to_string(),
            vec![
                "nginx".to_string(),
                "-g".to_string(),
                "daemon off;".to_string(),
            ],
            "512m".to_string(),
        );
        store.save(&c).unwrap();

        let app = router(st);
        // Shows up in the listing, with the fields intact.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/containers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let arr = body_json(resp).await;
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "web");
        assert_eq!(arr[0]["image"], "nginx:latest");

        // GET by exact id → the same container.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/containers/abc123def456")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = body_json(resp).await;
        assert_eq!(got["id"], "abc123def456");
        assert_eq!(got["name"], "web");
        assert_eq!(got["command"][0], "nginx");
    }

    #[tokio::test]
    async fn imagens_list_e_rmi() {
        use delonix_image::{Image, ImageConfig, ImageStore};
        let (st, dir) = test_state();
        let store = ImageStore::open(dir.path()).unwrap();
        store
            .save(&Image {
                id: "sha256:aabbccddeeff00112233".to_string(),
                repo_tags: vec!["nginx:latest".to_string()],
                layers: vec![],
                config: ImageConfig::default(),
                created_unix: 1,
            })
            .unwrap();

        let app = router(st);
        // The list shows the image.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/images")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let arr = body_json(resp).await;
        assert_eq!(arr.as_array().unwrap().len(), 1);
        assert_eq!(arr[0]["repo_tags"][0], "nginx:latest");

        // rmi by tag (ref goes by query, with `:` and potentially `/`).
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/images?ref=nginx:latest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_json(resp).await["result"]
            .as_str()
            .unwrap()
            .contains("deleted"));

        // No longer exists → rmi again gives 404.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/images?ref=nginx:latest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn mutacao_de_container_valida_o_id_antes_do_exec() {
        let (st, _d) = test_state();
        let app = router(st);
        // `..` and a leading `-` (which the CLI would read as a flag) → 400, no exec. (An
        // `a/b` doesn't even reach the handler — it becomes 2 segments and the router gives 404.)
        for bad in ["..", "-rf"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(format!("/v1/containers/{bad}?force=true"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "delete devia recusar {bad:?}"
            );
        }
        // Unknown action → 400 (allowlist).
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/containers/web/action")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"action":"detonar"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // run: empty image or one starting with `-` (would become a positional flag) → 400.
        for bad in [
            r#"{"image":""}"#,
            r#"{"image":"-rm"}"#,
            r#"{"image":"x","name":"-p"}"#,
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/containers")
                        .header("content-type", "application/json")
                        .body(Body::from(bad))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "run devia recusar {bad}"
            );
        }
        // pull: empty ref or one starting with `-` → 400 (before any exec).
        for bad in [r#"{"ref":""}"#, r#"{"ref":"-x"}"#] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/images/pull")
                        .header("content-type", "application/json")
                        .body(Body::from(bad))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "pull devia recusar {bad}"
            );
        }
        // build: invalid tag or empty Delonixfile → 400 (before writing/exec).
        for bad in [
            r#"{"delonixfile":"FROM x","tag":""}"#,
            r#"{"delonixfile":"FROM x","tag":"-t"}"#,
            r#"{"delonixfile":"","tag":"ok:1"}"#,
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/images/build")
                        .header("content-type", "application/json")
                        .body(Body::from(bad))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "build devia recusar {bad}"
            );
        }
        // network create: invalid name (`-`/empty) → 400.
        for bad in [r#"{"name":""}"#, r#"{"name":"-net"}"#] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/networks")
                        .header("content-type", "application/json")
                        .body(Body::from(bad))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "network create devia recusar {bad}"
            );
        }
    }

    #[test]
    fn build_run_args_usa_net_nao_network_e_respeita_filtros() {
        let spec = RunSpecBody {
            image: "nginx:latest".into(),
            name: "web".into(),
            ports: vec!["8080:80".into(), "mau;porta".into()], // 2nd is filtered out
            env: vec!["K=v".into()],
            network: "minha-rede".into(),
            memory: "256m".into(),
            restart: "always".into(),
            command: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
            volumes: vec!["dados:/var".into(), "mau/../x:/y".into()], // 2nd filtered out
            knows: vec!["db".into()],
            knows_none: false,
        };
        let args = build_run_args(spec);
        // Network flag is `--net=…` (the runtime's), NEVER `--network`.
        assert!(
            args.contains(&"--net=minha-rede".to_string()),
            "args: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--network")),
            "não pode usar --network"
        );
        // Filters preserved: invalid port and volume with `..` fall out.
        assert!(args.contains(&"8080:80".to_string()));
        assert!(!args.iter().any(|a| a.contains("mau;porta")));
        assert!(args.contains(&"dados:/var".to_string()));
        assert!(!args.iter().any(|a| a.contains("..")));
        // The image comes before the command (final positional), and the command after.
        let img = args.iter().position(|a| a == "nginx:latest").unwrap();
        let cmd = args.iter().position(|a| a == "daemon off;").unwrap();
        assert!(img < cmd, "imagem antes do command");
        assert!(args.contains(&"--knows".to_string()) && args.contains(&"db".to_string()));
    }

    #[test]
    fn build_run_args_knows_none_tem_precedencia() {
        let spec = RunSpecBody {
            image: "x".into(),
            name: String::new(),
            ports: vec![],
            env: vec![],
            network: String::new(),
            memory: String::new(),
            restart: String::new(),
            command: vec![],
            volumes: vec![],
            knows: vec!["db".into()],
            knows_none: true,
        };
        let args = build_run_args(spec);
        assert!(args.contains(&"--knows-none".to_string()));
        assert!(
            !args.contains(&"--knows".to_string()),
            "knows-none exclui knows"
        );
    }

    #[tokio::test]
    async fn container_get_com_dot_dot_da_400() {
        let (st, _d) = test_state();
        // `..` in the id path must be refused at the boundary (`Store::load` does
        // `root.join(id)` before the scan — a `..` would escape the root).
        let resp = router(st)
            .oneshot(
                Request::builder()
                    .uri("/v1/containers/..")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn nome_invalido_da_400() {
        let (st, _d) = test_state();
        let create = Request::builder()
            .method("POST")
            .uri("/v1/volumes")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"nome invalido!!"}"#))
            .unwrap();
        let resp = router(st).oneshot(create).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
    /// As rotas de rede novas: listar, obter, e o estado da infra realizada.
    ///
    /// `DELONIX_ROOT` é do PROCESSO, e o `test_state` já dá uma raiz temporária
    /// por teste — mas estas rotas leem pelo `infra`, que resolve a raiz do
    /// ambiente e não do `AppState`. Por isso a raiz é apontada aqui, uma vez.
    #[tokio::test]
    async fn redes_lista_get_e_estado() {
        let (st, d) = test_state();
        let _root = ROOT_ENV.lock().await;
        std::env::set_var("DELONIX_ROOT", d.path());
        let app = router(st);

        // Sem redes declaradas → lista vazia (e não um 500).
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/networks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await.as_array().unwrap().len(), 0);

        // Uma rede que não existe → 404, e não 200 com corpo vazio: quem chama
        // isto está a decidir entre criar e reutilizar.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/networks/nao-existe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Nome inválido é recusado ANTES de tocar no disco.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/networks/..")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Declara uma rede pela biblioteca e confirma que as duas rotas a veem.
        let def = delonix_net::infra::network_create("api-teste").expect("criar rede");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/networks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let lista = body_json(resp).await;
        assert_eq!(lista.as_array().unwrap().len(), 1);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/networks/api-teste")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        assert_eq!(j["name"], "api-teste");
        assert_eq!(j["bridge"], def.bridge);
        assert_eq!(j["prefix"], def.prefix);

        // O estado da infra responde mesmo com o holder em baixo — é a pergunta
        // «está de pé?», e uma resposta de erro aí não se distingue de um nó
        // inacessível.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/net/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    /// A publicação de portos: o que é recusado ANTES de chegar ao mecanismo.
    ///
    /// O caminho feliz não se testa aqui de propósito — publicar exige o holder
    /// de pé, slirp e nft, e um teste que precise disso deixa de correr no CI e
    /// passa a decoração. O que se testa é a fronteira: o que a API deixa passar
    /// para uma regra de DNAT.
    #[tokio::test]
    async fn publish_recusa_ip_e_spec_invalidos() {
        let (st, _d) = test_state();
        let app = router(st);

        // Um IP que não é um IP acabaria numa regra de DNAT arbitrária.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/net/publish")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"container_ip":"nao-e-um-ip","spec":"8080:80"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // E a spec passa pelo mesmo crivo dos outros argumentos.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/net/publish")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"container_ip":"10.200.0.5","spec":"8080:80; rm -rf /"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // O porto do unpublish é um número, e nada mais — é o que identifica a
        // regra a remover.
        for mau in ["abc", "80a", ".."] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(format!("/v1/net/publish/{mau}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "aceitou {mau:?}");
        }
    }
    /// A firewall e a política de saída: o que é recusado ANTES de tocar no nft.
    ///
    /// Como no publish, o caminho feliz não vive aqui — aplicar uma cadeia exige
    /// holder e nft de pé. O que se testa é o que a API deixa passar para uma
    /// regra que decide tráfego.
    #[tokio::test]
    async fn firewall_e_egress_recusam_entrada_invalida() {
        let (st, _d) = test_state();
        let app = router(st);

        let put = |uri: &str, corpo: &'static str| {
            Request::builder()
                .method("PUT")
                .uri(uri.to_string())
                .header("content-type", "application/json")
                .body(Body::from(corpo))
                .unwrap()
        };

        // IP que não é IP: acabaria numa cadeia endereçada a coisa nenhuma.
        let resp = app
            .clone()
            .oneshot(put(
                "/v1/net/firewall/nao-e-ip",
                r#"{"id":"abc","fw":{"enabled":true}}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Id com metacaracteres passa pelo mesmo crivo do resto do módulo.
        let resp = app
            .clone()
            .oneshot(put(
                "/v1/net/firewall/10.200.0.5",
                r#"{"id":"a; rm -rf /","fw":{"enabled":true}}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // O DELETE valida o mesmo IP.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/net/firewall/..")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Bridge inválida no egress por-rede. `..` e não `br; reboot`: o
        // segundo nem chega a ser um URI legal, e o construtor do pedido
        // rejeita-o antes do servidor — um teste assim não testava a API,
        // testava o `http::Request`.
        let resp = app
            .clone()
            .oneshot(put("/v1/net/egress/..", r#"{"deny":true}"#))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // E o `deny` é OBRIGATÓRIO: um corpo sem ele não pode ser lido como
        // «não negar» — cortar a saída e não a cortar são resultados opostos, e
        // um default silencioso escolheria um deles por omissão.
        let resp = app
            .clone()
            .oneshot(put("/v1/net/egress", r#"{}"#))
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::OK, "corpo sem `deny` foi aceite");

        // O limite de banda valida o id.
        let resp = app
            .oneshot(put(
                "/v1/containers/../rate",
                r#"{"rate_bit":1000,"burst_bytes":100}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
    /// Endereços e ligação a redes: o crivo, e o `valid_mac` que existe por
    /// causa dos dois-pontos.
    #[tokio::test]
    async fn dhcp_e_attach_validam_o_que_recebem() {
        let (st, d) = test_state();
        let _root = ROOT_ENV.lock().await;
        std::env::set_var("DELONIX_ROOT", d.path());
        let app = router(st);

        let g = |uri: String| {
            app.clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        };

        // MAC com caracteres que não são hex nem `:`.
        let resp = g("/v1/net/dhcp/app/zz:zz:zz:zz:zz:zz".into())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Um MAC legítimo NÃO pode ser recusado — é o que o `valid_mac` existe
        // para garantir, porque o `valid_arg` do módulo rejeita os dois-pontos.
        // A rede não existe, portanto 404 (e não 400): a distinção é o ponto.
        let resp = g("/v1/net/dhcp/rede-que-nao-existe/aa:bb:cc:dd:ee:ff".into())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // O IP derivado de um id responde sem tocar em estado.
        let resp = g("/v1/net/container-ip/abc123".into()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_json(resp).await["ip"].is_string());

        // Id inválido é recusado.
        let resp = g("/v1/net/container-ip/..".into()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Detach com um IP que não é IP.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/net/attach/abc/nao-e-ip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // E o attach recusa rede inválida antes de tocar no holder.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/net/attach-extra")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"id":"abc","idx":1,"net":"..","namespace":""}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

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

use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use delonix_image::ImageStore;
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
        eprintln!("delonix-mgmt (management API) listening on unix://{path}");
        serve_over_uds(uds, router(AppState { base, bin })).await
    })
}

/// Serves an axum `Router` over a `UnixListener` (`axum::serve` only accepts TCP;
/// here we use the accept loop + hyper-util, the pattern from axum's unix example).
async fn serve_over_uds(uds: tokio::net::UnixListener, app: Router) -> Result<(), Error> {
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tower::Service;
    let mut make = app.into_make_service();
    loop {
        let (socket, _) = uds.accept().await.map_err(|e| Error::Runtime {
            context: "accept",
            message: e.to_string(),
        })?;
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
        // exposes (defined in `delonix-runtime-core::metrics`). Only render, zero new
        // metrics: the control-plane can scrape the engine through this management socket.
        .route("/metrics", get(metrics))
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
        .route("/v1/networks", post(create_network))
        .route("/v1/networks/:name", axum::routing::delete(delete_network))
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
/// `delonix-runtime-core` (the same one `delonix-cri` serves). No metrics of
/// its own: only render, so the control-plane can scrape via the management socket.
async fn metrics() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        delonix_runtime_core::metrics::encode(),
    )
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
fn err_response(e: Error) -> Response {
    let (code, msg) = match e {
        Error::NotFound(m) => (StatusCode::NOT_FOUND, m),
        Error::Invalid(m) => (StatusCode::BAD_REQUEST, m),
        Error::Conflict(m) => (StatusCode::CONFLICT, m),
        other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    };
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
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
}

//! **API de gestão local do Delonix Runtime** (HTTP+JSON sobre um unix socket).
//!
//! É a superfície que um control-plane externo (o `delonix-paas`, via o seu
//! `RemoteRuntime`) consome para operar o motor **sem link directo aos crates** —
//! fala só HTTP com este socket no mesmo host. Complementa o CRI (`delonix-cri`,
//! que serve o kubelet): este serve a *gestão* do produto (volumes/containers/…).
//!
//! As superfícies migram uma de cada vez. Feito: **volumes** (CRUD), **containers**
//! (list/get por biblioteca + rm/start/stop/restart/pause/unpause por shell-out à
//! CLI) e **imagens** (list + rmi). O contrato de LEITURA é o próprio tipo serde de
//! cada recurso (`delonix_volume::Volume`, `delonix_runtime_core::Container`,
//! `delonix_image::Image`); as MUTAÇÕES devolvem `{ok, output}`.

use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use delonix_image::ImageStore;
use delonix_runtime_core::{Error, Store};
use delonix_volume::VolumeStore;

/// Estado partilhado dos handlers.
#[derive(Clone)]
struct AppState {
    /// A raiz do estado do runtime (`$DELONIX_ROOT`).
    base: PathBuf,
    /// O binário da CLI do runtime (`delonix`) para as MUTAÇÕES. Ao contrário das
    /// leituras (chamadas de biblioteca ao Store), uma mutação de container
    /// (rm/stop/start/…) tem de reusar o caminho REAL do motor — matar o processo,
    /// limpar cgroups/namespaces, despublicar portas, desligar redes — que vive na
    /// CLI. Chamar a própria CLI garante paridade total, em vez de reimplementar
    /// essa limpeza aqui. É a mesma decisão que o `InProcessRuntime` do PaaS já
    /// tomava; a arquitectura Runtime-como-serviço só MOVE esse shell-out para aqui.
    bin: PathBuf,
}

/// Arranca a API de gestão a escutar num unix socket (bloqueante). `addr` aceita
/// um caminho ou `unix:///caminho`. Mesmo padrão do `delonix-cri::serve_blocking`.
pub fn serve_blocking(base: PathBuf, addr: &str) -> Result<(), Error> {
    // O binário para as mutações é o PRÓPRIO executável (este processo É o
    // `delonix api`); fallback para "delonix" no PATH se `current_exe` falhar.
    let bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("delonix"));
    serve_blocking_with(base, bin, addr)
}

/// Como [`serve_blocking`], mas com o binário da CLI explícito (para testes).
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
        let _ = std::fs::remove_file(&path); // limpa um socket antigo
        let uds = tokio::net::UnixListener::bind(&path).map_err(|e| Error::Runtime {
            context: "bind",
            message: e.to_string(),
        })?;
        eprintln!("delonix-mgmt (API de gestão) a escutar em unix://{path}");
        serve_over_uds(uds, router(AppState { base, bin })).await
    })
}

/// Serve um `Router` axum sobre um `UnixListener` (o `axum::serve` só aceita TCP;
/// aqui usa-se o loop de accept + hyper-util, o padrão do exemplo unix do axum).
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
        // `into_make_service` é infalível → o service da ligação nunca falha aqui.
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

/// As rotas da API. Exposta para testes (sem socket, via `oneshot`).
fn router(state: AppState) -> Router {
    Router::new()
        .route("/_ping", get(ping))
        .route("/v1/volumes", get(list_volumes).post(create_volume))
        .route("/v1/volumes/:name", get(get_volume).delete(delete_volume))
        // Containers: leitura (list/get) por biblioteca; mutação (abaixo) por CLI.
        // `POST` = `run` (recebe o spec em JSON e reconstrói os args da CLI).
        .route("/v1/containers", get(list_containers).post(run_container))
        .route(
            "/v1/containers/:id",
            get(get_container).delete(delete_container),
        )
        // Mutação de container: rm/start/stop/restart/pause/unpause — shell-out à
        // CLI do runtime (paridade total de limpeza), não uma chamada ao Store.
        .route("/v1/containers/:id/action", post(container_action_ep))
        // Logs (request/response, não streaming) + exec não-interactivo.
        .route("/v1/containers/:id/logs", get(container_logs_ep))
        .route("/v1/containers/:id/exec", post(container_exec_ep))
        // Imagens: list + rmi. A referência (`nginx:latest`, `library/nginx`,
        // `sha256:…`) NÃO cabe num segmento de path (tem `/` e `:`) → vai por
        // query (`?ref=…`). Não há risco de traversal: `ImageStore::remove`
        // resolve por varrimento linear (compara tags/prefixo de id) e o ficheiro
        // que apaga usa o `id` sanitizado, nunca o `ref` cru.
        .route("/v1/images", get(list_images).delete(delete_image))
        // Pull (opcionalmente com scan de CVE a seguir) — shell-out à CLI.
        .route("/v1/images/pull", post(pull_image))
        // Build a partir de um Delonixfile colado (materializa + `delonix build`).
        .route("/v1/images/build", post(build_image))
        .with_state(state)
}

async fn ping() -> &'static str {
    "delonix-mgmt ok"
}

/// Nome de volume seguro no LIMITE da API (defesa contra path traversal). É
/// deliberadamente MAIS estrito que o `VolumeStore`: este aceita `..` (só carateres
/// `.`) e o `inspect`/`remove` nem validam o nome — um `remove("..")` vindo do path
/// da URL apagaria o diretório-pai. Aqui rejeita-se qualquer `..`/`/`/`.` sozinho.
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

/// Erro 400 padrão para um nome de volume inválido.
fn invalid_name() -> Response {
    err_response(Error::Invalid("nome de volume inválido".to_string()))
}

/// Mapeia um `Error` do motor para (código HTTP, corpo JSON) — o cliente
/// reconstrói o seu próprio `RuntimeError` a partir do código + mensagem.
fn err_response(e: Error) -> Response {
    let (code, msg) = match e {
        Error::NotFound(m) => (StatusCode::NOT_FOUND, m),
        Error::Invalid(m) => (StatusCode::BAD_REQUEST, m),
        Error::Conflict(m) => (StatusCode::CONFLICT, m),
        other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    };
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Corre uma operação síncrona do `VolumeStore` numa thread de bloqueio (o store
/// é síncrono; não deve bloquear o executor async).
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

/// Corpo de `POST /v1/volumes`.
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

// ---- Containers (leitura) --------------------------------------------------

/// Corre uma operação síncrona do `Store` de containers numa thread de bloqueio.
/// O store vive em `<base>/containers` (mesma resolução que a CLI usa em
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
    // Mesma defesa de fronteira dos volumes: o `Store::load` faz `root.join(id)`
    // antes de cair no varrimento por nome/prefixo — um `..` no path escaparia.
    if !valid_name(&id) {
        return err_response(Error::Invalid("id de container inválido".to_string()));
    }
    match with_container_store(s.base, move |store| store.load(&id)).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => err_response(e),
    }
}

/// Argumento seguro para passar à CLI: além do `valid_name` (sem `..`/`/`), recusa
/// um `-` inicial — senão o `clap` da CLI interpretaria o id como uma flag (ex.: um
/// id `--rm`). Os args da CLI não sofrem injecção de shell (`Command::args`, não uma
/// string), mas podem ser lidos como opções — daí a barreira contra `-`.
fn valid_arg(s: &str) -> bool {
    valid_name(s) && !s.starts_with('-')
}

/// Corre a CLI do runtime (`delonix …`) com `DELONIX_ROOT` na base, e devolve
/// `(sucesso, saída combinada)`. Bloqueante → corre em `spawn_blocking`.
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

/// Query de `DELETE /v1/containers/:id?force=<bool>`.
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
        return err_response(Error::Invalid("id de container inválido".to_string()));
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

/// Corpo de `POST /v1/containers/:id/action`.
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
        return err_response(Error::Invalid("id de container inválido".to_string()));
    }
    // Só acções conhecidas (allowlist) chegam à CLI. `remove` = `rm -f`.
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
        other => return err_response(Error::Invalid(format!("acção desconhecida: {other}"))),
    };
    match run_cli(s.bin, s.base, vec!["container".to_string(), sub, id]).await {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn container_logs_ep(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    if !valid_arg(&id) {
        return err_response(Error::Invalid("id de container inválido".to_string()));
    }
    // `logs` request/response (não streaming); a saída vem tal e qual, mesmo se o
    // container não existir (o cliente ignora o `ok`, como o InProcessRuntime).
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

/// Corpo de `POST /v1/containers/:id/exec`.
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
        return err_response(Error::Invalid("id de container inválido".to_string()));
    }
    // `exec <id> sh -c <cmd>`: o `cmd` é passado como UM argumento a `sh -c` DENTRO
    // do container — corre no container, nunca no shell do host (é `Command::args`,
    // sem shell nossa). Exec é, por natureza, execução arbitrária no container.
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

/// Corpo de `POST /v1/containers` (run). Espelha o `ContainerRunSpec` do PaaS — o
/// contrato são os nomes dos campos (o PaaS serializa o seu spec, este desserializa).
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

/// Reconstrói os args `delonix container run -d …` a partir do spec — função PURA
/// (testável sem kernel). Os filtros são os MESMOS que o `InProcessRuntime` do PaaS
/// já usava; a única diferença de nome de flag é deliberada: o binário do runtime
/// usa `--net` (o do PaaS, com shim docker, usava `--network`).
fn build_run_args(spec: RunSpecBody) -> Vec<String> {
    let mut args: Vec<String> = vec!["container".into(), "run".into(), "-d".into()];
    if !spec.name.is_empty() {
        args.push("--name".into());
        args.push(spec.name);
    }
    if !spec.network.is_empty() && spec.network != "none" {
        // O CLI do runtime usa `--net` (não `--network`). Forma `--net=<v>` para o
        // valor nunca escapar para um token novo.
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
    // `image` é obrigatória e um valor começado por `-` seria lido pelo clap como
    // uma flag (é o argumento POSICIONAL final) — recusa no limite. O mesmo para
    // `name` (valor de `--name`). Os restantes campos ou têm charset próprio
    // (ports) ou são valores de opção sem ambiguidade posicional.
    if spec.image.is_empty() || spec.image.starts_with('-') {
        return err_response(Error::Invalid("imagem inválida".to_string()));
    }
    if spec.name.starts_with('-') {
        return err_response(Error::Invalid("nome inválido".to_string()));
    }
    match run_cli(s.bin, s.base, build_run_args(spec)).await {
        Ok((ok, out)) => Json(serde_json::json!({ "ok": ok, "output": out })).into_response(),
        Err(e) => err_response(e),
    }
}

// ---- Imagens (list + rmi) --------------------------------------------------

/// Corre uma operação síncrona do `ImageStore` numa thread de bloqueio. O store
/// resolve `<base>/images` internamente (recebe a base, como o `VolumeStore`).
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

/// Query de `DELETE /v1/images?ref=…`. `ref` é palavra-reservada em Rust.
#[derive(serde::Deserialize)]
struct RefQuery {
    #[serde(rename = "ref")]
    reference: String,
}

async fn delete_image(State(s): State<AppState>, Query(q): Query<RefQuery>) -> Response {
    if q.reference.is_empty() {
        return err_response(Error::Invalid("referência de imagem vazia".to_string()));
    }
    match with_image_store(s.base, move |store| store.remove(&q.reference)).await {
        // `remove` devolve "untagged: …" ou "deleted: …" — devolve-o tal e qual.
        Ok(result) => Json(serde_json::json!({ "result": result })).into_response(),
        Err(e) => err_response(e),
    }
}

/// Corpo de `POST /v1/images/pull`.
#[derive(serde::Deserialize)]
struct PullBody {
    #[serde(rename = "ref")]
    reference: String,
    /// Corre também um scan de CVE depois do pull (e anexa a saída).
    #[serde(default)]
    scan_after: bool,
}

async fn pull_image(State(s): State<AppState>, Json(b): Json<PullBody>) -> Response {
    // A referência é o argumento POSICIONAL de `image pull` — um `-` inicial seria
    // lido como flag. Recusa no limite. (Refs válidas têm `/`/`:`, nunca `-` no
    // início.)
    if b.reference.is_empty() || b.reference.starts_with('-') {
        return err_response(Error::Invalid("referência de imagem inválida".to_string()));
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
    // Auto-scan opcional a seguir ao pull (só se o pull correu bem).
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

/// Corpo de `POST /v1/images/build`.
#[derive(serde::Deserialize)]
struct BuildBody {
    /// Conteúdo do Delonixfile (colado; os `RUN` correm durante o build).
    delonixfile: String,
    /// Tag da imagem resultante (`repo:tag`).
    tag: String,
}

/// Contador monótono por-processo para nomear work dirs de build únicos.
static BUILD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn build_image(State(s): State<AppState>, Json(b): Json<BuildBody>) -> Response {
    // `-t <tag>`: um valor começado por `-` seria lido como flag. Recusa no limite.
    if b.tag.is_empty() || b.tag.starts_with('-') {
        return err_response(Error::Invalid("tag inválida".to_string()));
    }
    if b.delonixfile.trim().is_empty() {
        return err_response(Error::Invalid("Delonixfile vazio".to_string()));
    }
    // Work dir ÚNICO por-build (contexto onde o `COPY` resolve): `pid-seq` isola
    // builds concorrentes — sem isto, dois builds em paralelo partilhariam o
    // `Delonixfile`/contexto (TOCTOU: um constrói o Delonixfile do outro). Limpo no
    // fim. O nome deriva só de `s.base`+pid+contador — nunca de input do utilizador.
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
    // Limpa o work dir (best-effort) — não deixa Delonixfiles/contextos a acumular.
    let dir_c = dir.clone();
    let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&dir_c)).await;
    match result {
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
                // `/bin/false`: as mutações reais provam-se no e2e cross-processo;
                // os testes de unidade cobrem só a validação (recusa ANTES do exec).
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
    async fn ciclo_de_vida_de_um_volume() {
        let (st, _d) = test_state();
        let app = router(st);

        // Lista vazia inicialmente.
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

        // Cria um volume.
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

        // Aparece na listagem e no GET individual.
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

        // Apaga.
        let del = Request::builder()
            .method("DELETE")
            .uri("/v1/volumes/dados")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(del).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // GET de um volume inexistente → 404.
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
        // Traversal / separadores / dot-segments → rejeitados.
        for bad in ["", ".", "..", "../x", "a/b", "a..b", "..\u{0000}", "/etc"] {
            assert!(!valid_name(bad), "devia rejeitar {bad:?}");
        }
    }

    #[tokio::test]
    async fn delete_com_dot_dot_da_400_e_nao_apaga_nada() {
        let (st, _d) = test_state();
        // Um DELETE com `..` no path tem de ser recusado no limite (não chega ao
        // remove_dir_all do store — senão apagava o diretório-pai).
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

        // Sem containers criados → lista vazia.
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

        // GET de um container inexistente → 404.
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
        // Persiste um container real no store (`<base>/containers`), como a CLI faz.
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
        // Aparece na listagem, com os campos intactos.
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

        // GET por id exacto → o mesmo container.
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
        // Lista mostra a imagem.
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

        // rmi por tag (ref vai por query, com `:` e potencialmente `/`).
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

        // Já não existe → rmi de novo dá 404.
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
        // `..` e um `-` inicial (que a CLI leria como flag) → 400, sem exec. (Um
        // `a/b` nem chega ao handler — vira 2 segmentos e o router dá 404.)
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
        // Acção desconhecida → 400 (allowlist).
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

        // run: imagem vazia ou começada por `-` (viraria flag posicional) → 400.
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
        // pull: ref vazia ou começada por `-` → 400 (antes de qualquer exec).
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
        // build: tag inválida ou Delonixfile vazio → 400 (antes de escrever/exec).
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
    }

    #[test]
    fn build_run_args_usa_net_nao_network_e_respeita_filtros() {
        let spec = RunSpecBody {
            image: "nginx:latest".into(),
            name: "web".into(),
            ports: vec!["8080:80".into(), "mau;porta".into()], // 2.ª é filtrada
            env: vec!["K=v".into()],
            network: "minha-rede".into(),
            memory: "256m".into(),
            restart: "always".into(),
            command: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
            volumes: vec!["dados:/var".into(), "mau/../x:/y".into()], // 2.ª filtrada
            knows: vec!["db".into()],
            knows_none: false,
        };
        let args = build_run_args(spec);
        // Flag de rede é `--net=…` (do runtime), NUNCA `--network`.
        assert!(
            args.contains(&"--net=minha-rede".to_string()),
            "args: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--network")),
            "não pode usar --network"
        );
        // Filtros preservados: porta inválida e volume com `..` caem fora.
        assert!(args.contains(&"8080:80".to_string()));
        assert!(!args.iter().any(|a| a.contains("mau;porta")));
        assert!(args.contains(&"dados:/var".to_string()));
        assert!(!args.iter().any(|a| a.contains("..")));
        // A imagem vem antes do command (posicional final), e o command a seguir.
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
        // `..` no path do id tem de ser recusado no limite (o `Store::load` faz
        // `root.join(id)` antes do varrimento — um `..` escaparia da raiz).
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

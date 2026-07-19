//! **API de gestão local do Delonix Runtime** (HTTP+JSON sobre um unix socket).
//!
//! É a superfície que um control-plane externo (o `delonix-paas`, via o seu
//! `RemoteRuntime`) consome para operar o motor **sem link directo aos crates** —
//! fala só HTTP com este socket no mesmo host. Complementa o CRI (`delonix-cri`,
//! que serve o kubelet): este serve a *gestão* do produto (volumes/containers/…).
//!
//! As superfícies migram uma de cada vez. Feito: **volumes** (CRUD) e a **leitura
//! de containers** (list/get). O contrato é o próprio tipo serde de cada recurso
//! (`delonix_volume::Volume`, `delonix_runtime_core::Container`) — o cliente
//! desserializa o mesmo shape.

use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use delonix_runtime_core::{Error, Store};
use delonix_volume::VolumeStore;

/// Estado partilhado dos handlers: a raiz do estado do runtime (`$DELONIX_ROOT`).
#[derive(Clone)]
struct AppState {
    base: PathBuf,
}

/// Arranca a API de gestão a escutar num unix socket (bloqueante). `addr` aceita
/// um caminho ou `unix:///caminho`. Mesmo padrão do `delonix-cri::serve_blocking`.
pub fn serve_blocking(base: PathBuf, addr: &str) -> Result<(), Error> {
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
        serve_over_uds(uds, router(AppState { base })).await
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
        // Containers: fatia de LEITURA (list/get). As mutações (rm/start/stop/…)
        // não são simples chamadas ao Store — precisam do caminho real do motor
        // (matar o processo, limpar namespaces/cgroups) — migram numa fatia própria.
        .route("/v1/containers", get(list_containers))
        .route("/v1/containers/:id", get(get_container))
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

//! `delonix-cri` — Kubernetes **CRI** (Container Runtime Interface) server.
//!
//! Implements the `runtime.v1` gRPC (RuntimeService + ImageService) over the
//! Delonix engine, so that a **kubelet** (or `crictl`) can use Delonix as the
//! node runtime. The stubs are generated from `proto/api.proto` (CRI v1, no
//! gogoproto). C2.
//!
//! The tonic gRPC pattern returns `Result<Response<T>, Status>`; `Status` is
//! large by nature, so we silence `result_large_err` across the whole crate.
#![allow(clippy::result_large_err)]

use std::path::PathBuf;
use tonic::{Request, Response, Status};

pub mod cri {
    #![allow(clippy::all)]
    tonic::include_proto!("runtime.v1");
}

use cri::image_service_server::{ImageService, ImageServiceServer};
use cri::runtime_service_server::RuntimeServiceServer;
use cri::*;

mod runtime_svc;
pub mod spdy;
pub mod streaming;

const RUNTIME_NAME: &str = "delonix";
const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Generic error → gRPC `Status`.
fn st<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

/// Resolves the `delonix` CLI binary (the one the CRI delegates the lifecycle to
/// — single-threaded). NEVER `current_exe()`, which is `delonix-cri` itself:
/// reinvoking it would fall back into [`serve_blocking`], which does
/// `remove_file`+`bind` on the socket and STEALS it from the server (the client
/// sees "malformed header: missing HTTP content-type"). Order: (1) explicit
/// `DELONIX_BIN`; (2) a `delonix` sibling of the executable (the golden image
/// installs both in `/usr/local/bin`; a dev build has both in
/// `target/<profile>/`); (3) `delonix` on the PATH.
pub(crate) fn cli_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("DELONIX_BIN") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(sib) = exe.parent().map(|d| d.join("delonix")) {
            if sib.is_file() {
                return sib;
            }
        }
    }
    PathBuf::from("delonix")
}

/// Opens the image store at the given root.
fn images(base: &PathBuf) -> Result<delonix_image::ImageStore, Status> {
    delonix_image::ImageStore::open(base).map_err(st)
}

// ---------------------------------------------------------------------------
// ImageService — fully functional over the Delonix engine.
// ---------------------------------------------------------------------------

pub struct DelonixImage {
    pub base: PathBuf,
}

#[tonic::async_trait]
impl ImageService for DelonixImage {
    async fn list_images(
        &self,
        _req: Request<ListImagesRequest>,
    ) -> Result<Response<ListImagesResponse>, Status> {
        let base = self.base.clone();
        let list = tokio::task::spawn_blocking(move || images(&base)?.list().map_err(st))
            .await
            .map_err(st)??;
        let images = list
            .into_iter()
            .map(|i| {
                let (uid, username) = image_user(&i.config.user);
                Image {
                    id: i.id.clone(),
                    repo_tags: i.repo_tags.clone(),
                    repo_digests: vec![],
                    size: layer_size(&i),
                    uid,
                    username,
                    spec: Some(ImageSpec {
                        image: i.id.clone(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            })
            .collect();
        Ok(Response::new(ListImagesResponse { images }))
    }

    async fn image_status(
        &self,
        req: Request<ImageStatusRequest>,
    ) -> Result<Response<ImageStatusResponse>, Status> {
        let name = req.into_inner().image.map(|s| s.image).unwrap_or_default();
        let base = self.base.clone();
        let found = tokio::task::spawn_blocking(move || images(&base).ok()?.resolve(&name).ok())
            .await
            .map_err(st)?;
        let image = found.map(|i| {
            let (uid, username) = image_user(&i.config.user);
            Image {
                id: i.id.clone(),
                repo_tags: i.repo_tags.clone(),
                size: layer_size(&i),
                uid,
                username,
                spec: Some(ImageSpec {
                    image: i.id.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            }
        });
        Ok(Response::new(ImageStatusResponse {
            image,
            info: Default::default(),
        }))
    }

    async fn pull_image(
        &self,
        req: Request<PullImageRequest>,
    ) -> Result<Response<PullImageResponse>, Status> {
        let r = req.into_inner();
        let name = r.image.map(|s| s.image).unwrap_or_default();
        if name.is_empty() {
            return Err(Status::invalid_argument("empty image"));
        }
        // `AuthConfig` comes from the Pod's `imagePullSecrets` (the kubelet
        // resolves them and sends them here) — WITHOUT this, any private registry
        // fails because `pull_from_registry` only uses local credentials
        // (`delonix login` on the node itself, which usually does not have the
        // tenant's credentials). Only `username`+`password` are supported for now
        // (the base CRI schema); `identity_token`/`registry_token`/`auth`
        // (already-combined base64) are left for when a real case that needs them
        // shows up.
        let creds = r
            .auth
            .filter(|a| !a.username.is_empty())
            .map(|a| (a.username, a.password));
        let base = self.base.clone();
        let img = tokio::task::spawn_blocking(move || {
            let store = images(&base)?;
            delonix_image::pull_from_registry_with_creds(&store, &name, creds).map_err(st)
        })
        .await
        .map_err(st)??;
        delonix_runtime_core::metrics::inc_image_pulled();
        Ok(Response::new(PullImageResponse { image_ref: img.id }))
    }

    async fn remove_image(
        &self,
        req: Request<RemoveImageRequest>,
    ) -> Result<Response<RemoveImageResponse>, Status> {
        let name = req.into_inner().image.map(|s| s.image).unwrap_or_default();
        let base = self.base.clone();
        // CRI: RemoveImage is IDEMPOTENT — removing a nonexistent image is success
        // (the kubelet calls it in GC cycles without guaranteeing it still exists).
        tokio::task::spawn_blocking(move || {
            if let Ok(store) = images(&base) {
                let _ = store.remove(&name);
            }
        })
        .await
        .map_err(st)?;
        Ok(Response::new(RemoveImageResponse {}))
    }

    async fn image_fs_info(
        &self,
        _req: Request<ImageFsInfoRequest>,
    ) -> Result<Response<ImageFsInfoResponse>, Status> {
        let base = self.base.clone();
        let used = tokio::task::spawn_blocking(move || {
            images(&base)
                .ok()
                .and_then(|s| s.list().ok())
                .map(|v| v.iter().map(layer_size).sum::<u64>())
                .unwrap_or(0)
        })
        .await
        .map_err(st)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let fs = FilesystemUsage {
            timestamp: now,
            fs_id: Some(FilesystemIdentifier {
                mountpoint: self.base.to_string_lossy().into_owned(),
            }),
            used_bytes: Some(UInt64Value { value: used }),
            inodes_used: Some(UInt64Value { value: 0 }),
        };
        Ok(Response::new(ImageFsInfoResponse {
            image_filesystems: vec![fs.clone()],
            container_filesystems: vec![fs],
        }))
    }
}

/// Size (bytes) of an image's layers, summing the CAS blobs.
/// Maps the image OCI config's `user` to the `uid`/`username` fields of the CRI
/// `Image` (`crictl`/kubelet use them to validate RunAsNonRoot). Rule:
/// `""` (no USER) → root, `uid = 0`; numeric `"uid[:gid]"` → `uid`; a name →
/// `username` (resolved at runtime against the image's `/etc/passwd`). The
/// conformance spec `image status … should not have Uid|Username empty` requires
/// that ONE of the two be filled — before, both came back empty.
fn image_user(user: &str) -> (Option<Int64Value>, String) {
    let u = user.trim();
    if u.is_empty() {
        return (Some(Int64Value { value: 0 }), String::new());
    }
    let uid_part = u.split(':').next().unwrap_or(u);
    match uid_part.parse::<i64>() {
        Ok(uid) => (Some(Int64Value { value: uid }), String::new()),
        Err(_) => (None, u.to_string()),
    }
}

fn layer_size(img: &delonix_image::Image) -> u64 {
    let mut total = 0u64;
    for l in &img.layers {
        let hex = l.strip_prefix("sha256:").unwrap_or(l);
        // the blob path is <root>/blobs/sha256/<hex>; we use the store via path.
        if let Ok(meta) = std::fs::metadata(blob_path(img, hex)) {
            total += meta.len();
        }
    }
    total
}

fn blob_path(_img: &delonix_image::Image, hex: &str) -> PathBuf {
    delonix_image::ImageStore::default_root()
        .join("blobs")
        .join("sha256")
        .join(hex)
}

// ---------------------------------------------------------------------------
// Server startup (unix socket), with both services.
// ---------------------------------------------------------------------------

/// Starts the CRI server on a **unix socket** (`addr` = path, or
/// `unix:///path`). Blocks the thread (creates the Tokio runtime).
/// `GET /metrics` — body in OpenMetrics format (what `prometheus-client`
/// produces), from the shared registry in `delonix-runtime-core`.
async fn metrics_handler() -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        delonix_runtime_core::metrics::encode(),
    )
}

pub fn serve_blocking(base: PathBuf, addr: &str) -> Result<(), delonix_runtime_core::Error> {
    let path = addr.strip_prefix("unix://").unwrap_or(addr).to_string();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| delonix_runtime_core::Error::Runtime {
            context: "tokio",
            message: e.to_string(),
        })?;
    rt.block_on(async move {
        let _ = std::fs::remove_file(&path); // clean up an old socket
        let uds = tokio::net::UnixListener::bind(&path).map_err(|e| {
            delonix_runtime_core::Error::Runtime {
                context: "bind",
                message: e.to_string(),
            }
        })?;
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(uds);
        eprintln!("delonix-cri (CRI v1) listening on unix://{path}");

        // Streaming server (exec/attach/port-forward): HTTP/WebSocket on a
        // loopback port. The RPCs return URLs pointing here.
        let stream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| delonix_runtime_core::Error::Runtime {
                context: "bind-stream",
                message: e.to_string(),
            })?;
        let stream_port = stream_listener
            .local_addr()
            .map(|a| a.port())
            .map_err(|e| delonix_runtime_core::Error::Runtime {
                context: "stream-addr",
                message: e.to_string(),
            })?;
        let advertised = format!("http://127.0.0.1:{stream_port}");
        let streamer = streaming::Streamer::new(base.clone(), advertised.clone());
        eprintln!("delonix-cri: streaming (exec/attach) at {advertised}");
        let app = streamer.clone().router();
        tokio::spawn(async move {
            let _ = axum::serve(stream_listener, app).await;
        });

        // Prometheus metrics (OPTIONAL): a dedicated HTTP listener, like
        // containerd/CRI-O. Off by default; enabled by `DELONIX_METRICS_ADDR`
        // (e.g. `0.0.0.0:9100`). Does not live on the gRPC socket — Prometheus
        // speaks HTTP.
        if let Some(maddr) = std::env::var_os("DELONIX_METRICS_ADDR") {
            let maddr = maddr.to_string_lossy().into_owned();
            tokio::spawn(async move {
                match tokio::net::TcpListener::bind(&maddr).await {
                    Ok(l) => {
                        tracing::info!(addr = %maddr, "delonix-cri: /metrics listening");
                        let app = axum::Router::new()
                            .route("/metrics", axum::routing::get(metrics_handler));
                        let _ = axum::serve(l, app).await;
                    }
                    Err(e) => {
                        tracing::error!(addr = %maddr, error = %e, "delonix-cri: /metrics bind failed")
                    }
                }
            });
        }

        let img = DelonixImage { base: base.clone() };
        let rtsvc = runtime_svc::DelonixRuntime::new(base, streamer);
        tonic::transport::Server::builder()
            .add_service(RuntimeServiceServer::new(rtsvc))
            .add_service(ImageServiceServer::new(img))
            .serve_with_incoming(incoming)
            .await
            .map_err(|e| delonix_runtime_core::Error::Runtime {
                context: "serve",
                message: e.to_string(),
            })
    })
}

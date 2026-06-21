//! `delonix-cri` — servidor **CRI** (Container Runtime Interface) do Kubernetes.
//!
//! Implementa o gRPC `runtime.v1` (RuntimeService + ImageService) sobre o engine
//! Delonix, para que um **kubelet** (ou o `crictl`) use o Delonix como runtime de
//! nó. Os stubs são gerados de `proto/api.proto` (CRI v1, sem gogoproto). C2.
//!
//! O padrão gRPC do tonic devolve `Result<Response<T>, Status>`; o `Status` é
//! grande por natureza, logo silenciamos `result_large_err` em toda a crate.
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
pub mod streaming;
pub mod spdy;

const RUNTIME_NAME: &str = "delonix";
const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Erro genérico → `Status` gRPC.
fn st<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

/// Abre o armazém de imagens na raiz dada.
fn images(base: &PathBuf) -> Result<delonix_image::ImageStore, Status> {
    delonix_image::ImageStore::open(base).map_err(st)
}

// ---------------------------------------------------------------------------
// ImageService — totalmente funcional sobre o engine Delonix.
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
            .map(|i| Image {
                id: i.id.clone(),
                repo_tags: i.repo_tags.clone(),
                repo_digests: vec![],
                size: layer_size(&i),
                spec: Some(ImageSpec { image: i.id.clone(), ..Default::default() }),
                ..Default::default()
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
        let image = found.map(|i| Image {
            id: i.id.clone(),
            repo_tags: i.repo_tags.clone(),
            size: layer_size(&i),
            spec: Some(ImageSpec { image: i.id.clone(), ..Default::default() }),
            ..Default::default()
        });
        Ok(Response::new(ImageStatusResponse { image, info: Default::default() }))
    }

    async fn pull_image(
        &self,
        req: Request<PullImageRequest>,
    ) -> Result<Response<PullImageResponse>, Status> {
        let name = req.into_inner().image.map(|s| s.image).unwrap_or_default();
        if name.is_empty() {
            return Err(Status::invalid_argument("image vazia"));
        }
        let base = self.base.clone();
        let img = tokio::task::spawn_blocking(move || {
            let store = images(&base)?;
            delonix_image::pull_from_registry(&store, &name).map_err(st)
        })
        .await
        .map_err(st)??;
        Ok(Response::new(PullImageResponse { image_ref: img.id }))
    }

    async fn remove_image(
        &self,
        req: Request<RemoveImageRequest>,
    ) -> Result<Response<RemoveImageResponse>, Status> {
        let name = req.into_inner().image.map(|s| s.image).unwrap_or_default();
        let base = self.base.clone();
        // CRI: RemoveImage é IDEMPOTENTE — remover uma imagem inexistente é sucesso
        // (o kubelet chama-o em ciclos de GC sem garantir que ainda existe).
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
            fs_id: Some(FilesystemIdentifier { mountpoint: self.base.to_string_lossy().into_owned() }),
            used_bytes: Some(UInt64Value { value: used }),
            inodes_used: Some(UInt64Value { value: 0 }),
        };
        Ok(Response::new(ImageFsInfoResponse {
            image_filesystems: vec![fs.clone()],
            container_filesystems: vec![fs],
        }))
    }
}

/// Tamanho (bytes) dos layers de uma imagem, somando os blobs do CAS.
fn layer_size(img: &delonix_image::Image) -> u64 {
    let mut total = 0u64;
    for l in &img.layers {
        let hex = l.strip_prefix("sha256:").unwrap_or(l);
        // o caminho do blob é <root>/blobs/sha256/<hex>; usamos o store via path.
        if let Ok(meta) = std::fs::metadata(blob_path(img, hex)) {
            total += meta.len();
        }
    }
    total
}

fn blob_path(_img: &delonix_image::Image, hex: &str) -> PathBuf {
    delonix_image::ImageStore::default_root().join("blobs").join("sha256").join(hex)
}

// ---------------------------------------------------------------------------
// Arranque do servidor (unix socket), com ambos os serviços.
// ---------------------------------------------------------------------------

/// Arranca o servidor CRI num **unix socket** (`addr` = caminho, ou
/// `unix:///caminho`). Bloqueia a thread (cria o runtime Tokio).
pub fn serve_blocking(base: PathBuf, addr: &str) -> Result<(), delonix_core::Error> {
    let path = addr.strip_prefix("unix://").unwrap_or(addr).to_string();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| delonix_core::Error::Runtime { context: "tokio", message: e.to_string() })?;
    rt.block_on(async move {
        let _ = std::fs::remove_file(&path); // limpa um socket antigo
        let uds = tokio::net::UnixListener::bind(&path)
            .map_err(|e| delonix_core::Error::Runtime { context: "bind", message: e.to_string() })?;
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(uds);
        eprintln!("delonix-cri (CRI v1) a escutar em unix://{path}");

        // Servidor de streaming (exec/attach/port-forward): HTTP/WebSocket numa
        // porta de loopback. As RPCs devolvem URLs que apontam para cá.
        let stream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| delonix_core::Error::Runtime { context: "bind-stream", message: e.to_string() })?;
        let stream_port = stream_listener
            .local_addr()
            .map(|a| a.port())
            .map_err(|e| delonix_core::Error::Runtime { context: "stream-addr", message: e.to_string() })?;
        let advertised = format!("http://127.0.0.1:{stream_port}");
        let streamer = streaming::Streamer::new(base.clone(), advertised.clone());
        eprintln!("delonix-cri: streaming (exec/attach) em {advertised}");
        let app = streamer.clone().router();
        tokio::spawn(async move {
            let _ = axum::serve(stream_listener, app).await;
        });

        let img = DelonixImage { base: base.clone() };
        let rtsvc = runtime_svc::DelonixRuntime::new(base, streamer);
        tonic::transport::Server::builder()
            .add_service(RuntimeServiceServer::new(rtsvc))
            .add_service(ImageServiceServer::new(img))
            .serve_with_incoming(incoming)
            .await
            .map_err(|e| delonix_core::Error::Runtime { context: "serve", message: e.to_string() })
    })
}

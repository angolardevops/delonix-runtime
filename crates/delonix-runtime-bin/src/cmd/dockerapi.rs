//! `delonix docker-api` — a READ-ONLY slice of the Docker Engine API
//! (`/_ping`, `/version`, `/info`, `/containers/json`, `/images/json`), enough
//! for `docker version`/`docker ps`/`docker images`/`docker info` pointed at
//! this socket via `DOCKER_HOST=unix://<path>` to work.
//!
//! **Verified against a REAL `docker` CLI** (27.3.1, downloaded for exactly
//! this), not just the published spec: captured the exact wire protocol —
//! `HEAD /_ping` first, negotiates the API version from THIS server's
//! `Api-Version` response header (not the client's own bundled max), then
//! `/v<version>/...` for everything else. `strip_version_prefix` below
//! handles that; `/_ping` itself is always unversioned.
//!
//! **Mutations are NOT here yet** (`create`/`start`/`stop`/`exec`, which is
//! what `docker run`/`docker compose up` need) — deliberately out of scope
//! for this pass; see `docs/COMPARACAO-DOCKER-PODMAN.md` for the follow-up.
//! Any route this layer doesn't implement returns 404 with a clear message
//! rather than a confusing client-side parse error.
//!
//! Same security posture as `delonix-mgmt`: 0600 socket + `SO_PEERCRED`
//! (own-uid only). A real `docker.sock` is usually group-readable (the
//! `docker` group) — same-uid-only is the safer default and consistent with
//! every other control socket in this codebase; an operator who wants group
//! access can `chmod`/`chgrp` it themselves after the fact.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;

use delonix_image::ImageStore;
use delonix_runtime_core::{Container, Error, Result, Status, Store};

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

pub fn run(addr: Option<String>) -> Result<()> {
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
        eprintln!(
            "delonix-docker-api (Docker Engine API, read-only) listening on unix://{path}"
        );
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

async fn handle(
    req: Request<Incoming>,
    state: Arc<AppState>,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    let path = strip_version_prefix(req.uri().path()).to_string();
    let method = req.method().as_str();
    let (status, body): (StatusCode, Vec<u8>) = match (method, path.as_str()) {
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
        _ => (
            StatusCode::NOT_FOUND,
            json!({
                "message": format!(
                    "{method} {path}: not implemented in delonix's Docker API \
                     compatibility layer yet (read-only slice — see \
                     docs/COMPARACAO-DOCKER-PODMAN.md)"
                )
            })
            .to_string()
            .into_bytes(),
        ),
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
    // No chrono dependency in this crate — a minimal RFC 3339 formatter
    // (UTC only) is plenty for a status field nothing parses strictly.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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

//! CRI **streaming** server — the one that brings `kubectl exec -it` and
//! `crictl exec -it` to life.
//!
//! CRI model: the `Exec`/`Attach`/`PortForward` RPCs do **not** carry the
//! data; they return a **URL** to an HTTP server that the client (kubelet/
//! `crictl`) contacts and where it *upgrades* to the Kubernetes *remotecommand*
//! protocol. We implement that server here, over **WebSocket**
//! (`v5.channel.k8s.io`) — modern `crictl`/client-go try WebSocket first and
//! only fall back to SPDY if the server does not advertise it.
//!
//! WebSocket protocol (each binary *frame* starts with a channel byte):
//!   0 = stdin (client→us)   1 = stdout   2 = stderr   3 = error/state
//!   4 = resize (JSON `{"Width":w,"Height":h}`)   255 = close stream (v5)
//!
//! `exec` is run by the `delonix exec` binary (which does `setns` into the
//! container). In TTY mode we allocate an external **pty** and pass the *slave*
//! as the stdio of `delonix exec -t` — which in turn allocates the inner pty in
//! the container's devpts (runc's *console socket* model). The master↔WebSocket
//! bridge moves the raw bytes. C2.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path as AxPath, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};

/// Streaming request kept between the RPC and the subsequent connection.
#[derive(Clone)]
pub struct Pending {
    pub container_id: String,
    pub cmd: Vec<String>,
    pub tty: bool,
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    /// `true` = ATTACH (streams the container's output via `delonix logs -f`),
    /// `false` = EXEC (runs `cmd` in the container). Reuses the whole
    /// SPDY/WebSocket streaming machinery — only the launched command differs.
    pub attach: bool,
    created: Instant,
}

/// Shared streaming server (token-cache + engine base).
#[derive(Clone)]
pub struct Streamer {
    base: PathBuf,
    advertised: String, // e.g. "http://127.0.0.1:34567"
    execs: Arc<Mutex<HashMap<String, Pending>>>,
    pforwards: Arc<Mutex<HashMap<String, PfPending>>>,
}

/// `PortForward` request kept between the RPC and the subsequent connection.
#[derive(Clone)]
pub struct PfPending {
    pub pod_sandbox_id: String,
    pub ports: Vec<i32>,
    pub created: Instant,
}

impl Streamer {
    pub fn new(base: PathBuf, advertised: String) -> Self {
        Self {
            base,
            advertised,
            execs: Arc::new(Mutex::new(HashMap::new())),
            pforwards: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Registers a `port_forward` and returns the streaming URL. The
    /// `pod_sandbox_id` is resolved at connection time to the `pid` of the pod's
    /// infra container (`pod-cri-<id>`), whose netns is where the TCP proxy runs.
    pub fn prepare_port_forward(&self, pod_sandbox_id: String, ports: Vec<i32>) -> String {
        let token = random_token();
        let mut m = self.pforwards.lock().unwrap();
        m.retain(|_, p| Instant::now().duration_since(p.created) < Duration::from_secs(300));
        m.insert(
            token.clone(),
            PfPending {
                pod_sandbox_id,
                ports,
                created: Instant::now(),
            },
        );
        format!("{}/portforward/{}", self.advertised, token)
    }

    /// Registers an `exec` and returns the URL the client will contact.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_exec(
        &self,
        container_id: String,
        cmd: Vec<String>,
        tty: bool,
        stdin: bool,
        stdout: bool,
        stderr: bool,
    ) -> String {
        let token = random_token();
        let mut m = self.execs.lock().unwrap();
        purge_expired(&mut m);
        m.insert(
            token.clone(),
            Pending {
                container_id,
                cmd,
                tty,
                stdin,
                stdout,
                stderr,
                attach: false,
                created: Instant::now(),
            },
        );
        format!("{}/exec/{}", self.advertised, token)
    }

    /// Registers an `attach` (container output) and returns the streaming URL.
    /// Reuses the same route/handler as exec; the `attach` flag in `Pending` makes
    /// the handler run `delonix logs -f` instead of `delonix exec`.
    pub fn prepare_attach(
        &self,
        container_id: String,
        tty: bool,
        stdin: bool,
        stdout: bool,
        stderr: bool,
    ) -> String {
        let token = random_token();
        let mut m = self.execs.lock().unwrap();
        purge_expired(&mut m);
        m.insert(
            token.clone(),
            Pending {
                container_id,
                cmd: Vec::new(),
                tty,
                stdin,
                stdout,
                stderr,
                attach: true,
                created: Instant::now(),
            },
        );
        format!("{}/exec/{}", self.advertised, token)
    }

    /// Builds the axum `Router` with the streaming routes. Accepts `GET`
    /// (WebSocket) and `POST` (SPDY) — `crictl`/`kubelet` use `POST` for SPDY.
    pub fn router(self) -> Router {
        Router::new()
            .route("/exec/:token", any(exec_upgrade))
            .route("/portforward/:token", any(portforward_upgrade))
            .with_state(self)
    }
}

/// Resolves the `pid` of a pod sandbox's infra container (`pod-cri-<id>`), whose
/// netns is where the port-forward TCP proxy runs. `None` if absent/stopped.
fn pod_sandbox_pid(base: &std::path::Path, sandbox_id: &str) -> Option<i32> {
    let store = delonix_runtime_core::Store::open(base.join("containers")).ok()?;
    let c = store.load(&format!("pod-cri-{sandbox_id}")).ok()?;
    c.pid.filter(|p| delonix_runtime::is_alive(*p))
}

/// HTTP handler → SPDY upgrade for `PortForward`.
async fn portforward_upgrade(
    State(st): State<Streamer>,
    AxPath(token): AxPath<String>,
    req: axum::extract::Request,
) -> Response {
    let pending = st.pforwards.lock().unwrap().remove(&token);
    let Some(p) = pending else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let Some(pid) = pod_sandbox_pid(&st.base, &p.pod_sandbox_id) else {
        return (
            axum::http::StatusCode::CONFLICT,
            "pod sandbox has no live netns",
        )
            .into_response();
    };
    crate::spdy::handle_port_forward(req, pid)
}

/// Builds the `delonix` argv to launch to serve this stream: `logs -f`
/// (attach — container output) or `exec [-t] <cmd>` (exec). Centralizes the
/// exec/attach difference that would otherwise be scattered across the handlers.
pub fn subprocess_args(attach: bool, cmd: &[String], name: &str, tty: bool) -> Vec<String> {
    if attach {
        vec![
            "container".into(),
            "logs".into(),
            "-f".into(),
            name.to_string(),
        ]
    } else if tty {
        let mut a = vec![
            "container".into(),
            "exec".into(),
            "-t".into(),
            name.to_string(),
        ];
        a.extend(cmd.iter().cloned());
        a
    } else {
        let mut a = vec!["container".into(), "exec".into(), name.to_string()];
        a.extend(cmd.iter().cloned());
        a
    }
}

/// Removes tokens older than 5 minutes (the client connects within seconds).
fn purge_expired(m: &mut HashMap<String, Pending>) {
    let now = Instant::now();
    m.retain(|_, p| now.duration_since(p.created) < Duration::from_secs(300));
}

/// Random token (16 bytes from `/dev/urandom`, hex) — unguessable on loopback.
fn random_token() -> String {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn delonix_bin() -> PathBuf {
    crate::cli_bin()
}

// ---------------------------------------------------------------------------
// HTTP handler → WebSocket upgrade.
// ---------------------------------------------------------------------------

async fn exec_upgrade(
    State(st): State<Streamer>,
    AxPath(token): AxPath<String>,
    req: axum::extract::Request,
) -> Response {
    let pending = st.execs.lock().unwrap().remove(&token);
    let Some(p) = pending else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let base = st.base.clone();
    let name = format!("cri-{}", p.container_id);
    let upgrade = req
        .headers()
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Today's `crictl`/`kubelet` use SPDY/3.1; WebSocket (v5) is the future.
    if upgrade.contains("spdy") {
        return crate::spdy::handle_exec(req, base, name, p);
    }
    if upgrade.contains("websocket") {
        let (mut parts, _body) = req.into_parts();
        let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &st).await {
            Ok(w) => w,
            Err(e) => return e.into_response(),
        };
        let (cmd, tty, attach) = (p.cmd, p.tty, p.attach);
        return ws
            .protocols(["v5.channel.k8s.io", "v4.channel.k8s.io"])
            .on_upgrade(move |socket| async move {
                if tty {
                    exec_tty(socket, base, name, cmd, attach).await;
                } else {
                    exec_pipes(socket, base, name, cmd, attach).await;
                }
            });
    }
    axum::http::StatusCode::BAD_REQUEST.into_response()
}

// ---------------------------------------------------------------------------
// Final state encoding (channel 3) — metav1.Status protocol.
// ---------------------------------------------------------------------------

/// State message for channel 3. On success `Success`; on code != 0 client-go
/// reads `details.causes[ExitCode]` to report the real exit code.
fn status_frame(exit_code: i32) -> Message {
    let body = if exit_code == 0 {
        serde_json::json!({"metadata":{},"status":"Success"})
    } else {
        serde_json::json!({
            "metadata":{},
            "status":"Failure",
            "message": format!("command terminated with non-zero exit code: error executing command, exit status {exit_code}"),
            "reason":"NonZeroExitCode",
            "details":{"causes":[{"reason":"ExitCode","message": exit_code.to_string()}]}
        })
    };
    let mut frame = vec![3u8];
    frame.extend_from_slice(body.to_string().as_bytes());
    Message::Binary(frame)
}

fn err_frame(msg: &str) -> Message {
    let body = serde_json::json!({"metadata":{},"status":"Failure","message":msg});
    let mut frame = vec![3u8];
    frame.extend_from_slice(body.to_string().as_bytes());
    Message::Binary(frame)
}

// ---------------------------------------------------------------------------
// Exec with TTY: external pty ↔ WebSocket (all on channel 1/stdout).
// ---------------------------------------------------------------------------

async fn exec_tty(
    mut socket: WebSocket,
    base: PathBuf,
    name: String,
    cmd: Vec<String>,
    attach: bool,
) {
    let (master, slave) = match open_pty() {
        Some(p) => p,
        None => {
            let _ = socket
                .send(err_frame("delonix-cri: failed to allocate pty"))
                .await;
            return;
        }
    };

    // Capture the initial size: wait for a `resize` up to 250 ms (crictl sends
    // it right away), stashing any stdin that arrives meanwhile to resend.
    let mut pending_stdin: Vec<Vec<u8>> = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(250), socket.recv()).await {
            Ok(Some(Ok(Message::Binary(b)))) if !b.is_empty() => match b[0] {
                4 => {
                    apply_resize(master, &b[1..]);
                    break;
                }
                0 => pending_stdin.push(b[1..].to_vec()),
                _ => {}
            },
            Ok(Some(Ok(_))) => {}
            _ => break, // timeout or connection closed
        }
    }

    // Launch `delonix exec -t <name> <cmd>` with the slave as stdio.
    let mut command = std::process::Command::new(delonix_bin());
    command
        .env("DELONIX_ROOT", &base)
        .env("DELONIX_INTERNAL", "1")
        .args(subprocess_args(attach, &cmd, &name, true));
    // SAFETY: dup of the slave; each Stdio owns its fd and closes it.
    unsafe {
        command
            .stdin(Stdio::from_raw_fd(libc::dup(slave)))
            .stdout(Stdio::from_raw_fd(libc::dup(slave)))
            .stderr(Stdio::from_raw_fd(libc::dup(slave)));
    }
    let child = command.spawn();
    unsafe { libc::close(slave) };
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            unsafe { libc::close(master) };
            let _ = socket
                .send(err_frame(&format!("delonix-cri: exec failed: {e}")))
                .await;
            return;
        }
    };

    // Resend the stdin that arrived before the spawn.
    for chunk in &pending_stdin {
        write_all(master, chunk);
    }

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Thread: master → mpsc channel (stdout). EOF when the child closes the pty.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let m_read = master;
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { libc::read(m_read, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                break;
            }
            if out_tx.send(buf[..n as usize].to_vec()).is_err() {
                break;
            }
        }
    });

    // Thread: waitpid → exit code.
    let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel::<i32>();
    std::thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = exit_tx.send(code);
    });

    let mut out_open = true;
    let mut exit_code: Option<i32> = None;
    loop {
        tokio::select! {
            biased;
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Binary(b))) if !b.is_empty() => match b[0] {
                    0 => write_all(master, &b[1..]),
                    4 => apply_resize(master, &b[1..]),
                    255 => { /* close stdin: the pty doesn't half-close; ignore */ }
                    _ => {}
                },
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
            chunk = out_rx.recv(), if out_open => match chunk {
                Some(data) => {
                    let mut frame = vec![1u8];
                    frame.extend_from_slice(&data);
                    if ws_tx.send(Message::Binary(frame)).await.is_err() { break; }
                }
                None => { out_open = false; }
            },
            code = &mut exit_rx, if exit_code.is_none() => {
                exit_code = Some(code.unwrap_or(-1));
            }
        }
        // Ends when the child has exited AND stdout has drained.
        if exit_code.is_some() && !out_open {
            break;
        }
    }

    let code = match exit_code {
        Some(c) => c,
        None => exit_rx.await.unwrap_or(-1),
    };
    let _ = ws_tx.send(status_frame(code)).await;
    let _ = ws_tx.send(Message::Close(None)).await;
    unsafe { libc::close(master) };
    let _ = reader.join();
}

// ---------------------------------------------------------------------------
// Exec without TTY: separate stdin/stdout/stderr (channels 0/1/2).
// ---------------------------------------------------------------------------

async fn exec_pipes(
    mut socket: WebSocket,
    base: PathBuf,
    name: String,
    cmd: Vec<String>,
    attach: bool,
) {
    let mut command = std::process::Command::new(delonix_bin());
    command
        .env("DELONIX_ROOT", &base)
        .env("DELONIX_INTERNAL", "1")
        .args(subprocess_args(attach, &cmd, &name, false))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = socket
                .send(err_frame(&format!("delonix-cri: exec failed: {e}")))
                .await;
            return;
        }
    };

    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    let mut readers = Vec::new();
    let sources: [(u8, Option<Box<dyn Read + Send>>); 2] = [
        (1u8, stdout.map(|s| Box::new(s) as Box<dyn Read + Send>)),
        (2u8, stderr.map(|s| Box::new(s) as Box<dyn Read + Send>)),
    ];
    for (chan, src) in sources {
        if let Some(mut r) = src {
            let tx = out_tx.clone();
            readers.push(std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = r.read(&mut buf) {
                    if n == 0 || tx.send((chan, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }));
        }
    }
    drop(out_tx);

    let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel::<i32>();
    std::thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = exit_tx.send(code);
    });

    let mut out_open = true;
    let mut exit_code: Option<i32> = None;
    loop {
        tokio::select! {
            biased;
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Binary(b))) if !b.is_empty() => match b[0] {
                    0 => {
                        if let Some(si) = stdin.as_mut() {
                            use std::io::Write;
                            let _ = si.write_all(&b[1..]);
                            let _ = si.flush();
                        }
                    }
                    255 => { stdin = None; } // close stdin (EOF for the process)
                    _ => {}
                },
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
            item = out_rx.recv(), if out_open => match item {
                Some((chan, data)) => {
                    let mut frame = vec![chan];
                    frame.extend_from_slice(&data);
                    if ws_tx.send(Message::Binary(frame)).await.is_err() { break; }
                }
                None => { out_open = false; }
            },
            code = &mut exit_rx, if exit_code.is_none() => {
                exit_code = Some(code.unwrap_or(-1));
            }
        }
        if exit_code.is_some() && !out_open {
            break;
        }
    }

    let code = match exit_code {
        Some(c) => c,
        None => exit_rx.await.unwrap_or(-1),
    };
    let _ = ws_tx.send(status_frame(code)).await;
    let _ = ws_tx.send(Message::Close(None)).await;
    for r in readers {
        let _ = r.join();
    }
}

// ---------------------------------------------------------------------------
// Low-level helpers (pty, resize, write).
// ---------------------------------------------------------------------------

/// Allocates an external pty (80x24 by default). Returns raw `(master, slave)`.
pub(crate) fn open_pty() -> Option<(i32, i32)> {
    let mut master: i32 = -1;
    let mut slave: i32 = -1;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_row = 24;
    ws.ws_col = 80;
    // SAFETY: openpty fills master/slave; the remaining null pointers = defaults.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    if r == 0 {
        Some((master, slave))
    } else {
        None
    }
}

/// Applies a `resize` (JSON `{"Width":w,"Height":h}`) to the master via TIOCSWINSZ.
pub(crate) fn apply_resize(master: i32, payload: &[u8]) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return;
    };
    let cols = v.get("Width").and_then(|x| x.as_u64()).unwrap_or(80) as u16;
    let rows = v.get("Height").and_then(|x| x.as_u64()).unwrap_or(24) as u16;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_row = rows;
    ws.ws_col = cols;
    // SAFETY: master is a valid pty fd; TIOCSWINSZ accepts a winsize.
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }
}

/// Writes everything to a raw fd (the pty master), tolerating partial writes.
pub(crate) fn write_all(fd: i32, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const _, data.len()) };
        if n <= 0 {
            break;
        }
        data = &data[n as usize..];
    }
}

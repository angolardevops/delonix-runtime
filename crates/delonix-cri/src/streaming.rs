//! Servidor de **streaming** do CRI — o que dá vida ao `kubectl exec -it` e ao
//! `crictl exec -it`.
//!
//! Modelo do CRI: as RPCs `Exec`/`Attach`/`PortForward` **não** transportam os
//! dados; devolvem uma **URL** para um servidor HTTP que o cliente (kubelet/
//! `crictl`) contacta e onde faz *upgrade* para o protocolo *remotecommand* do
//! Kubernetes. Implementamos esse servidor aqui, sobre **WebSocket**
//! (`v5.channel.k8s.io`) — o `crictl`/client-go modernos tentam WebSocket
//! primeiro e só caem para SPDY se o servidor não o anunciar.
//!
//! Protocolo WebSocket (cada *frame* binário começa por um byte de canal):
//!   0 = stdin (cliente→nós)   1 = stdout   2 = stderr   3 = erro/estado
//!   4 = resize (JSON `{"Width":w,"Height":h}`)   255 = fechar stream (v5)
//!
//! O `exec` é executado pelo binário `delonix exec` (que faz `setns` ao
//! container). Em modo TTY alocamos um **pty** externo e passamos o *slave* como
//! stdio do `delonix exec -t` — que por sua vez aloca o pty interno no devpts do
//! container (modelo *console socket* do runc). A ponte master↔WebSocket move os
//! bytes em bruto. C2.
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

/// Pedido de streaming guardado entre a RPC e a ligação subsequente.
#[derive(Clone)]
pub struct Pending {
    pub container_id: String,
    pub cmd: Vec<String>,
    pub tty: bool,
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    /// `true` = ATTACH (transmite o output do container via `delonix logs -f`),
    /// `false` = EXEC (corre `cmd` no container). Reutiliza toda a máquina de
    /// streaming SPDY/WebSocket — só o comando lançado difere.
    pub attach: bool,
    created: Instant,
}

/// Servidor de streaming partilhado (token-cache + base do engine).
#[derive(Clone)]
pub struct Streamer {
    base: PathBuf,
    advertised: String, // ex.: "http://127.0.0.1:34567"
    execs: Arc<Mutex<HashMap<String, Pending>>>,
    pforwards: Arc<Mutex<HashMap<String, PfPending>>>,
}

/// Pedido de `PortForward` guardado entre a RPC e a ligação subsequente.
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

    /// Regista um `port_forward` e devolve a URL de streaming. O `pod_sandbox_id`
    /// resolve-se em tempo de ligação para o `pid` do infra container do pod
    /// (`pod-cri-<id>`), cujo netns é onde se faz o proxy TCP.
    pub fn prepare_port_forward(&self, pod_sandbox_id: String, ports: Vec<i32>) -> String {
        let token = random_token();
        let mut m = self.pforwards.lock().unwrap();
        m.retain(|_, p| Instant::now().duration_since(p.created) < Duration::from_secs(300));
        m.insert(token.clone(), PfPending { pod_sandbox_id, ports, created: Instant::now() });
        format!("{}/portforward/{}", self.advertised, token)
    }

    /// Regista um `exec` e devolve a URL que o cliente vai contactar.
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
            Pending { container_id, cmd, tty, stdin, stdout, stderr, attach: false, created: Instant::now() },
        );
        format!("{}/exec/{}", self.advertised, token)
    }

    /// Regista um `attach` (output do container) e devolve a URL de streaming.
    /// Reutiliza a mesma rota/handler do exec; o flag `attach` no `Pending` faz o
    /// handler correr `delonix logs -f` em vez de `delonix exec`.
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

    /// Constrói o `Router` axum com as rotas de streaming. Aceita `GET`
    /// (WebSocket) e `POST` (SPDY) — o `crictl`/`kubelet` usam `POST` para SPDY.
    pub fn router(self) -> Router {
        Router::new()
            .route("/exec/:token", any(exec_upgrade))
            .route("/portforward/:token", any(portforward_upgrade))
            .with_state(self)
    }
}

/// Resolve o `pid` do infra container de um pod sandbox (`pod-cri-<id>`), cujo
/// netns é onde se faz o proxy TCP do port-forward. `None` se não existir/parado.
fn pod_sandbox_pid(base: &std::path::Path, sandbox_id: &str) -> Option<i32> {
    let store = delonix_core::Store::open(base.join("containers")).ok()?;
    let c = store.load(&format!("pod-cri-{sandbox_id}")).ok()?;
    c.pid.filter(|p| delonix_runtime::is_alive(*p))
}

/// Handler HTTP → upgrade SPDY para `PortForward`.
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
        return (axum::http::StatusCode::CONFLICT, "pod sandbox sem netns vivo").into_response();
    };
    crate::spdy::handle_port_forward(req, pid)
}

/// Constrói o argv do `delonix` a lançar para servir este stream: `logs -f`
/// (attach — output do container) ou `exec [-t] <cmd>` (exec). Centraliza a
/// diferença exec/attach que de outra forma estaria espalhada pelos handlers.
pub fn subprocess_args(attach: bool, cmd: &[String], name: &str, tty: bool) -> Vec<String> {
    if attach {
        vec!["logs".into(), "-f".into(), name.to_string()]
    } else if tty {
        let mut a = vec!["exec".into(), "-t".into(), name.to_string()];
        a.extend(cmd.iter().cloned());
        a
    } else {
        let mut a = vec!["exec".into(), name.to_string()];
        a.extend(cmd.iter().cloned());
        a
    }
}

/// Remove tokens com mais de 5 minutos (o cliente liga-se em segundos).
fn purge_expired(m: &mut HashMap<String, Pending>) {
    let now = Instant::now();
    m.retain(|_, p| now.duration_since(p.created) < Duration::from_secs(300));
}

/// Token aleatório (16 bytes de `/dev/urandom`, hex) — não adivinhável em loopback.
fn random_token() -> String {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn delonix_bin() -> PathBuf {
    delonix_core::self_bin()
}

// ---------------------------------------------------------------------------
// Handler HTTP → upgrade WebSocket.
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

    // O `crictl`/`kubelet` de hoje usam SPDY/3.1; o WebSocket (v5) é o futuro.
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
        return ws.protocols(["v5.channel.k8s.io", "v4.channel.k8s.io"]).on_upgrade(
            move |socket| async move {
                if tty {
                    exec_tty(socket, base, name, cmd, attach).await;
                } else {
                    exec_pipes(socket, base, name, cmd, attach).await;
                }
            },
        );
    }
    axum::http::StatusCode::BAD_REQUEST.into_response()
}

// ---------------------------------------------------------------------------
// Codificação do estado final (canal 3) — protocolo metav1.Status.
// ---------------------------------------------------------------------------

/// Mensagem de estado para o canal 3. Em sucesso `Success`; em código != 0 o
/// client-go lê `details.causes[ExitCode]` para reportar o exit code real.
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
// Exec com TTY: pty externo ↔ WebSocket (tudo no canal 1/stdout).
// ---------------------------------------------------------------------------

async fn exec_tty(mut socket: WebSocket, base: PathBuf, name: String, cmd: Vec<String>, attach: bool) {
    let (master, slave) = match open_pty() {
        Some(p) => p,
        None => {
            let _ = socket.send(err_frame("delonix-cri: falha a alocar pty")).await;
            return;
        }
    };

    // Captura o tamanho inicial: espera por um `resize` até 250 ms (o crictl
    // envia-o de imediato), guardando o stdin que chegar entretanto para reenviar.
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
            _ => break, // timeout ou ligação fechada
        }
    }

    // Lança `delonix exec -t <name> <cmd>` com o slave como stdio.
    let mut command = std::process::Command::new(delonix_bin());
    command.env("DELONIX_ROOT", &base).env("DELONIX_INTERNAL", "1").args(subprocess_args(attach, &cmd, &name, true));
    // SAFETY: dup do slave; cada Stdio fica dono do fd e fecha-o.
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
            let _ = socket.send(err_frame(&format!("delonix-cri: exec falhou: {e}"))).await;
            return;
        }
    };

    // Reenvia o stdin que chegou antes do spawn.
    for chunk in &pending_stdin {
        write_all(master, chunk);
    }

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Thread: master → canal mpsc (stdout). EOF quando o filho fecha o pty.
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
                    255 => { /* fechar stdin: o pty não meia-fecha; ignora */ }
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
        // Termina quando o filho saiu E o stdout drenou.
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
// Exec sem TTY: stdin/stdout/stderr separados (canais 0/1/2).
// ---------------------------------------------------------------------------

async fn exec_pipes(mut socket: WebSocket, base: PathBuf, name: String, cmd: Vec<String>, attach: bool) {
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
            let _ = socket.send(err_frame(&format!("delonix-cri: exec falhou: {e}"))).await;
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
                    255 => { stdin = None; } // fecha stdin (EOF para o processo)
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
// Auxiliares de baixo nível (pty, resize, escrita).
// ---------------------------------------------------------------------------

/// Aloca um pty externo (80x24 por omissão). Devolve `(master, slave)` em bruto.
pub(crate) fn open_pty() -> Option<(i32, i32)> {
    let mut master: i32 = -1;
    let mut slave: i32 = -1;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_row = 24;
    ws.ws_col = 80;
    // SAFETY: openpty preenche master/slave; restantes ponteiros nulos = defaults.
    let r = unsafe {
        libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws)
    };
    if r == 0 {
        Some((master, slave))
    } else {
        None
    }
}

/// Aplica um `resize` (JSON `{"Width":w,"Height":h}`) ao master via TIOCSWINSZ.
pub(crate) fn apply_resize(master: i32, payload: &[u8]) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return;
    };
    let cols = v.get("Width").and_then(|x| x.as_u64()).unwrap_or(80) as u16;
    let rows = v.get("Height").and_then(|x| x.as_u64()).unwrap_or(24) as u16;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_row = rows;
    ws.ws_col = cols;
    // SAFETY: master é um fd de pty válido; TIOCSWINSZ aceita uma winsize.
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }
}

/// Escreve tudo num fd em bruto (master do pty), tolerando escritas parciais.
pub(crate) fn write_all(fd: i32, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const _, data.len()) };
        if n <= 0 {
            break;
        }
        data = &data[n as usize..];
    }
}

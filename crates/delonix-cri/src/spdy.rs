//! Servidor **SPDY/3.1** para o protocolo *remotecommand* do Kubernetes — o que
//! o `crictl`/`kubelet` usam HOJE para `exec`/`attach` (tentam SPDY antes de
//! WebSocket). Complementa o servidor WebSocket em [`crate::streaming`].
//!
//! Fluxo: o cliente faz `POST` com `Upgrade: SPDY/3.1`; respondemos `101` e a
//! ligação passa a SPDY. O cliente abre uma *stream* por canal (com um cabeçalho
//! `streamType` = error/stdin/stdout/stderr/resize); nós corremos `delonix exec`
//! e movemos os bytes entre as streams e o processo. O canal `error` transporta
//! o `metav1.Status` final (com o exit code, protocolo v4). C2.
//!
//! Os cabeçalhos SPDY são comprimidos com zlib + o **dicionário fixo SPDY/3**
//! (`spdy3.dict`, adler32 0xe3c6a7c2). Exige o backend `zlib-rs` do `flate2`.
#![allow(clippy::result_large_err)]

use std::collections::{HashMap, VecDeque};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::streaming::{apply_resize, open_pty, write_all as write_fd, Pending};

/// Dicionário fixo SPDY/3 (1423 bytes) usado na (de)compressão de cabeçalhos.
static SPDY3_DICT: &[u8] = include_bytes!("spdy3.dict");

// ---------------------------------------------------------------------------
// Codificação de blocos Nome/Valor (SPDY/3): u32 count, depois pares
// (u32 len + nome, u32 len + valor).
// ---------------------------------------------------------------------------

fn encode_nv(pairs: &[(String, String)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
    for (k, v) in pairs {
        b.extend_from_slice(&(k.len() as u32).to_be_bytes());
        b.extend_from_slice(k.as_bytes());
        b.extend_from_slice(&(v.len() as u32).to_be_bytes());
        b.extend_from_slice(v.as_bytes());
    }
    b
}

fn decode_nv(buf: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if buf.len() < 4 {
        return out;
    }
    let count = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let mut p = 4;
    let rd = |buf: &[u8], p: &mut usize| -> Option<Vec<u8>> {
        if *p + 4 > buf.len() {
            return None;
        }
        let n = u32::from_be_bytes([buf[*p], buf[*p + 1], buf[*p + 2], buf[*p + 3]]) as usize;
        *p += 4;
        if *p + n > buf.len() {
            return None;
        }
        let s = buf[*p..*p + n].to_vec();
        *p += n;
        Some(s)
    };
    for _ in 0..count {
        let (Some(k), Some(v)) = (rd(buf, &mut p), rd(buf, &mut p)) else {
            break;
        };
        out.push((
            String::from_utf8_lossy(&k).into_owned(),
            String::from_utf8_lossy(&v).into_owned(),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// (De)compressão zlib contínua com dicionário e Z_SYNC_FLUSH. O estado é
// partilhado por todos os frames de cabeçalhos numa direção.
// ---------------------------------------------------------------------------

pub struct Deflater {
    c: Compress,
}

impl Default for Deflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Deflater {
    pub fn new() -> Self {
        let mut c = Compress::new(Compression::default(), true);
        c.set_dictionary(SPDY3_DICT).expect("deflateSetDictionary");
        Self { c }
    }

    /// Comprime um bloco NV, devolvendo os bytes (terminados pelo marcador de
    /// sync 00 00 ff ff) para concatenar no frame de controlo.
    pub fn block(&mut self, nv: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(nv.len() / 2 + 32);
        let mut input = nv;
        let mut tmp = [0u8; 8192];
        loop {
            let (bi, bo) = (self.c.total_in(), self.c.total_out());
            let _ = self.c.compress(input, &mut tmp, FlushCompress::Sync);
            let ci = (self.c.total_in() - bi) as usize;
            let co = (self.c.total_out() - bo) as usize;
            out.extend_from_slice(&tmp[..co]);
            input = &input[ci..];
            if co == tmp.len() {
                continue; // buffer cheio: mais saída pendente
            }
            if input.is_empty() {
                break; // tudo consumido e flush emitido
            }
        }
        out
    }
}

pub struct Inflater {
    d: Decompress,
    dict_set: bool,
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Inflater {
    pub fn new() -> Self {
        Self {
            d: Decompress::new(true),
            dict_set: false,
        }
    }

    /// Descomprime um bloco de cabeçalhos. Em SPDY o dicionário só pode ser
    /// instalado quando o zlib o pede (`Z_NEED_DICT`).
    pub fn block(&mut self, comp: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(comp.len() * 4 + 64);
        let mut input = comp;
        let mut tmp = [0u8; 8192];
        loop {
            let (bi, bo) = (self.d.total_in(), self.d.total_out());
            let res = self.d.decompress(input, &mut tmp, FlushDecompress::Sync);
            let ci = (self.d.total_in() - bi) as usize;
            let co = (self.d.total_out() - bo) as usize;
            out.extend_from_slice(&tmp[..co]);
            input = &input[ci..];
            match res {
                Ok(Status::StreamEnd) => break,
                Ok(_) => {
                    if co == tmp.len() {
                        continue; // mais saída pendente
                    }
                    if input.is_empty() {
                        break;
                    }
                    if co == 0 && ci == 0 {
                        // precisa de dicionário antes de progredir
                        if !self.dict_set && self.d.set_dictionary(SPDY3_DICT).is_ok() {
                            self.dict_set = true;
                            continue;
                        }
                        break;
                    }
                }
                Err(_) => {
                    // Z_NEED_DICT (ou erro): instala o dicionário e continua.
                    if !self.dict_set && self.d.set_dictionary(SPDY3_DICT).is_ok() {
                        self.dict_set = true;
                        continue;
                    }
                    break;
                }
            }
        }
        out
    }
}

// ===========================================================================
// Servidor SPDY/3.1 — framing, upgrade e ponte com `delonix exec`.
// ===========================================================================

const SYN_STREAM: u16 = 1;
const SYN_REPLY: u16 = 2;
const SETTINGS: u16 = 4;
const PING: u16 = 6;
const GOAWAY: u16 = 7;
const WINDOW_UPDATE: u16 = 9;
const FLAG_FIN: u8 = 0x01;
const MAX_DATA: usize = 16 * 1024;

fn delonix_bin() -> PathBuf {
    delonix_runtime_core::self_bin()
}

/// Constrói um frame de controlo SPDY/3.
fn ctrl(kind: u16, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(8 + payload.len());
    f.extend_from_slice(&(0x8000u16 | 3).to_be_bytes()); // bit de controlo + versão 3
    f.extend_from_slice(&kind.to_be_bytes());
    f.push(flags);
    let len = payload.len() as u32;
    f.extend_from_slice(&len.to_be_bytes()[1..]); // u24
    f.extend_from_slice(payload);
    f
}

/// Constrói um frame de dados SPDY.
fn data_frame(sid: u32, flags: u8, data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(8 + data.len());
    f.extend_from_slice(&(sid & 0x7fff_ffff).to_be_bytes());
    f.push(flags);
    let len = data.len() as u32;
    f.extend_from_slice(&len.to_be_bytes()[1..]);
    f.extend_from_slice(data);
    f
}

/// Seleciona a versão do subprotocolo remotecommand (preferimos v4: traz o
/// exit code no stream de erro).
fn select_proto(headers: &header::HeaderMap) -> String {
    let mut offered: Vec<String> = Vec::new();
    for v in headers.get_all("x-stream-protocol-version") {
        if let Ok(s) = v.to_str() {
            offered.extend(s.split(',').map(|x| x.trim().to_string()));
        }
    }
    for pref in [
        "v4.channel.k8s.io",
        "v5.channel.k8s.io",
        "v3.channel.k8s.io",
    ] {
        if offered.iter().any(|o| o == pref) {
            return pref.to_string();
        }
    }
    offered
        .into_iter()
        .next()
        .unwrap_or_else(|| "v4.channel.k8s.io".into())
}

/// Handler do `POST` com `Upgrade: SPDY/3.1`. Responde `101` e corre o SPDY na
/// ligação atualizada.
pub fn handle_exec(
    mut req: axum::extract::Request,
    base: PathBuf,
    name: String,
    p: Pending,
) -> Response {
    let proto = select_proto(req.headers());
    let Some(on_upgrade) = req.extensions_mut().remove::<OnUpgrade>() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    tokio::spawn(async move {
        if let Ok(upgraded) = on_upgrade.await {
            run_exec(TokioIo::new(upgraded), base, name, p).await;
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "SPDY/3.1")
        .header("X-Stream-Protocol-Version", proto)
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------------
// Mensagens para a tarefa de escrita (única dona do socket de saída + zlib).
// ---------------------------------------------------------------------------

enum Out {
    SynReply(u32),
    Data { sid: u32, fin: bool, buf: Vec<u8> },
    Credit { sid: u32, delta: i64 }, // recebemos WINDOW_UPDATE → cresce a nossa janela
    SendWu { sid: u32, delta: u32 }, // reabastece o cliente
    Ping(u32),
    Goaway(u32),
    Close,
}

/// Estado final no stream de erro (v4): em código 0, fecha vazio (= sucesso); em
/// código != 0, escreve o `metav1.Status` com o exit code.
fn finish(tx: &UnboundedSender<Out>, error_sid: u32, code: i32) {
    let buf = if code == 0 {
        Vec::new()
    } else {
        serde_json::json!({
            "metadata":{},
            "status":"Failure",
            "message": format!("command terminated with non-zero exit code: error executing command, exit status {code}"),
            "reason":"NonZeroExitCode",
            "details":{"causes":[{"reason":"ExitCode","message": code.to_string()}]}
        })
        .to_string()
        .into_bytes()
    };
    let _ = tx.send(Out::Data {
        sid: error_sid,
        fin: true,
        buf,
    });
    let _ = tx.send(Out::Goaway(error_sid));
    let _ = tx.send(Out::Close);
}

type Pend = HashMap<u32, (VecDeque<u8>, bool, bool)>; // sid -> (buffer, fin_pendente, fin_enviado)

/// Escoa o máximo de dados pendentes de uma stream respeitando as janelas.
async fn flush_stream<W: AsyncWrite + Unpin>(
    wr: &mut W,
    sid: u32,
    session_win: &mut i64,
    win: &mut HashMap<u32, i64>,
    initial: i64,
    pend: &mut Pend,
) -> std::io::Result<()> {
    let Some(entry) = pend.get_mut(&sid) else {
        return Ok(());
    };
    let sw = win.entry(sid).or_insert(initial);
    loop {
        if entry.2 {
            break; // FIN já enviado
        }
        let avail = (*sw).min(*session_win).max(0) as usize;
        let n = entry.0.len().min(avail).min(MAX_DATA);
        if n == 0 {
            if entry.0.is_empty() && entry.1 {
                wr.write_all(&data_frame(sid, FLAG_FIN, &[])).await?;
                entry.2 = true;
            }
            break; // sem janela ou sem dados
        }
        let chunk: Vec<u8> = entry.0.drain(..n).collect();
        let fin = entry.0.is_empty() && entry.1;
        wr.write_all(&data_frame(sid, if fin { FLAG_FIN } else { 0 }, &chunk))
            .await?;
        *sw -= n as i64;
        *session_win -= n as i64;
        if fin {
            entry.2 = true;
            break;
        }
    }
    Ok(())
}

async fn writer_task<W: AsyncWrite + Unpin>(mut wr: W, mut rx: UnboundedReceiver<Out>) {
    let mut comp = Deflater::new();
    // O `docker/spdystream` (cliente do kubelet/crictl) anuncia uma janela de
    // 64 KB via SETTINGS mas NÃO faz controlo de fluxo de saída — nunca envia
    // WINDOW_UPDATE. Se respeitássemos a janela, travávamos exatamente aos 64 KB.
    // Enviamos livremente (o backpressure do TCP regula o ritmo); mantemos o
    // contador só para honrar créditos extra que um cliente conforme envie.
    let mut session_win: i64 = i64::MAX / 4;
    let initial_win: i64 = i64::MAX / 4;
    let mut win: HashMap<u32, i64> = HashMap::new();
    let mut pend: Pend = HashMap::new();

    while let Some(msg) = rx.recv().await {
        let res: std::io::Result<()> = async {
            match msg {
                Out::SynReply(sid) => {
                    let cnv = comp.block(&encode_nv(&[]));
                    let mut payload = Vec::with_capacity(4 + cnv.len());
                    payload.extend_from_slice(&sid.to_be_bytes());
                    payload.extend_from_slice(&cnv);
                    wr.write_all(&ctrl(SYN_REPLY, 0, &payload)).await?;
                }
                Out::Data { sid, fin, buf } => {
                    let e = pend
                        .entry(sid)
                        .or_insert_with(|| (VecDeque::new(), false, false));
                    e.0.extend(buf);
                    if fin {
                        e.1 = true;
                    }
                    flush_stream(
                        &mut wr,
                        sid,
                        &mut session_win,
                        &mut win,
                        initial_win,
                        &mut pend,
                    )
                    .await?;
                }
                Out::Credit { sid, delta } => {
                    if sid == 0 {
                        session_win += delta;
                        let sids: Vec<u32> = pend.keys().copied().collect();
                        for s in sids {
                            flush_stream(
                                &mut wr,
                                s,
                                &mut session_win,
                                &mut win,
                                initial_win,
                                &mut pend,
                            )
                            .await?;
                        }
                    } else {
                        *win.entry(sid).or_insert(initial_win) += delta;
                        flush_stream(
                            &mut wr,
                            sid,
                            &mut session_win,
                            &mut win,
                            initial_win,
                            &mut pend,
                        )
                        .await?;
                    }
                }
                Out::SendWu { sid, delta } => {
                    let mut p = Vec::with_capacity(8);
                    p.extend_from_slice(&(sid & 0x7fff_ffff).to_be_bytes());
                    p.extend_from_slice(&delta.to_be_bytes());
                    wr.write_all(&ctrl(WINDOW_UPDATE, 0, &p)).await?;
                }
                Out::Ping(id) => {
                    wr.write_all(&ctrl(PING, 0, &id.to_be_bytes())).await?;
                }
                Out::Goaway(last) => {
                    let mut p = Vec::with_capacity(8);
                    p.extend_from_slice(&last.to_be_bytes());
                    p.extend_from_slice(&0u32.to_be_bytes());
                    wr.write_all(&ctrl(GOAWAY, 0, &p)).await?;
                }
                Out::Close => {
                    let _ = wr.flush().await;
                    return Err(std::io::Error::other("close"));
                }
            }
            Ok(())
        }
        .await;
        if res.is_err() {
            break;
        }
    }
    let _ = wr.shutdown().await;
}

// ---------------------------------------------------------------------------
// Encaminhamento do stdin/resize para o processo.
// ---------------------------------------------------------------------------

enum Input {
    None,
    Tty(i32),
    Pipe(Option<std::process::ChildStdin>),
}

impl Input {
    fn write_stdin(&mut self, data: &[u8]) {
        match self {
            Input::Tty(m) => write_fd(*m, data),
            Input::Pipe(Some(si)) => {
                use std::io::Write;
                let _ = si.write_all(data);
                let _ = si.flush();
            }
            _ => {}
        }
    }
    fn close_stdin(&mut self) {
        if let Input::Pipe(si) = self {
            *si = None;
        }
    }
    fn resize(&mut self, data: &[u8]) {
        if let Input::Tty(m) = self {
            apply_resize(*m, data);
        }
    }
    fn close(self) {
        if let Input::Tty(m) = self {
            unsafe { libc::close(m) };
        }
    }
}

/// Lança `delonix exec` e arranca o bombeamento de saída para as streams. Devolve
/// o destino do stdin.
fn spawn_and_pump(
    base: &Path,
    name: &str,
    p: &Pending,
    out_tx: UnboundedSender<Out>,
    stdout_sid: u32,
    stderr_sid: u32,
    error_sid: u32,
) -> Input {
    if p.tty {
        let Some((master, slave)) = open_pty() else {
            finish(&out_tx, error_sid, -1);
            return Input::None;
        };
        let mut cmd = Command::new(delonix_bin());
        cmd.env("DELONIX_ROOT", base)
            .env("DELONIX_INTERNAL", "1")
            .args(crate::streaming::subprocess_args(
                p.attach, &p.cmd, name, true,
            ));
        unsafe {
            cmd.stdin(Stdio::from_raw_fd(libc::dup(slave)))
                .stdout(Stdio::from_raw_fd(libc::dup(slave)))
                .stderr(Stdio::from_raw_fd(libc::dup(slave)));
        }
        let spawned = cmd.spawn();
        unsafe { libc::close(slave) };
        let mut child = match spawned {
            Ok(c) => c,
            Err(_) => {
                unsafe { libc::close(master) };
                finish(&out_tx, error_sid, -1);
                return Input::None;
            }
        };
        let tx = out_tx.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut _, buf.len()) };
                if n <= 0 {
                    break;
                }
                if tx
                    .send(Out::Data {
                        sid: stdout_sid,
                        fin: false,
                        buf: buf[..n as usize].to_vec(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let tx2 = out_tx;
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            let _ = reader.join();
            finish(&tx2, error_sid, code);
        });
        Input::Tty(master)
    } else {
        let mut cmd = Command::new(delonix_bin());
        cmd.env("DELONIX_ROOT", base)
            .env("DELONIX_INTERNAL", "1")
            .args(crate::streaming::subprocess_args(
                p.attach, &p.cmd, name, false,
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => {
                finish(&out_tx, error_sid, -1);
                return Input::None;
            }
        };
        let stdin = child.stdin.take();
        let mut handles = Vec::new();
        for (sid, src) in [
            (
                stdout_sid,
                child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            ),
            (
                stderr_sid,
                child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            ),
        ] {
            if let Some(mut r) = src {
                let tx = out_tx.clone();
                handles.push(std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = r.read(&mut buf) {
                        if n == 0
                            || tx
                                .send(Out::Data {
                                    sid,
                                    fin: false,
                                    buf: buf[..n].to_vec(),
                                })
                                .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
        }
        let tx2 = out_tx;
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            for h in handles {
                let _ = h.join();
            }
            finish(&tx2, error_sid, code);
        });
        Input::Pipe(stdin)
    }
}

// ---------------------------------------------------------------------------
// Loop principal: lê frames SPDY, gere streams e move os dados.
// ---------------------------------------------------------------------------

async fn run_exec<S>(io: S, base: PathBuf, name: String, p: Pending)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, wr) = tokio::io::split(io);
    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<Out>();
    tokio::spawn(writer_task(wr, out_rx));

    let mut inf = Inflater::new();
    let (mut error_sid, mut stdout_sid, mut stderr_sid, mut stdin_sid, mut resize_sid) =
        (0u32, 0, 0, 0, 0);
    let (mut have_error, mut have_stdout, mut have_stderr, mut have_stdin, mut have_resize) =
        (false, false, false, false, false);
    let want_stdout = p.stdout;
    let want_stderr = p.stderr && !p.tty;
    let want_stdin = p.stdin;
    let want_resize = p.tty;
    let mut input = Input::None;
    let mut started = false;

    let mut hdr = [0u8; 8];
    while rd.read_exact(&mut hdr).await.is_ok() {
        let len = ((hdr[5] as usize) << 16) | ((hdr[6] as usize) << 8) | (hdr[7] as usize);
        let mut payload = vec![0u8; len];
        if len > 0 && rd.read_exact(&mut payload).await.is_err() {
            break;
        }

        if hdr[0] & 0x80 != 0 {
            let kind = u16::from_be_bytes([hdr[2], hdr[3]]);
            match kind {
                SYN_STREAM => {
                    if payload.len() < 10 {
                        continue;
                    }
                    let sid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                        & 0x7fff_ffff;
                    let headers = decode_nv(&inf.block(&payload[10..]));
                    let stype = headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("streamtype"))
                        .map(|(_, v)| v.to_ascii_lowercase())
                        .unwrap_or_default();
                    let _ = out_tx.send(Out::SynReply(sid));
                    match stype.as_str() {
                        "error" => (error_sid, have_error) = (sid, true),
                        "stdout" => (stdout_sid, have_stdout) = (sid, true),
                        "stderr" => (stderr_sid, have_stderr) = (sid, true),
                        "stdin" => (stdin_sid, have_stdin) = (sid, true),
                        "resize" => (resize_sid, have_resize) = (sid, true),
                        _ => {}
                    }
                    let ready = have_error
                        && (!want_stdout || have_stdout)
                        && (!want_stderr || have_stderr)
                        && (!want_stdin || have_stdin)
                        && (!want_resize || have_resize);
                    if ready && !started {
                        started = true;
                        input = spawn_and_pump(
                            &base,
                            &name,
                            &p,
                            out_tx.clone(),
                            stdout_sid,
                            stderr_sid,
                            error_sid,
                        );
                    }
                }
                SETTINGS => {
                    // Ignoramos o INITIAL_WINDOW_SIZE de propósito: o cliente
                    // anuncia-o mas não reabastece (ver writer_task).
                }
                PING => {
                    if payload.len() >= 4 {
                        let _ = out_tx.send(Out::Ping(u32::from_be_bytes([
                            payload[0], payload[1], payload[2], payload[3],
                        ])));
                    }
                }
                WINDOW_UPDATE => {
                    if payload.len() >= 8 {
                        let sid =
                            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                                & 0x7fff_ffff;
                        let delta =
                            u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                        let _ = out_tx.send(Out::Credit {
                            sid,
                            delta: delta as i64,
                        });
                    }
                }
                GOAWAY => break,
                _ => {}
            }
        } else {
            // DATA frame
            let sid = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) & 0x7fff_ffff;
            let flags = hdr[4];
            if have_stdin && sid == stdin_sid {
                input.write_stdin(&payload);
                if !payload.is_empty() {
                    let _ = out_tx.send(Out::SendWu {
                        sid,
                        delta: payload.len() as u32,
                    });
                    let _ = out_tx.send(Out::SendWu {
                        sid: 0,
                        delta: payload.len() as u32,
                    });
                }
                if flags & FLAG_FIN != 0 {
                    input.close_stdin();
                }
            } else if have_resize && sid == resize_sid {
                input.resize(&payload);
            }
        }
    }
    input.close();
}

// ===========================================================================
// PortForward (issue #14): proxy TCP entre as streams SPDY e portas DENTRO do
// netns do pod. O cliente abre, por porta, uma stream `error` e uma `data`
// (header `port`). Para cada `data`, ligamos um TCP a `127.0.0.1:<porta>` no
// netns do pod (via `setns` numa thread dedicada) e movemos os bytes nos dois
// sentidos.
// ===========================================================================

/// Liga um TCP a `127.0.0.1:<port>` DENTRO do netns do processo `pid`. Faz
/// `setns(CLONE_NEWNET)` numa thread descartável (só essa thread muda de netns)
/// e devolve o socket já ligado (válido em todo o processo).
fn connect_in_netns(pid: i32, port: u16) -> std::io::Result<std::net::TcpStream> {
    use std::os::fd::AsRawFd;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let res = (|| -> std::io::Result<std::net::TcpStream> {
            let f = std::fs::File::open(format!("/proc/{pid}/ns/net"))?;
            // SAFETY: fd válido; setns muda só o netns DESTA thread.
            if unsafe { libc::setns(f.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        })();
        let _ = tx.send(res);
    });
    rx.recv()
        .unwrap_or_else(|_| Err(std::io::Error::other("netns connect thread morreu")))
}

pub fn handle_port_forward(mut req: axum::extract::Request, pid: i32) -> Response {
    let Some(on_upgrade) = req.extensions_mut().remove::<OnUpgrade>() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    tokio::spawn(async move {
        if let Ok(upgraded) = on_upgrade.await {
            run_port_forward(TokioIo::new(upgraded), pid).await;
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "SPDY/3.1")
        .header("X-Stream-Protocol-Version", "portforward.k8s.io")
        .body(Body::empty())
        .unwrap()
}

async fn run_port_forward<S>(io: S, pid: i32)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, wr) = tokio::io::split(io);
    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<Out>();
    tokio::spawn(writer_task(wr, out_rx));

    let mut inf = Inflater::new();
    // sid -> canal que alimenta o socket com os bytes vindos do cliente.
    let mut data_sinks: HashMap<u32, UnboundedSender<Vec<u8>>> = HashMap::new();

    let mut hdr = [0u8; 8];
    while rd.read_exact(&mut hdr).await.is_ok() {
        let len = ((hdr[5] as usize) << 16) | ((hdr[6] as usize) << 8) | (hdr[7] as usize);
        let mut payload = vec![0u8; len];
        if len > 0 && rd.read_exact(&mut payload).await.is_err() {
            break;
        }
        if hdr[0] & 0x80 != 0 {
            let kind = u16::from_be_bytes([hdr[2], hdr[3]]);
            match kind {
                SYN_STREAM => {
                    if payload.len() < 10 {
                        continue;
                    }
                    let sid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                        & 0x7fff_ffff;
                    let headers = decode_nv(&inf.block(&payload[10..]));
                    let get = |k: &str| {
                        headers
                            .iter()
                            .find(|(n, _)| n.eq_ignore_ascii_case(k))
                            .map(|(_, v)| v.clone())
                    };
                    let stype = get("streamtype").unwrap_or_default().to_ascii_lowercase();
                    let port: u16 = get("port").and_then(|p| p.trim().parse().ok()).unwrap_or(0);
                    let _ = out_tx.send(Out::SynReply(sid));
                    if stype == "data" && port != 0 {
                        match connect_in_netns(pid, port) {
                            Ok(std_sock) => {
                                let _ = std_sock.set_nonblocking(true);
                                if let Ok(sock) = tokio::net::TcpStream::from_std(std_sock) {
                                    let (cli_tx, cli_rx) =
                                        tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                                    data_sinks.insert(sid, cli_tx);
                                    tokio::spawn(pump_socket(sock, sid, out_tx.clone(), cli_rx));
                                }
                            }
                            Err(e) => {
                                // reporta no próprio data stream e fecha-o.
                                let msg = format!("port-forward {port}: {e}");
                                let _ = out_tx.send(Out::Data {
                                    sid,
                                    fin: true,
                                    buf: msg.into_bytes(),
                                });
                            }
                        }
                    }
                    // streams `error`: só SynReply (já feito); ficam abertas/vazias.
                }
                PING => {
                    if payload.len() >= 4 {
                        let _ = out_tx.send(Out::Ping(u32::from_be_bytes([
                            payload[0], payload[1], payload[2], payload[3],
                        ])));
                    }
                }
                WINDOW_UPDATE => {
                    if payload.len() >= 8 {
                        let sid =
                            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                                & 0x7fff_ffff;
                        let delta =
                            u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                        let _ = out_tx.send(Out::Credit {
                            sid,
                            delta: delta as i64,
                        });
                    }
                }
                GOAWAY => break,
                _ => {}
            }
        } else {
            // DATA frame do cliente → escreve no socket da porta respectiva.
            let sid = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) & 0x7fff_ffff;
            let flags = hdr[4];
            if let Some(sink) = data_sinks.get(&sid) {
                if !payload.is_empty() {
                    let _ = sink.send(payload);
                    // reabastece a janela (como no exec — o cliente não o faz).
                    let _ = out_tx.send(Out::SendWu {
                        sid,
                        delta: len as u32,
                    });
                    let _ = out_tx.send(Out::SendWu {
                        sid: 0,
                        delta: len as u32,
                    });
                }
                if flags & FLAG_FIN != 0 {
                    data_sinks.remove(&sid); // fecha a escrita para o socket
                }
            }
        }
    }
}

/// Bombeia um socket TCP do pod ↔ stream SPDY `data`: socket→cliente (Out::Data)
/// e cliente(cli_rx)→socket. Termina quando qualquer lado fecha.
async fn pump_socket(
    sock: tokio::net::TcpStream,
    sid: u32,
    out_tx: UnboundedSender<Out>,
    mut cli_rx: UnboundedReceiver<Vec<u8>>,
) {
    let (mut sr, mut sw) = sock.into_split();
    // socket → cliente
    let up = {
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATA];
            loop {
                match sr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out_tx
                            .send(Out::Data {
                                sid,
                                fin: false,
                                buf: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            let _ = out_tx.send(Out::Data {
                sid,
                fin: true,
                buf: Vec::new(),
            });
        })
    };
    // cliente → socket
    while let Some(chunk) = cli_rx.recv().await {
        if sw.write_all(&chunk).await.is_err() {
            break;
        }
    }
    let _ = sw.shutdown().await;
    let _ = up.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_adler_is_canonical() {
        // Adler-32 do dicionário SPDY/3 canónico.
        let mut c = Compress::new(Compression::default(), true);
        let adler = c.set_dictionary(SPDY3_DICT).unwrap();
        assert_eq!(adler, 0xe3c6a7c2, "dicionário SPDY/3 errado");
    }

    #[test]
    fn nv_roundtrip_through_zlib_dict() {
        // Dois blocos seguidos (estado contínuo), como num fluxo SPDY real.
        let mut def = Deflater::new();
        let mut inf = Inflater::new();

        let b1 = vec![("streamtype".to_string(), "stdin".to_string())];
        let c1 = def.block(&encode_nv(&b1));
        let d1 = decode_nv(&inf.block(&c1));
        assert_eq!(d1, b1, "primeiro bloco");

        let b2 = vec![
            ("streamtype".to_string(), "stdout".to_string()),
            ("x-extra".to_string(), "valor-mais-longo-aqui".to_string()),
        ];
        let c2 = def.block(&encode_nv(&b2));
        let d2 = decode_nv(&inf.block(&c2));
        assert_eq!(d2, b2, "segundo bloco (estado contínuo)");
    }
}

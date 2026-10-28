//! `ContainerService.Exec` — a command inside a running container, as a
//! bidirectional gRPC stream: the client's `stdin` frames flow in, the
//! process's `stdout`/`stderr` frames flow out, and the `exit_code` is the last
//! frame. The same attach the CLI's `container exec` does, lifted onto the wire.
//!
//! Like the up-path ([`crate::container_launch`]), this does NOT fork in
//! process — the server is multi-threaded and the engine's exec is a
//! single-threaded `setns`/`clone` dance. It re-execs the `delonix` CLI
//! (`container exec -i [-t] … <id> -- <cmd>`) in a fresh process and pumps that
//! child's stdio onto the stream. The CRI serves `exec`/`attach` the same way
//! (`delonix_cri::spdy`); this is the gRPC-native version of that bridge.
//!
//! Two stdio shapes:
//!
//! * **no tty** — three pipes. `stdout` and `stderr` stay apart (two frame
//!   kinds), `stdin` is the client's `Stdin` frames, and a `Resize` is a no-op
//!   (a pipe has no window).
//! * **tty** — one pseudo-terminal. The child's three fds are the slave; the
//!   master is read for `stdout` frames (a tty merges `stderr` in, so no
//!   `stderr` frame is ever sent), written from `Stdin` frames, and resized by
//!   `Resize` (`TIOCSWINSZ`).
//!
//! The child is reaped either when it exits (its code is the last frame), when
//! the deadline passes (`timeout`, killed, code reported), or when the client
//! drops the stream (`kill_on_drop`, nothing more sent). The `exit_code` frame
//! is sent only after the output pumps have drained, so it never races ahead of
//! the last byte of output.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Status, Streaming};

use crate::proto::v1::{exec_request, exec_response, ExecRequest, ExecResponse, ExecStart};

/// The channel between the pumps and the outbound gRPC stream.
type Tx = mpsc::Sender<Result<ExecResponse, Status>>;

/// Read size for every output pump — the 8 KiB the CRI's streaming loop uses.
const BUF: usize = 8192;

fn requested_namespace(namespace: &str) -> &str {
    if namespace.is_empty() {
        "default"
    } else {
        namespace
    }
}

fn stdout_frame(bytes: Vec<u8>) -> Result<ExecResponse, Status> {
    Ok(ExecResponse {
        frame: Some(exec_response::Frame::Stdout(bytes)),
    })
}

fn stderr_frame(bytes: Vec<u8>) -> Result<ExecResponse, Status> {
    Ok(ExecResponse {
        frame: Some(exec_response::Frame::Stderr(bytes)),
    })
}

fn exit_frame(code: i32) -> Result<ExecResponse, Status> {
    Ok(ExecResponse {
        frame: Some(exec_response::Frame::ExitCode(code)),
    })
}

/// The deadline as a `Duration`, or `None` when unset (`0`/absent means «no
/// timeout», as the CLI's own `exec` has none).
fn deadline_of(timeout: Option<pbjson_types::Duration>) -> Option<Duration> {
    let d = timeout?;
    if d.seconds <= 0 && d.nanos <= 0 {
        return None;
    }
    Some(Duration::new(
        d.seconds.max(0) as u64,
        d.nanos.max(0) as u32,
    ))
}

/// The first frame of an `Exec` stream must be `start` — exactly once, before
/// any `stdin`. Anything else is a malformed stream, refused before a child is
/// spawned. Split out of [`serve`] so it is unit-testable without a live
/// `Streaming`.
fn start_of(first: Option<ExecRequest>) -> Result<ExecStart, Status> {
    match first {
        Some(ExecRequest {
            frame: Some(exec_request::Frame::Start(s)),
        }) => Ok(s),
        Some(_) => Err(Status::invalid_argument(
            "Exec: the first frame must be `start`",
        )),
        None => Err(Status::invalid_argument(
            "Exec: the stream closed before the `start` frame",
        )),
    }
}

/// Resolve a running container by `(name, namespace)` to its node-wide id — the
/// id the CLI re-exec targets, the same identity rule the rest of the service
/// reads with. `NotFound` when it is absent or in another namespace,
/// `FailedPrecondition` when it is not running (an `exec` has nothing to enter).
fn resolve(root: &Path, name: &str, namespace: &str) -> Result<String, Status> {
    let missing = || Status::not_found(format!("container '{name}' in namespace '{namespace}'"));
    if namespace == "*" || !root.join("containers").is_dir() {
        return Err(missing());
    }
    let store = delonix_state::Store::open(root.join("containers"))
        .map_err(|e| crate::network_ops::status_of(&e.into()))?;
    let mut c = store.load(name).map_err(|e| {
        if e.is_not_found() {
            missing()
        } else {
            crate::network_ops::status_of(&e.into())
        }
    })?;
    if c.namespace != namespace {
        return Err(missing());
    }
    delonix_linux::reconcile_status(&mut c);
    if !matches!(
        c.status,
        delonix_model::records::Status::Running | delonix_model::records::Status::Paused
    ) {
        return Err(Status::failed_precondition(format!(
            "container '{name}' is not running"
        )));
    }
    Ok(c.id)
}

/// The `delonix container exec` argv for this `start`, targeting the resolved
/// node-wide `id`. `-i` always (the stream carries stdin); `-t` for a tty; `-e`
/// per env var (sorted, so the argv is deterministic); `-w`/`-u` when asked.
/// An empty command is left off — the CLI's shell fallback (`bash`→`sh`).
fn build_args(start: &ExecStart, id: &str) -> Vec<String> {
    let mut a = vec![
        "container".to_string(),
        "exec".to_string(),
        "-i".to_string(),
    ];
    if start.tty {
        a.push("-t".to_string());
    }
    let mut env: Vec<(&String, &String)> = start.env.iter().collect();
    env.sort();
    for (k, v) in env {
        a.push("-e".to_string());
        a.push(format!("{k}={v}"));
    }
    if !start.workdir.is_empty() {
        a.push("-w".to_string());
        a.push(start.workdir.clone());
    }
    if !start.user.is_empty() {
        a.push("-u".to_string());
        a.push(start.user.clone());
    }
    a.push(id.to_string());
    if !start.command.is_empty() {
        a.push("--".to_string());
        a.extend(start.command.iter().cloned());
    }
    a
}

/// A re-exec of `delonix` with this server's root, the machine-to-machine
/// delegation flag, and the built argv — the same shape [`crate::container_launch`]
/// and the CRI use. `kill_on_drop` so an abandoned child never outlives its task.
fn command(root: &Path, args: &[String]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(delonix_node::dispatch::cli_bin());
    cmd.env("DELONIX_ROOT", root)
        .env("DELONIX_INTERNAL", "1")
        .args(args)
        .kill_on_drop(true);
    cmd
}

/// `Exec` under `root`: resolve the target, re-exec the CLI, and hand back the
/// outbound stream. The first inbound frame (the `start`) has already shaped the
/// child; the rest of `inbound` is the live stdin, consumed by the pump.
pub async fn serve(
    root: PathBuf,
    mut inbound: Streaming<ExecRequest>,
) -> Result<ReceiverStream<Result<ExecResponse, Status>>, Status> {
    let first = inbound.message().await?;
    let start = start_of(first)?;

    let (r, name, ns) = (
        root.clone(),
        start.container.clone(),
        requested_namespace(&start.namespace).to_string(),
    );
    let id = tokio::task::spawn_blocking(move || resolve(&r, &name, &ns))
        .await
        .map_err(|e| Status::internal(format!("exec: resolving the container panicked: {e}")))??;

    let args = build_args(&start, &id);
    let deadline = deadline_of(start.timeout);
    let (tx, rx) = mpsc::channel(64);

    if start.tty {
        spawn_tty(&root, &args, deadline, inbound, tx)?;
    } else {
        spawn_piped(&root, &args, deadline, inbound, tx)?;
    }
    Ok(ReceiverStream::new(rx))
}

/// The no-tty path: three pipes, `stdout`/`stderr` kept apart.
fn spawn_piped(
    root: &Path,
    args: &[String],
    deadline: Option<Duration>,
    inbound: Streaming<ExecRequest>,
    tx: Tx,
) -> Result<(), Status> {
    let mut child = command(root, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_err)?;

    let mut cin = child.stdin.take().expect("piped stdin");
    let cout = child.stdout.take().expect("piped stdout");
    let cerr = child.stderr.take().expect("piped stderr");

    // Client stdin → the child, closing the child's stdin on EOF.
    let stdin_task = tokio::spawn(async move {
        let mut inbound = inbound;
        while let Ok(Some(req)) = inbound.message().await {
            match req.frame {
                Some(exec_request::Frame::Stdin(b)) => {
                    if cin.write_all(&b).await.is_err() {
                        break;
                    }
                }
                // A pipe has no window; a resize is nothing to apply, not an error.
                Some(exec_request::Frame::Resize(_))
                | Some(exec_request::Frame::Start(_))
                | None => {}
            }
        }
        drop(cin);
    });

    let out = tokio::spawn(pump_reader(cout, Stream::Stdout, tx.clone()));
    let err = tokio::spawn(pump_reader(cerr, Stream::Stderr, tx.clone()));

    tokio::spawn(async move {
        let code = reap(&mut child, deadline, &tx).await;
        stdin_task.abort();
        let _ = out.await;
        let _ = err.await;
        if let Some(code) = code {
            let _ = tx.send(exit_frame(code)).await;
        }
    });
    Ok(())
}

/// The tty path: one pty. The child's three fds are the slave; the master is
/// this side's read/write end.
fn spawn_tty(
    root: &Path,
    args: &[String],
    deadline: Option<Duration>,
    inbound: Streaming<ExecRequest>,
    tx: Tx,
) -> Result<(), Status> {
    let (master, slave) =
        open_pty().ok_or_else(|| Status::internal("exec: could not allocate a pseudo-terminal"))?;
    let stdio = || -> std::io::Result<Stdio> { Ok(slave.try_clone()?.into()) };
    let mut child = command(root, args)
        .stdin(stdio().map_err(spawn_err)?)
        .stdout(stdio().map_err(spawn_err)?)
        .stderr(stdio().map_err(spawn_err)?)
        .spawn()
        .map_err(spawn_err)?;
    // The child holds its own duplicates of the slave; ours is not needed.
    drop(slave);

    set_nonblocking(&master);
    // SAFETY: `register` only asks that the descriptor stay valid and open for
    // as long as the `AsyncFd` lives. `master` is an `OwnedFd` moved in, so the
    // `AsyncFd` owns it and closes it exactly once on drop — never out from
    // under tokio.
    let master = Arc::new(
        unsafe { AsyncFd::register(master) }
            .map_err(|e| Status::internal(format!("exec: watching the pty master: {e}")))?,
    );

    // Master → stdout frames (a tty merges stderr, so there is only one stream).
    let reader = tokio::spawn(pump_master(Arc::clone(&master), tx.clone()));

    // Client stdin → master; a resize sets the window.
    let writer = Arc::clone(&master);
    let stdin_task = tokio::spawn(async move {
        let mut inbound = inbound;
        while let Ok(Some(req)) = inbound.message().await {
            match req.frame {
                Some(exec_request::Frame::Stdin(b)) => {
                    if write_master(&writer, &b).await.is_err() {
                        break;
                    }
                }
                Some(exec_request::Frame::Resize(ts)) => {
                    set_winsize(writer.get_ref().as_raw_fd(), ts.rows, ts.cols);
                }
                Some(exec_request::Frame::Start(_)) | None => {}
            }
        }
    });

    tokio::spawn(async move {
        let code = reap(&mut child, deadline, &tx).await;
        stdin_task.abort();
        let _ = reader.await;
        if let Some(code) = code {
            let _ = tx.send(exit_frame(code)).await;
        }
    });
    Ok(())
}

fn spawn_err(e: std::io::Error) -> Status {
    Status::internal(format!(
        "exec: could not start `delonix container exec`: {e}"
    ))
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Pump a child pipe to the stream, one frame per read, until EOF or the client
/// goes away.
async fn pump_reader<R: AsyncReadExt + Unpin>(mut r: R, which: Stream, tx: Tx) {
    let mut buf = [0u8; BUF];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let frame = match which {
                    Stream::Stdout => stdout_frame(buf[..n].to_vec()),
                    Stream::Stderr => stderr_frame(buf[..n].to_vec()),
                };
                if tx.send(frame).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Pump the pty master to `stdout` frames. A read on a master whose slave has
/// closed returns `EIO`, not `0` — both end the pump.
async fn pump_master(master: Arc<AsyncFd<OwnedFd>>, tx: Tx) {
    loop {
        let Ok(mut guard) = master.readable().await else {
            break;
        };
        let read = guard.try_io(|inner| {
            let mut b = vec![0u8; BUF];
            // SAFETY: `b` is a live buffer and its exact length is passed; `inner`
            // owns a valid pty master fd for the duration of the call.
            let n = unsafe { libc::read(inner.as_raw_fd(), b.as_mut_ptr().cast(), b.len()) };
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            b.truncate(n as usize);
            Ok(b)
        });
        match read {
            // Would-block: readiness cleared, wait for the next.
            Err(_would_block) => continue,
            Ok(Err(_)) => break, // EIO/EOF: the slave closed.
            Ok(Ok(b)) if b.is_empty() => break,
            Ok(Ok(b)) => {
                if tx.send(stdout_frame(b)).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Write every byte to the pty master, awaiting writability between partial
/// writes — the duplex twin of [`pump_master`].
async fn write_master(master: &AsyncFd<OwnedFd>, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let mut guard = master.writable().await?;
        let wrote = guard.try_io(|inner| {
            // SAFETY: `data` is a live slice of its exact length; `inner` owns a
            // valid pty master fd for the duration of the call.
            let n = unsafe { libc::write(inner.as_raw_fd(), data.as_ptr().cast(), data.len()) };
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(n as usize)
        });
        match wrote {
            Err(_would_block) => continue,
            Ok(Err(e)) => return Err(e),
            Ok(Ok(n)) => data = &data[n..],
        }
    }
    Ok(())
}

/// Wait for the child, returning the exit code to send — or `None` when the
/// client dropped the stream (there is no one left to tell). On timeout the
/// child is killed and its resulting code reported (`124` if it carried none,
/// the `timeout(1)` convention).
async fn reap(
    child: &mut tokio::process::Child,
    deadline: Option<Duration>,
    tx: &Tx,
) -> Option<i32> {
    let sleep = async {
        match deadline {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(sleep);

    enum Done {
        Exited(i32),
        TimedOut,
        Disconnected,
    }
    let outcome = tokio::select! {
        st = child.wait() => Done::Exited(st.ok().and_then(|s| s.code()).unwrap_or(-1)),
        _ = tx.closed() => Done::Disconnected,
        _ = &mut sleep => Done::TimedOut,
    };
    match outcome {
        Done::Exited(code) => Some(code),
        Done::TimedOut => {
            let _ = child.start_kill();
            Some(
                child
                    .wait()
                    .await
                    .ok()
                    .and_then(|s| s.code())
                    .unwrap_or(124),
            )
        }
        Done::Disconnected => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            None
        }
    }
}

/// A pty pair (master, slave) with an 80×24 initial window. `None` if the host
/// refuses one.
fn open_pty() -> Option<(OwnedFd, OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut master: i32 = -1;
    let mut slave: i32 = -1;
    // SAFETY: `winsize` is four integers; all-zero is valid.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_row = 24;
    ws.ws_col = 80;
    // SAFETY: `openpty` fills master/slave; the two null pointers take defaults.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    if r != 0 {
        return None;
    }
    // SAFETY: `openpty` returned 0, so both are fresh descriptors nothing else
    // owns; each `OwnedFd` closes its own exactly once.
    Some(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

/// `AsyncFd` requires the descriptor be non-blocking, or a readiness-driven read
/// would block the runtime instead of yielding `WouldBlock`.
fn set_nonblocking(fd: &OwnedFd) {
    // SAFETY: direct FFI over our own valid descriptor; best-effort.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Apply a terminal window to the pty master (`TIOCSWINSZ`), the gRPC-native
/// twin of the CRI's JSON `{"Width","Height"}` resize.
fn set_winsize(master: i32, rows: u32, cols: u32) {
    // SAFETY: `winsize` is four integers; all-zero is valid.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_row = rows as u16;
    ws.ws_col = cols as u16;
    // SAFETY: `master` is a valid pty fd; `TIOCSWINSZ` reads one `winsize`.
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn start(tty: bool) -> ExecStart {
        ExecStart {
            container: "c".into(),
            namespace: "default".into(),
            command: vec![],
            env: HashMap::new(),
            workdir: String::new(),
            user: String::new(),
            tty,
            timeout: None,
        }
    }

    #[test]
    fn the_first_frame_must_be_start() {
        assert_eq!(
            start_of(None).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let stdin = ExecRequest {
            frame: Some(exec_request::Frame::Stdin(b"hi".to_vec())),
        };
        assert_eq!(
            start_of(Some(stdin)).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let s = ExecRequest {
            frame: Some(exec_request::Frame::Start(start(false))),
        };
        assert_eq!(start_of(Some(s)).unwrap().container, "c");
    }

    #[test]
    fn args_carry_i_tty_and_the_command_after_a_separator() {
        let mut s = start(true);
        s.command = vec!["echo".into(), "hello".into()];
        assert_eq!(
            build_args(&s, "abc123"),
            vec![
                "container",
                "exec",
                "-i",
                "-t",
                "abc123",
                "--",
                "echo",
                "hello"
            ]
        );
    }

    #[test]
    fn args_without_a_command_leave_the_shell_fallback_to_the_cli() {
        let s = start(false);
        assert_eq!(
            build_args(&s, "abc123"),
            vec!["container", "exec", "-i", "abc123"]
        );
    }

    #[test]
    fn args_sort_env_and_place_workdir_and_user_before_the_id() {
        let mut s = start(false);
        s.env = HashMap::from([("B".into(), "2".into()), ("A".into(), "1".into())]);
        s.workdir = "/srv".into();
        s.user = "1000:1000".into();
        s.command = vec!["id".into()];
        assert_eq!(
            build_args(&s, "cid"),
            vec![
                "container",
                "exec",
                "-i",
                "-e",
                "A=1",
                "-e",
                "B=2",
                "-w",
                "/srv",
                "-u",
                "1000:1000",
                "cid",
                "--",
                "id"
            ]
        );
    }

    #[test]
    fn a_zero_or_absent_timeout_means_no_deadline() {
        assert!(deadline_of(None).is_none());
        assert!(deadline_of(Some(pbjson_types::Duration {
            seconds: 0,
            nanos: 0
        }))
        .is_none());
        assert_eq!(
            deadline_of(Some(pbjson_types::Duration {
                seconds: 5,
                nanos: 0
            })),
            Some(Duration::from_secs(5))
        );
    }

    #[tokio::test]
    async fn exec_into_a_missing_container_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve(dir.path(), "ghost", "default").unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }
}

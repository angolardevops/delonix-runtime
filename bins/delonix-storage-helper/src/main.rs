//! `delonix-storage-helper` — the privileged side of the storage pools
//! (ADR-0067 D3, option A).
//!
//! Two ways to run it, and only two:
//!
//! - **No arguments: one connection.** systemd's `delonix-storage-helper.socket`
//!   (`Accept=yes`) starts one instance per connection with the connection on
//!   standard input. It reads ONE request, asks the kernel who sent it
//!   (`SO_PEERCRED`), serves it within the administrator's allowlist and
//!   replies. It never listens, never stays, and never takes a uid, a device,
//!   a path or a command from the request: the uid is the kernel's, the pool
//!   comes from `/etc/delonix/storage-pools.yaml`, and the request's fields are
//!   refused one by one before any tool runs (`delonix_storage::helper`).
//! - **`create-pool <name> [--wipe-devices]` / `destroy-pool <name>`**: the
//!   administrator, at a terminal, as root. They are never served over the
//!   socket.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use delonix_provider_btrfs::BtrfsDriver;
use delonix_provider_lvm::LvmThinDriver;
use delonix_provider_zfs::ZfsDriver;
use delonix_storage::admin::{self, PoolAdmin};
use delonix_storage::allowlist::{self, Allowlist};
use delonix_storage::exec::Run;
use delonix_storage::helper::{self, Outcome, Reply, ServeContext, MAX_REQUEST_BYTES};
use delonix_storage::pool::{PoolRef, StoragePoolDriver};
use delonix_storage::{Error, Result};
use delonix_volume::pool_exec::SystemRun;

/// How long a caller may take to send its request. The connection is the
/// caller's; a request that never ends must not keep a root process.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The `--version` line.
fn version_line() -> String {
    format!("delonix-storage-helper {}", env!("CARGO_PKG_VERSION"))
}

const USAGE: &str = concat!(
    "usage: delonix-storage-helper                       serve one connection (systemd)\n",
    "       delonix-storage-helper create-pool <name> [--wipe-devices]\n",
    "       delonix-storage-helper destroy-pool <name>\n",
    "       delonix-storage-helper --version",
);

#[derive(Debug, PartialEq)]
enum Command {
    Version,
    Help,
    Serve,
    CreatePool { name: String, wipe: bool },
    DestroyPool { name: String },
}

fn usage_error(text: String) -> Error {
    Error::Engine(delonix_model::Error::Invalid(format!("{text}\n{USAGE}")))
}

fn parse_args(args: &[String]) -> Result<Command> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        [] => Ok(Command::Serve),
        ["--version" | "-V"] => Ok(Command::Version),
        ["--help" | "-h"] => Ok(Command::Help),
        ["create-pool", name] => Ok(Command::CreatePool {
            name: name.to_string(),
            wipe: false,
        }),
        ["create-pool", name, "--wipe-devices"] | ["create-pool", "--wipe-devices", name] => {
            Ok(Command::CreatePool {
                name: name.to_string(),
                wipe: true,
            })
        }
        ["destroy-pool", name] => Ok(Command::DestroyPool {
            name: name.to_string(),
        }),
        _ => Err(usage_error(format!(
            "unexpected arguments: {}",
            args.join(" ")
        ))),
    }
}

// ---- the request side ------------------------------------------------------

/// The allowlist, from the root-owned path only. The environment variable the
/// engine honours is NOT read here: whoever could set it would choose which
/// pools a root process works on.
fn load_allowlist() -> Result<Allowlist> {
    let file = Path::new(allowlist::DEFAULT_PATH);
    allowlist::load_from(file, |text| {
        serde_yaml::from_str(text).map_err(|e| e.to_string())
    })?
    .ok_or_else(|| {
        Error::PoolUnavailable(format!(
            "there is no storage pool allowlist at {} — the administrator declares the pools \
             there",
            file.display()
        ))
    })
}

fn run() -> Arc<dyn Run> {
    Arc::new(SystemRun)
}

/// The drivers this helper runs. `dir` is not one of them: a `dir` pool is
/// always the engine's own process.
fn driver_for(id: &str) -> Option<Arc<dyn StoragePoolDriver>> {
    let mounts = PathBuf::from(helper::MOUNT_ROOT);
    match id {
        delonix_provider_lvm::ID => Some(Arc::new(LvmThinDriver::new(run(), mounts))),
        delonix_provider_zfs::ID => Some(Arc::new(ZfsDriver::new(run(), mounts))),
        delonix_provider_btrfs::ID => Some(Arc::new(BtrfsDriver::new(run(), None, true))),
        _ => None,
    }
}

/// The pool administration of a driver.
fn admin_for(id: &str) -> Option<Box<dyn PoolAdmin>> {
    let mounts = PathBuf::from(helper::MOUNT_ROOT);
    match id {
        delonix_provider_lvm::ID => Some(Box::new(LvmThinDriver::new(run(), mounts))),
        delonix_provider_zfs::ID => Some(Box::new(ZfsDriver::new(run(), mounts))),
        delonix_provider_btrfs::ID => Some(Box::new(BtrfsDriver::new(run(), None, true))),
        _ => None,
    }
}

/// A user name → uid, from the password database.
fn uid_of(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: `passwd` is a C struct of pointers and integers; all-zero is valid.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and `buf` outlives `pw`'s use.
    let rc =
        unsafe { libc::getpwnam_r(c.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    (rc == 0 && !out.is_null()).then_some(pw.pw_uid)
}

/// Is `fd` a connected unix stream socket? Anything else on standard input
/// means the helper was run by hand instead of by its socket unit.
fn check_socket(fd: RawFd) -> Result<()> {
    let opt = |name| -> Option<libc::c_int> {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `v`/`len` are valid out-pointers of the size passed.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                &mut v as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        (rc == 0).then_some(v)
    };
    if opt(libc::SO_DOMAIN) == Some(libc::AF_UNIX) && opt(libc::SO_TYPE) == Some(libc::SOCK_STREAM)
    {
        Ok(())
    } else {
        Err(Error::HelperUnavailable(
            "standard input is not a unix stream socket — this helper is started by \
             delonix-storage-helper.socket, one instance per connection; an administrator runs \
             `create-pool`/`destroy-pool`"
                .into(),
        ))
    }
}

/// The uid of the process at the other end, from the kernel.
fn peer_uid(stream: &UnixStream) -> Result<u32> {
    // SAFETY: `ucred` is a C struct of integers; all-zero is valid.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred`/`len` are valid out-pointers of the size passed.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 {
        Ok(cred.uid)
    } else {
        Err(Error::HelperUnavailable(format!(
            "the caller's credentials cannot be read: {}",
            std::io::Error::last_os_error()
        )))
    }
}

/// Reads the request: until the caller shuts its side, at most
/// [`MAX_REQUEST_BYTES`]. One byte more is a refusal, never a truncation.
fn read_request(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut bytes = Vec::new();
    stream
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::HelperRequestInvalid(format!("the request could not be read: {e}")))?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(Error::HelperRequestInvalid(format!(
            "a request is at most {MAX_REQUEST_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

/// Serves one connection and returns the line the journal gets. The reply is
/// written whatever happened: a caller is never left without an answer.
fn serve_connection(
    stream: &mut UnixStream,
    list: Result<Allowlist>,
    driver: &dyn Fn(&str) -> Option<Arc<dyn StoragePoolDriver>>,
    uid_of: &dyn Fn(&str) -> Option<u32>,
) -> String {
    // The request is read FIRST, whatever is refused after: closing a unix
    // socket with unread bytes resets the connection, and the caller would
    // get «connection reset» instead of the refusal.
    let bytes = read_request(stream);
    let uid = peer_uid(stream);
    let parse_size = |s: &str| delonix_volume::parse_size_bytes(s);
    let result = uid.as_ref().map_err(clone_error).and_then(|&uid| {
        // The request's grammar first: a malformed or smuggling request is
        // refused as such, whatever the state of the allowlist.
        let bytes = bytes?;
        helper::Request::decode(&bytes)?.check()?;
        let list = list?;
        let ctx = ServeContext {
            allowlist: &list,
            driver,
            uid_of,
            parse_size: &parse_size,
        };
        helper::serve(&bytes, uid, &ctx)
    });
    let who = match &uid {
        Ok(u) => format!("uid={u}"),
        Err(_) => "uid=?".into(),
    };
    let line = match &result {
        Ok(o) => format!("{who} ok {}", describe(o)),
        Err(e) => format!(
            "{who} refused {}: {e}",
            delonix_model::codes::label(e.number())
        ),
    };
    let reply = Reply::of(result).encode();
    let _ = stream.write_all(&reply);
    let _ = stream.flush();
    line
}

fn clone_error(e: &Error) -> Error {
    Error::from_number(e.number(), e.to_string())
}

/// What was done, for the journal. Built from the outcome, whose names were
/// validated before any driver ran.
fn describe(o: &Outcome) -> String {
    match o {
        Outcome::Probe { probe } => format!("probe {}", probe.label()),
        Outcome::Usage { .. } => "usage".into(),
        Outcome::Allocation { allocation: a } => format!(
            "allocate pool={} volume={} adopted={}",
            a.pool, a.volume, a.adopted
        ),
        Outcome::Released => "release".into(),
    }
}

fn serve_stdin() -> Result<()> {
    check_socket(0)?;
    // SAFETY: fd 0 was just verified to be a unix stream socket, and this
    // process owns it for the rest of its life.
    let mut stream = unsafe { UnixStream::from_raw_fd(0) };
    let line = serve_connection(&mut stream, load_allowlist(), &driver_for, &uid_of);
    eprintln!("{line}");
    Ok(())
}

// ---- the administrator's side ----------------------------------------------

fn admin_command(name: &str, create: Option<bool>) -> Result<String> {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error::AdminNeedsRoot(format!(
            "`{}` builds or unmakes a pool on block devices — run it as root (`sudo \
             delonix-storage-helper …`)",
            if create.is_some() {
                "create-pool"
            } else {
                "destroy-pool"
            }
        )));
    }
    let list = load_allowlist()?;
    let entry = list.pools.get(name).ok_or_else(|| {
        Error::NotAllowed(format!(
            "storage pool '{name}' is not in {}",
            allowlist::DEFAULT_PATH
        ))
    })?;
    let admin = admin_for(&entry.driver).ok_or_else(|| {
        Error::InvalidPoolRequest(format!(
            "storage pool '{name}': driver '{}' has no pool administration — a `dir` pool is a \
             directory the administrator makes and removes",
            entry.driver
        ))
    })?;
    let pool = PoolRef { name, entry };
    match create {
        Some(wipe) => admin::create_checked(
            admin.as_ref(),
            &SystemRun,
            &admin::resolve_block_device,
            &pool,
            wipe,
        ),
        None => admin::destroy_guarded(admin.as_ref(), &pool),
    }
}

fn main_with(cmd: Command) -> Result<()> {
    match cmd {
        Command::Version => println!("{}", version_line()),
        Command::Help => println!("{USAGE}"),
        Command::Serve => serve_stdin()?,
        Command::CreatePool { name, wipe } => println!("{}", admin_command(&name, Some(wipe))?),
        Command::DestroyPool { name } => println!("{}", admin_command(&name, None)?),
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args).and_then(main_with) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "delonix-storage-helper: {}: {e}",
                delonix_model::codes::label(e.number())
            );
            let dx = delonix_model::Error::from(e);
            ExitCode::from(delonix_model::exitcode::for_error(&dx).clamp(1, 255) as u8)
        }
    }
}

#[cfg(test)]
mod tests;

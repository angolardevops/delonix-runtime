//! The privileged storage helper's protocol (ADR-0067 D3, phase P1).
//!
//! Some pool operations need root — everything LVM does, mounting a ZFS
//! dataset. The engine runs without root, so those cross into
//! `delonix-storage-helper`: socket-activated, one process per connection, gone
//! when the request is answered. This module is the whole contract between the
//! two sides, so the engine and the helper cannot disagree about it:
//!
//! - [`Request`]: ONE JSON object per connection. It names a pool, an
//!   operation, and for a volume its name, shape and size, plus the engine's
//!   state root (only ever used as a label in the owner stamp). It never
//!   carries a path, a device, an option or a command — the helper takes all
//!   of those from the root-owned allowlist.
//! - [`Request::check`]: the validation the helper runs before anything else.
//! - [`serve`]: what the helper does with a request, given the caller's uid as
//!   the kernel reported it (`SO_PEERCRED`), the allowlist it read itself, and
//!   the drivers it was built with. Pure of sockets, so it is tested as data.
//! - [`Routed`]: the engine side. It wraps a driver and sends the operations
//!   that driver says need the helper across the socket; everything else runs
//!   in the engine's own process.
//!
//! Pool creation and destruction are never on the socket: they are
//! administrator commands run at a terminal (ADR-0067 D3.3, D5).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::allowlist::{Allowlist, Entry};
use crate::ownership::Owner;
use crate::pool::{
    valid_volume_name, Allocation, PoolOp, PoolProbe, PoolRef, PoolUsage, Privilege,
    StoragePoolDriver, VolumeRequest, VolumeShape,
};
use crate::{Error, Result};

/// The socket the helper's `.socket` unit listens on.
pub const DEFAULT_SOCKET: &str = "/run/delonix-storage-helper.sock";

/// Names another socket, for the ENGINE side only (tests, a non-standard
/// install). The helper itself is handed its socket by systemd.
pub const SOCKET_ENV: &str = "DELONIX_STORAGE_HELPER_SOCKET";

/// Where the helper mounts the filesystems it makes (LVM volumes, ZFS
/// datasets). One fixed root, so the unit's `ReadWritePaths=` can name it and
/// no request can point a mount anywhere else.
pub const MOUNT_ROOT: &str = "/var/lib/delonix-storage/mounts";

/// The protocol version a request carries.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest request the helper reads. A real one is a few hundred bytes;
/// anything bigger is refused before it is parsed.
pub const MAX_REQUEST_BYTES: usize = 4096;

/// The largest reply the engine reads.
pub const MAX_REPLY_BYTES: usize = 64 * 1024;

/// The largest volume the protocol carries (1 PiB). The pool's own
/// `maxVolumeBytes` is checked as well; this bounds what a request can say.
pub const MAX_SIZE_BYTES: u64 = 1 << 50;

/// The longest state root a request may carry.
pub const MAX_ROOT_BYTES: usize = 1024;

/// One request, as it crosses the socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub v: u32,
    /// `probe`, `usage`, `allocate` or `release`.
    pub op: String,
    pub pool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    /// `filesystem` or `block`; `allocate` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
    /// `allocate` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// The engine state root of the caller; the owner stamp is
    /// `<caller uid>:<root>`. `allocate` and `release` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

/// An operation the socket serves, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Probe,
    Usage,
    Allocate {
        volume: String,
        shape: VolumeShape,
        size_bytes: u64,
        root: String,
    },
    Release {
        volume: String,
        root: String,
    },
}

/// A request that passed [`Request::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub pool: String,
    pub op: Op,
}

fn invalid(why: impl Into<String>) -> Error {
    Error::HelperRequestInvalid(why.into())
}

/// Is `root` an acceptable state root to put in a stamp? An absolute path
/// with no `..`, no control characters, of bounded length. The helper never
/// opens it; it is checked because it ends up written onto a volume.
pub fn valid_root(root: &str) -> bool {
    !root.is_empty()
        && root.len() <= MAX_ROOT_BYTES
        && !root.chars().any(char::is_control)
        && Path::new(root).is_absolute()
        && !Path::new(root)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
}

impl Request {
    pub fn new(op: &str, pool: &str) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            op: op.into(),
            pool: pool.into(),
            volume: None,
            shape: None,
            size_bytes: None,
            root: None,
        }
    }

    /// Reads one request, refusing what is too large or does not parse.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(invalid(format!(
                "the request is {} bytes; the helper reads at most {MAX_REQUEST_BYTES}",
                bytes.len()
            )));
        }
        serde_json::from_slice(bytes)
            .map_err(|e| invalid(format!("the request is not a helper request: {e}")))
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// The validation the helper runs on every request before it touches the
    /// allowlist. Every field is checked for the operation it belongs to, and
    /// a field an operation does not take is refused, not ignored.
    pub fn check(&self) -> Result<Checked> {
        if self.v != PROTOCOL_VERSION {
            return Err(invalid(format!(
                "protocol version {} is not served (this helper speaks {PROTOCOL_VERSION})",
                self.v
            )));
        }
        if !crate::allowlist::valid_pool_name(&self.pool) {
            return Err(invalid(format!(
                "'{}' is not a pool name",
                printable(&self.pool)
            )));
        }
        let given = |f: bool, name: &str| -> Result<()> {
            if f {
                Err(invalid(format!(
                    "operation '{}' does not take `{name}`",
                    self.op
                )))
            } else {
                Ok(())
            }
        };
        let volume = |v: &Option<String>| -> Result<String> {
            let v = v.as_deref().ok_or_else(|| invalid("`volume` is missing"))?;
            if !valid_volume_name(v) {
                return Err(invalid(format!("'{}' is not a volume name", printable(v))));
            }
            Ok(v.to_string())
        };
        let root = |r: &Option<String>| -> Result<String> {
            let r = r.as_deref().ok_or_else(|| invalid("`root` is missing"))?;
            if !valid_root(r) {
                return Err(invalid(
                    "`root` must be an absolute path without '..' or control characters, at \
                     most 1024 bytes",
                ));
            }
            Ok(r.to_string())
        };
        let op = match self.op.as_str() {
            "probe" | "usage" => {
                given(self.volume.is_some(), "volume")?;
                given(self.shape.is_some(), "shape")?;
                given(self.size_bytes.is_some(), "size_bytes")?;
                given(self.root.is_some(), "root")?;
                if self.op == "probe" {
                    Op::Probe
                } else {
                    Op::Usage
                }
            }
            "allocate" => {
                let shape = self
                    .shape
                    .as_deref()
                    .ok_or_else(|| invalid("`shape` is missing"))?;
                let shape = VolumeShape::parse(shape).ok_or_else(|| {
                    invalid(format!(
                        "shape '{}' is not filesystem or block",
                        printable(shape)
                    ))
                })?;
                let size_bytes = self
                    .size_bytes
                    .ok_or_else(|| invalid("`size_bytes` is missing"))?;
                if size_bytes == 0 || size_bytes > MAX_SIZE_BYTES {
                    return Err(invalid(format!(
                        "size_bytes {size_bytes} is outside 1..={MAX_SIZE_BYTES}"
                    )));
                }
                Op::Allocate {
                    volume: volume(&self.volume)?,
                    shape,
                    size_bytes,
                    root: root(&self.root)?,
                }
            }
            "release" => {
                given(self.shape.is_some(), "shape")?;
                given(self.size_bytes.is_some(), "size_bytes")?;
                Op::Release {
                    volume: volume(&self.volume)?,
                    root: root(&self.root)?,
                }
            }
            "create-pool" | "destroy-pool" => {
                return Err(invalid(format!(
                    "'{}' is never served over the socket: an administrator runs \
                     `delonix-storage-helper {} <pool>` as root at a terminal",
                    self.op, self.op
                )))
            }
            other => {
                return Err(invalid(format!(
                    "'{}' is not an operation (probe, usage, allocate, release)",
                    printable(other)
                )))
            }
        };
        Ok(Checked {
            pool: self.pool.clone(),
            op,
        })
    }
}

/// A value from a request, made safe to echo in a message and a log line.
fn printable(s: &str) -> String {
    let s: String = s
        .chars()
        .take(64)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    s
}

/// What a served request produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Probe { probe: PoolProbe },
    Usage { usage: PoolUsage },
    Allocation { allocation: Allocation },
    Released,
}

/// The reply, as it crosses the socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    Ok { result: Outcome },
    Err { number: u16, message: String },
}

impl Reply {
    pub fn of(r: Result<Outcome>) -> Self {
        match r {
            Ok(result) => Reply::Ok { result },
            Err(e) => Reply::Err {
                number: e.number(),
                message: e.to_string(),
            },
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn into_result(self) -> Result<Outcome> {
        match self {
            Reply::Ok { result } => Ok(result),
            Reply::Err { number, message } => Err(Error::from_number(number, message)),
        }
    }
}

// ---- the helper's side -------------------------------------------------------

/// What the helper needs from its host to serve a request. The binary gives
/// the real ones; the tests give fakes.
pub struct ServeContext<'a> {
    /// The allowlist the helper read itself, from the root-owned path.
    pub allowlist: &'a Allowlist,
    /// The driver for an id, privileged (it runs the tools directly).
    pub driver: &'a dyn Fn(&str) -> Option<Arc<dyn StoragePoolDriver>>,
    /// A user name in `allowUsers` → its uid (the password database).
    pub uid_of: &'a dyn Fn(&str) -> Option<u32>,
    /// The size grammar of `maxVolumeBytes`.
    pub parse_size: &'a dyn Fn(&str) -> Option<u64>,
}

/// Does the pool's `allowUsers` name `uid`? An entry is a user name or a
/// numeric uid. An empty list serves nobody.
pub fn allows(entry: &Entry, uid: u32, uid_of: &dyn Fn(&str) -> Option<u32>) -> bool {
    entry.allow_users.iter().any(|u| match u.parse::<u32>() {
        Ok(n) => n == uid,
        Err(_) => uid_of(u) == Some(uid),
    })
}

/// Serves one request from the caller whose uid the kernel reported. Every
/// refusal happens before a driver is asked to do anything.
pub fn serve(bytes: &[u8], peer_uid: u32, ctx: &ServeContext) -> Result<Outcome> {
    let req = Request::decode(bytes)?.check()?;
    let entry = ctx.allowlist.pools.get(&req.pool).ok_or_else(|| {
        Error::NotAllowed(format!(
            "storage pool '{}' is not in the administrator's allowlist",
            req.pool
        ))
    })?;
    if !allows(entry, peer_uid, ctx.uid_of) {
        return Err(Error::CallerNotAllowed(format!(
            "storage pool '{}': uid {peer_uid} is not in its allowUsers",
            req.pool
        )));
    }
    let driver = (ctx.driver)(&entry.driver).ok_or_else(|| {
        Error::PoolUnavailable(format!(
            "storage pool '{}': driver '{}' is not in this helper",
            req.pool, entry.driver
        ))
    })?;
    let pool = PoolRef {
        name: &req.pool,
        entry,
    };
    match req.op {
        Op::Probe => Ok(Outcome::Probe {
            probe: driver.probe(&pool),
        }),
        Op::Usage => Ok(Outcome::Usage {
            usage: driver.usage(&pool),
        }),
        Op::Allocate {
            volume,
            shape,
            size_bytes,
            root,
        } => {
            if let Some(max) = entry.max_volume_bytes(&req.pool, ctx.parse_size)? {
                if size_bytes > max {
                    return Err(Error::NotAllowed(format!(
                        "storage pool '{}': {size_bytes} bytes is above its maxVolumeBytes \
                         ({max})",
                        req.pool
                    )));
                }
            }
            let allocation = driver.allocate(
                &pool,
                &VolumeRequest {
                    name: volume,
                    shape,
                    size_bytes,
                },
                &Owner::of(peer_uid, &root),
            )?;
            Ok(Outcome::Allocation { allocation })
        }
        Op::Release { volume, root } => {
            driver.release(&pool, &volume, &Owner::of(peer_uid, &root))?;
            Ok(Outcome::Released)
        }
    }
}

// ---- the engine's side -------------------------------------------------------

/// Sends one request to the helper.
pub trait HelperCall: Send + Sync {
    fn call(&self, req: &Request) -> Result<Outcome>;
}

/// The socket the engine talks to: [`SOCKET_ENV`] when set, else
/// [`DEFAULT_SOCKET`].
pub fn socket_path() -> PathBuf {
    match std::env::var_os(SOCKET_ENV) {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(DEFAULT_SOCKET),
    }
}

/// The helper over its unix socket.
pub struct SocketClient {
    pub path: PathBuf,
    /// How long one request may take — a `mkfs` on a large volume included.
    pub timeout: Duration,
}

impl SocketClient {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            timeout: Duration::from_secs(300),
        }
    }
}

/// The remedy every «no helper» refusal names.
pub const HELPER_REMEDY: &str = "enable the helper (`systemctl enable --now \
     delonix-storage-helper.socket`) and add this user to the pool's `allowUsers`, or run the \
     engine as root with `mode: in-process` in the allowlist";

impl HelperCall for SocketClient {
    fn call(&self, req: &Request) -> Result<Outcome> {
        let unavailable = |what: String| {
            Error::HelperUnavailable(format!(
                "the storage helper at {} {what} — {HELPER_REMEDY}",
                self.path.display()
            ))
        };
        let mut s = UnixStream::connect(&self.path).map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => unavailable(format!(
                "refused this user ({e}); the socket is for the `delonix-storage` group"
            )),
            _ => unavailable(format!("cannot be reached ({e})")),
        })?;
        let _ = s.set_read_timeout(Some(self.timeout));
        let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
        s.write_all(&req.encode())
            .and_then(|_| s.shutdown(std::net::Shutdown::Write))
            .map_err(|e| unavailable(format!("did not take the request ({e})")))?;
        let mut buf = Vec::new();
        s.take(MAX_REPLY_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| unavailable(format!("did not answer ({e})")))?;
        if buf.is_empty() {
            return Err(unavailable(
                "closed the connection without an answer".into(),
            ));
        }
        if buf.len() > MAX_REPLY_BYTES {
            return Err(unavailable("answered more than a reply can hold".into()));
        }
        let reply: Reply = serde_json::from_slice(&buf)
            .map_err(|e| unavailable(format!("answered something that is not a reply ({e})")))?;
        reply.into_result()
    }
}

/// How the operations a driver says need root are carried out on this node.
#[derive(Clone)]
pub enum Privileged {
    /// The engine itself runs as root, configured by the administrator
    /// (`mode: in-process`): call the driver directly.
    InProcess,
    /// Send them to the helper.
    Helper(Arc<dyn HelperCall>),
}

/// A driver as the engine uses it: the operations it can do unprivileged (or
/// by delegation) run here, the ones it needs root for go to the helper.
pub struct Routed {
    inner: Arc<dyn StoragePoolDriver>,
    privileged: Privileged,
}

impl Routed {
    pub fn new(inner: Arc<dyn StoragePoolDriver>, privileged: Privileged) -> Self {
        Self { inner, privileged }
    }

    fn helper(&self, op: PoolOp) -> Option<&Arc<dyn HelperCall>> {
        match (self.inner.required(op), &self.privileged) {
            (_, Privileged::InProcess) => None,
            (Privilege::Helper, Privileged::Helper(h)) => Some(h),
            _ => None,
        }
    }

    fn root_of(owner: &Owner) -> Result<String> {
        owner.parts().map(|(_, r)| r.to_string()).ok_or_else(|| {
            invalid(format!(
                "owner '{}' is not <uid>:<state root>",
                printable(owner.id())
            ))
        })
    }

    fn undetermined(&self, why: String) -> PoolProbe {
        PoolProbe::Undetermined {
            reason: format!("driver '{}': {why}", self.inner.id()),
            remedy: HELPER_REMEDY.into(),
        }
    }
}

impl StoragePoolDriver for Routed {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn probe(&self, pool: &PoolRef) -> PoolProbe {
        match self.helper(PoolOp::Probe) {
            None => {
                let local = self.inner.probe(pool);
                // A delegated probe that could not look locally may still be
                // answered by the helper, which can.
                if let (
                    PoolProbe::Undetermined { .. },
                    Privilege::Delegated(_),
                    Privileged::Helper(h),
                ) = (&local, self.inner.required(PoolOp::Probe), &self.privileged)
                {
                    if let Ok(Outcome::Probe { probe }) = h.call(&Request::new("probe", pool.name))
                    {
                        return probe;
                    }
                }
                local
            }
            Some(h) => match h.call(&Request::new("probe", pool.name)) {
                Ok(Outcome::Probe { probe }) => probe,
                Ok(other) => self.undetermined(format!("the helper answered {other:?}")),
                Err(e) => self.undetermined(e.to_string()),
            },
        }
    }

    fn required(&self, op: PoolOp) -> Privilege {
        self.inner.required(op)
    }

    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation> {
        let Some(h) = self.helper(PoolOp::Allocate(req.shape)) else {
            return self.inner.allocate(pool, req, owner);
        };
        let mut r = Request::new("allocate", pool.name);
        r.volume = Some(req.name.clone());
        r.shape = Some(req.shape.label().into());
        r.size_bytes = Some(req.size_bytes);
        r.root = Some(Self::root_of(owner)?);
        match h.call(&r)? {
            Outcome::Allocation { allocation } => Ok(allocation),
            other => Err(Error::PoolUnavailable(format!(
                "storage pool '{}': the helper answered {other:?} to an allocation",
                pool.name
            ))),
        }
    }

    fn release(&self, pool: &PoolRef, volume: &str, owner: &Owner) -> Result<()> {
        let Some(h) = self.helper(PoolOp::Release) else {
            return self.inner.release(pool, volume, owner);
        };
        let mut r = Request::new("release", pool.name);
        r.volume = Some(volume.into());
        r.root = Some(Self::root_of(owner)?);
        match h.call(&r)? {
            Outcome::Released => Ok(()),
            other => Err(Error::PoolUnavailable(format!(
                "storage pool '{}': the helper answered {other:?} to a release",
                pool.name
            ))),
        }
    }

    fn usage(&self, pool: &PoolRef) -> PoolUsage {
        let ask = |h: &Arc<dyn HelperCall>| match h.call(&Request::new("usage", pool.name)) {
            Ok(Outcome::Usage { usage }) => usage,
            _ => PoolUsage::default(),
        };
        match self.helper(PoolOp::Usage) {
            Some(h) => ask(h),
            None => {
                let local = self.inner.usage(pool);
                match (&self.privileged, self.inner.required(PoolOp::Usage)) {
                    (Privileged::Helper(h), Privilege::Delegated(_))
                        if local.capacity_bytes.is_none() =>
                    {
                        ask(h)
                    }
                    _ => local,
                }
            }
        }
    }
}

/// The routing for the operations a driver needs root for when no helper is
/// configured: every one is refused before a command runs, with exit class
/// 69, and a probe says «could not determine» — never «no pool».
pub struct NoHelper {
    inner: Arc<dyn StoragePoolDriver>,
}

impl NoHelper {
    pub fn new(inner: Arc<dyn StoragePoolDriver>) -> Self {
        Self { inner }
    }

    fn refuse(&self, pool: &str) -> Error {
        Error::HelperUnavailable(format!(
            "storage pool '{pool}': driver '{}' needs root for this, and no storage helper is \
             configured — {HELPER_REMEDY}",
            self.inner.id()
        ))
    }
}

impl StoragePoolDriver for NoHelper {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn probe(&self, pool: &PoolRef) -> PoolProbe {
        if self.inner.required(PoolOp::Probe) == Privilege::Helper {
            return PoolProbe::Undetermined {
                reason: format!(
                    "driver '{}' reads the pool's state only as root, and no storage helper is \
                     configured",
                    self.inner.id()
                ),
                remedy: HELPER_REMEDY.into(),
            };
        }
        self.inner.probe(pool)
    }

    fn required(&self, op: PoolOp) -> Privilege {
        self.inner.required(op)
    }

    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation> {
        if self.inner.required(PoolOp::Allocate(req.shape)) == Privilege::Helper {
            return Err(self.refuse(pool.name));
        }
        self.inner.allocate(pool, req, owner)
    }

    fn release(&self, pool: &PoolRef, volume: &str, owner: &Owner) -> Result<()> {
        if self.inner.required(PoolOp::Release) == Privilege::Helper {
            return Err(self.refuse(pool.name));
        }
        self.inner.release(pool, volume, owner)
    }

    fn usage(&self, pool: &PoolRef) -> PoolUsage {
        if self.inner.required(PoolOp::Usage) == Privilege::Helper {
            return PoolUsage::default();
        }
        self.inner.usage(pool)
    }
}

/// The driver as the engine must use it on this node, given how the
/// allowlist says privileged operations run (`mode`) and whether a helper
/// client exists. The ONE place that decision is made.
pub fn route(
    inner: Arc<dyn StoragePoolDriver>,
    list: &Allowlist,
    helper: Option<Arc<dyn HelperCall>>,
) -> Arc<dyn StoragePoolDriver> {
    match (list.mode.as_deref(), helper) {
        (Some("in-process"), _) => Arc::new(Routed::new(inner, Privileged::InProcess)),
        (_, Some(h)) => Arc::new(Routed::new(inner, Privileged::Helper(h))),
        (_, None) => Arc::new(NoHelper::new(inner)),
    }
}

#[cfg(test)]
mod tests;

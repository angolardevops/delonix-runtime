//! A read-through cache over the notes use cases.
//!
//! `Cache` is a bytes key/value store with per-entry expiry whose every method
//! is total — an adapter MUST NOT fail a caller on a backend error, it returns
//! `None` / does nothing and logs, so a cache outage is a slow path, never a
//! failed request. Two adapters implement it: `InMemoryCache` (the default when
//! no `REDIS_URL` is set) and `RedisCache`. `CachedNotes` wraps the use cases
//! from the OUTSIDE — exactly like `telemetry::TracedNotes` — so the `notes`
//! module never imports the cache.

use crate::notes::{CreateInput, Note, NotesApi, NotesError, Page};
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const KEY_PREFIX: &str = "note:v1:";

/// A bytes key/value store with per-entry expiry. Failures are swallowed.
#[async_trait]
pub trait Cache: Send + Sync {
    /// The stored value, or `None` on a miss OR any backend error.
    async fn get(&self, key: &str) -> Option<Vec<u8>>;
    /// Store `value` under `key` for `ttl`. A no-op on error or non-positive ttl.
    async fn set(&self, key: &str, value: &[u8], ttl: Duration);
    /// Whether the backend answers (for the readiness probe).
    async fn healthy(&self) -> bool;
    /// Release connections. Called once on shutdown.
    async fn close(&self);
}

/// In-process `Cache`. Entries live in a map with a monotonic-clock expiry; a
/// bounded size evicts the oldest first so a long run cannot grow without
/// limit. Lost on restart and not shared between replicas — for local runs,
/// tests, and as the graceful fallback when Redis is not configured.
pub struct InMemoryCache {
    inner: Mutex<InMemoryInner>,
    max: usize,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
}

struct InMemoryInner {
    by_key: HashMap<String, (f64, Vec<u8>)>, // expiry seconds -> value
    order: VecDeque<String>,                 // oldest first
}

impl InMemoryCache {
    pub fn new() -> Self {
        Self::with_capacity(4096)
    }

    pub fn with_capacity(max: usize) -> Self {
        let base = Instant::now();
        Self::with_clock(max, move || base.elapsed().as_secs_f64())
    }

    /// Constructs with an injected clock (seconds) — used by tests to pin TTL.
    pub fn with_clock(max: usize, clock: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        Self {
            inner: Mutex::new(InMemoryInner {
                by_key: HashMap::new(),
                order: VecDeque::new(),
            }),
            max,
            clock: Box::new(clock),
        }
    }
}

impl Default for InMemoryCache {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Cache for InMemoryCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        let now = (self.clock)();
        let mut inner = self.inner.lock().unwrap();
        match inner.by_key.get(key) {
            None => None,
            Some((expiry, _)) if now >= *expiry => {
                inner.by_key.remove(key);
                inner.order.retain(|k| k != key);
                None
            }
            Some((_, value)) => Some(value.clone()),
        }
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Duration) {
        if ttl.as_secs_f64() <= 0.0 {
            return;
        }
        let now = (self.clock)();
        let mut inner = self.inner.lock().unwrap();
        if inner.by_key.remove(key).is_some() {
            inner.order.retain(|k| k != key);
        } else if inner.by_key.len() >= self.max {
            if let Some(oldest) = inner.order.pop_front() {
                inner.by_key.remove(&oldest);
            }
        }
        inner
            .by_key
            .insert(key.to_string(), (now + ttl.as_secs_f64(), value.to_vec()));
        inner.order.push_back(key.to_string());
    }

    async fn healthy(&self) -> bool {
        true
    }

    async fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.by_key.clear();
        inner.order.clear();
    }
}

/// The commands `RedisCache` needs from a backend, named so the real TCP client
/// and a test fake both implement it. Every method may fail; `RedisCache`
/// swallows the failure.
#[async_trait]
pub trait RedisBackend: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    async fn set(&self, key: &str, value: &[u8], ttl_secs: u64) -> Result<(), String>;
    async fn ping(&self) -> Result<(), String>;
}

/// `Cache` backed by Redis. Every backend error is swallowed: a miss on read, a
/// no-op on write. A down Redis therefore turns the cache off (every lookup
/// falls through to the store) instead of failing requests. The URL — which
/// carries the password — is NEVER logged; only the fact of an outage is, once
/// per transition, so an outage does not flood the log.
pub struct RedisCache {
    backend: Box<dyn RedisBackend>,
    degraded: Mutex<bool>,
}

impl RedisCache {
    pub fn new(backend: Box<dyn RedisBackend>) -> Self {
        Self {
            backend,
            degraded: Mutex::new(false),
        }
    }

    /// Builds a client from a `redis://` URL. No connection is opened here — the
    /// first command connects, bounded by `timeout`.
    pub fn from_url(url: &str, timeout: Duration) -> Result<Self, String> {
        Ok(Self::new(Box::new(TcpRedis::parse(url, timeout)?)))
    }

    fn degrade(&self) {
        let mut d = self.degraded.lock().unwrap();
        if !*d {
            *d = true;
            tracing::warn!("cache backend unavailable, serving from the store");
        }
    }

    fn recover(&self) {
        let mut d = self.degraded.lock().unwrap();
        if *d {
            *d = false;
            tracing::info!("cache backend recovered");
        }
    }
}

#[async_trait]
impl Cache for RedisCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        match self.backend.get(key).await {
            Ok(v) => {
                self.recover();
                v
            }
            Err(_) => {
                self.degrade();
                None
            }
        }
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Duration) {
        let secs = ttl.as_secs().max(if ttl.as_secs_f64() > 0.0 { 1 } else { 0 });
        if secs == 0 {
            return;
        }
        match self.backend.set(key, value, secs).await {
            Ok(()) => self.recover(),
            Err(_) => self.degrade(),
        }
    }

    async fn healthy(&self) -> bool {
        self.backend.ping().await.is_ok()
    }

    async fn close(&self) {}
}

/// A minimal RESP (Redis serialization protocol) client: one short-lived TCP
/// connection per command — `GET`, `SET … EX`, `PING`, with optional `AUTH` /
/// `SELECT` from the URL. Enough for a read-through cache; connection reuse and
/// pipelining are the documented next step. Not exercised by `cargo test` (the
/// tests inject a fake `RedisBackend`); it runs against the compose `redis`.
pub struct TcpRedis {
    host: String,
    port: u16,
    password: Option<String>,
    db: Option<u32>,
    timeout: Duration,
}

impl TcpRedis {
    fn parse(url: &str, timeout: Duration) -> Result<Self, String> {
        let rest = url
            .strip_prefix("redis://")
            .or_else(|| url.strip_prefix("rediss://"))
            .ok_or_else(|| "REDIS_URL must start with redis://".to_string())?;
        let (auth, hostpart) = match rest.split_once('@') {
            Some((a, h)) => (Some(a), h),
            None => (None, rest),
        };
        let password = auth.and_then(|a| a.rsplit(':').next()).and_then(|p| {
            if p.is_empty() {
                None
            } else {
                Some(p.to_string())
            }
        });
        let (host_port, db) = match hostpart.split_once('/') {
            Some((hp, d)) => (hp, d.parse::<u32>().ok()),
            None => (hostpart, None),
        };
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(6379)),
            None => (host_port.to_string(), 6379),
        };
        Ok(Self {
            host,
            port,
            password,
            db,
            timeout,
        })
    }

    async fn command(&self, args: &[&[u8]]) -> Result<Reply, String> {
        let connect = TcpStream::connect((self.host.as_str(), self.port));
        let mut stream = tokio::time::timeout(self.timeout, connect)
            .await
            .map_err(|_| "connect timeout".to_string())?
            .map_err(|e| e.to_string())?;
        if let Some(pw) = &self.password {
            send(&mut stream, &[b"AUTH", pw.as_bytes()]).await?;
            read_reply(&mut stream, self.timeout).await?;
        }
        if let Some(db) = self.db {
            send(&mut stream, &[b"SELECT", db.to_string().as_bytes()]).await?;
            read_reply(&mut stream, self.timeout).await?;
        }
        send(&mut stream, args).await?;
        read_reply(&mut stream, self.timeout).await
    }
}

#[async_trait]
impl RedisBackend for TcpRedis {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        match self.command(&[b"GET", key.as_bytes()]).await? {
            Reply::Bulk(Some(b)) => Ok(Some(b)),
            Reply::Bulk(None) => Ok(None),
            _ => Err("unexpected GET reply".into()),
        }
    }

    async fn set(&self, key: &str, value: &[u8], ttl_secs: u64) -> Result<(), String> {
        let ttl = ttl_secs.to_string();
        match self
            .command(&[b"SET", key.as_bytes(), value, b"EX", ttl.as_bytes()])
            .await?
        {
            Reply::Simple => Ok(()),
            _ => Err("unexpected SET reply".into()),
        }
    }

    async fn ping(&self) -> Result<(), String> {
        match self.command(&[b"PING"]).await? {
            Reply::Simple => Ok(()),
            _ => Err("unexpected PING reply".into()),
        }
    }
}

enum Reply {
    /// A status (`+OK`) or integer (`:1`) reply — its payload is not needed.
    Simple,
    Bulk(Option<Vec<u8>>),
}

async fn send(stream: &mut TcpStream, args: &[&[u8]]) -> Result<(), String> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    stream.write_all(&buf).await.map_err(|e| e.to_string())
}

async fn read_reply(stream: &mut TcpStream, timeout: Duration) -> Result<Reply, String> {
    tokio::time::timeout(timeout, read_reply_inner(stream))
        .await
        .map_err(|_| "read timeout".to_string())?
}

async fn read_reply_inner(stream: &mut TcpStream) -> Result<Reply, String> {
    let line = read_line(stream).await?;
    let (kind, rest) = line.split_at(1);
    match kind {
        "+" => Ok(Reply::Simple),
        "-" => Err(rest.to_string()),
        ":" => Ok(Reply::Simple),
        "$" => {
            let n: i64 = rest.parse().map_err(|_| "bad bulk length".to_string())?;
            if n < 0 {
                return Ok(Reply::Bulk(None));
            }
            let mut body = vec![0u8; n as usize + 2]; // include trailing CRLF
            stream
                .read_exact(&mut body)
                .await
                .map_err(|e| e.to_string())?;
            body.truncate(n as usize);
            Ok(Reply::Bulk(Some(body)))
        }
        other => Err(format!("unexpected reply type {other:?}")),
    }
}

async fn read_line(stream: &mut TcpStream) -> Result<String, String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|e| e.to_string())?;
        if byte[0] == b'\r' {
            stream
                .read_exact(&mut byte)
                .await
                .map_err(|e| e.to_string())?; // consume '\n'
            break;
        }
        out.push(byte[0]);
    }
    String::from_utf8(out).map_err(|e| e.to_string())
}

/// Cache-aside over the notes use cases: `get(id)` reads through the cache. A
/// note is immutable once created, so a cached note is never stale — only the
/// TTL expires it. `create` pre-warms the cache so the first read is a hit;
/// `list` is never cached (it changes whenever a note is created). The cache is
/// a slow path, never a failure path — `Cache` swallows backend errors, and a
/// decode error here falls through to the store too.
pub struct CachedNotes {
    inner: std::sync::Arc<dyn NotesApi>,
    cache: std::sync::Arc<dyn Cache>,
    ttl: Duration,
}

impl CachedNotes {
    pub fn new(
        inner: std::sync::Arc<dyn NotesApi>,
        cache: std::sync::Arc<dyn Cache>,
        ttl: Duration,
    ) -> Self {
        Self { inner, cache, ttl }
    }
}

#[async_trait]
impl NotesApi for CachedNotes {
    async fn create(&self, input: CreateInput) -> Result<Note, NotesError> {
        let note = self.inner.create(input).await?;
        if let Ok(raw) = serde_json::to_vec(&note) {
            self.cache
                .set(&format!("{KEY_PREFIX}{}", note.id), &raw, self.ttl)
                .await;
        }
        Ok(note)
    }

    async fn get(&self, id: &str) -> Result<Note, NotesError> {
        let key = format!("{KEY_PREFIX}{id}");
        if let Some(raw) = self.cache.get(&key).await {
            match serde_json::from_slice::<Note>(&raw) {
                Ok(note) => return Ok(note),
                Err(_) => tracing::warn!(note_id = %id, "discarding an undecodable cache entry"),
            }
        }
        let note = self.inner.get(id).await?; // NotFound propagates
        if let Ok(raw) = serde_json::to_vec(&note) {
            self.cache.set(&key, &raw, self.ttl).await;
        }
        Ok(note)
    }

    async fn list(&self, offset: i64, limit: i64) -> Result<Page, NotesError> {
        self.inner.list(offset, limit).await
    }
}

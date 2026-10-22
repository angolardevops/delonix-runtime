//! Signs and verifies webhooks in the Standard Webhooks format
//! (<https://www.standardwebhooks.com>): headers `webhook-id`,
//! `webhook-timestamp` and `webhook-signature`, the signature being
//! `base64(HMAC-SHA256(key, "<id>.<timestamp>.<raw body>"))` prefixed with
//! `v1,`. Using a published format lets a sender or receiver written in another
//! language talk to this service without reading its code.
//!
//! It also holds the in-memory dedup set and the outbound dispatcher — the
//! `notes::Publisher` adapter that delivers a signed event per created note
//! without ever blocking the request that created it.

use crate::notes::{hex, Note, Publisher};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

type HmacSha256 = Hmac<Sha256>;

/// Header names (lower-case; HTTP headers are case-insensitive).
pub const HEADER_ID: &str = "webhook-id";
pub const HEADER_TIMESTAMP: &str = "webhook-timestamp";
pub const HEADER_SIGNATURE: &str = "webhook-signature";

/// How far a timestamp may be from now, either way. Outside it a delivery is
/// refused, which bounds how long a captured request can be replayed.
pub const TOLERANCE: Duration = Duration::from_secs(5 * 60);

/// The verification errors. Distinct so the handler answers 400 for a malformed
/// request and 401 for a bad signature, but none says which byte was wrong.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    MissingHeaders,
    BadTimestamp,
    BadSignature,
}

/// Decodes a `whsec_<base64>` secret. The key must be at least 24 bytes.
pub fn parse_secret(s: &str) -> Result<Vec<u8>, String> {
    let raw = s
        .strip_prefix("whsec_")
        .ok_or_else(|| r#"webhook secret must look like "whsec_<base64>""#.to_string())?;
    let key = base64::engine::general_purpose::STANDARD
        .decode(raw)
        .map_err(|e| format!("webhook secret is not valid base64: {e}"))?;
    if key.len() < 24 {
        return Err("webhook secret must decode to at least 24 bytes".into());
    }
    Ok(key)
}

/// Returns the `webhook-signature` header value.
pub fn sign(key: &[u8], id: &str, ts_unix: i64, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(format!("{id}.{ts_unix}.").as_bytes());
    mac.update(body);
    let tag = mac.finalize().into_bytes();
    format!("v1,{}", base64::engine::general_purpose::STANDARD.encode(tag))
}

/// Checks a delivery over the RAW body bytes (never a re-encoded body:
/// re-serializing JSON changes bytes and breaks every signature).
pub fn verify(
    key: &[u8],
    id: &str,
    timestamp: &str,
    signatures: &str,
    body: &[u8],
    now_unix: i64,
) -> Result<(), VerifyError> {
    if id.is_empty() || timestamp.is_empty() || signatures.is_empty() {
        return Err(VerifyError::MissingHeaders);
    }
    let secs: i64 = timestamp.parse().map_err(|_| VerifyError::BadTimestamp)?;
    let skew = (now_unix - secs).unsigned_abs();
    if skew > TOLERANCE.as_secs() {
        return Err(VerifyError::BadTimestamp);
    }
    let want = sign(key, id, secs, body);
    // Several space-separated signatures are allowed (key rotation). Compare in
    // constant time.
    for sig in signatures.split_whitespace() {
        if constant_time_eq(sig.as_bytes(), want.as_bytes()) {
            return Ok(());
        }
    }
    Err(VerifyError::BadSignature)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Current unix time in seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Remembers the ids of accepted deliveries for the tolerance window, so a
/// sender's retry of a message already handled is acknowledged without being
/// handled twice. Lives in memory: it does NOT survive a restart and is not
/// shared between replicas — enough against the retries of one sender to one
/// process; a deployment that must never process a message twice needs a
/// durable store (a unique key in the database) instead.
pub struct Dedup {
    seen: Mutex<HashMap<String, i64>>,
    max: usize,
}

impl Dedup {
    pub fn new(max: usize) -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            max,
        }
    }

    /// Records `id` and reports whether it was new.
    pub fn first_seen(&self, id: &str, now_unix: i64) -> bool {
        let mut seen = self.seen.lock().unwrap();
        seen.retain(|_, t| (now_unix - *t).unsigned_abs() <= 2 * TOLERANCE.as_secs());
        if seen.contains_key(id) {
            return false;
        }
        if seen.len() >= self.max {
            // Full: evict the oldest rather than refuse traffic.
            if let Some(oldest) = seen
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(k, _)| k.clone())
            {
                seen.remove(&oldest);
            }
        }
        seen.insert(id.to_string(), now_unix);
        true
    }

    /// Drops `id`, so a sender's retry is processed again. Called when
    /// processing failed after `first_seen` accepted the id.
    pub fn forget(&self, id: &str) {
        self.seen.lock().unwrap().remove(id);
    }
}

/// How the dispatcher bounds every dimension of delivery.
#[derive(Clone)]
pub struct DispatcherConfig {
    pub url: String,
    pub key: Vec<u8>,
    pub timeout: Duration,
    pub max_attempts: u32,
    pub base_backoff: Duration,
    pub queue_size: usize,
}

struct Job {
    body: Vec<u8>,
    note_id: String,
    trace_id: Option<String>,
}

/// Delivers events to one URL in the background: a bounded queue, one worker,
/// bounded attempts with exponential backoff and jitter.
///
/// Delivery is AT-LEAST-ONCE while the process lives and AT-MOST-ONCE across a
/// restart: the queue is memory, so what is queued when the process dies is
/// lost, and a full queue drops (and logs) instead of blocking the request that
/// created the note. The webhook-id is the note id, so a receiver that
/// deduplicates by it sees each note once however many attempts it took.
pub struct Dispatcher {
    tx: Mutex<Option<mpsc::Sender<Job>>>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Dispatcher {
    /// Starts the worker.
    pub fn start(cfg: DispatcherConfig) -> Self {
        let (tx, rx) = mpsc::channel::<Job>(cfg.queue_size.max(1));
        let client = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .expect("reqwest client");
        let handle = tokio::spawn(run(rx, cfg, client));
        Self {
            tx: Mutex::new(Some(tx)),
            handle: Mutex::new(Some(handle)),
        }
    }

    /// Stops taking events and waits for the queue to drain, up to `within`.
    pub async fn close(&self, within: Duration) {
        drop(self.tx.lock().unwrap().take()); // no more sends: the worker drains then exits
        let handle = self.handle.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = tokio::time::timeout(within, handle).await;
        }
    }
}

impl Publisher for Dispatcher {
    fn note_created(&self, note: &Note) {
        let event = serde_json::json!({ "type": "note.created", "data": note });
        let job = Job {
            body: serde_json::to_vec(&event).unwrap_or_default(),
            note_id: note.id.clone(),
            trace_id: crate::telemetry::current_trace_id(),
        };
        let guard = self.tx.lock().unwrap();
        match guard.as_ref() {
            None => tracing::warn!(note_id = %note.id, "webhook dispatcher closed, event dropped"),
            Some(tx) => {
                if tx.try_send(job).is_err() {
                    tracing::error!(note_id = %note.id, "webhook queue full, event dropped");
                }
            }
        }
    }
}

async fn run(mut rx: mpsc::Receiver<Job>, cfg: DispatcherConfig, client: reqwest::Client) {
    while let Some(job) = rx.recv().await {
        deliver(&job, &cfg, &client).await;
    }
}

async fn deliver(job: &Job, cfg: &DispatcherConfig, client: &reqwest::Client) {
    let mut last = String::new();
    for attempt in 1..=cfg.max_attempts {
        match attempt_once(job, cfg, client).await {
            Ok(()) => {
                tracing::info!(note_id = %job.note_id, attempts = attempt, "webhook delivered");
                return;
            }
            Err((retry, msg)) => {
                last = msg;
                if !retry || attempt == cfg.max_attempts {
                    break;
                }
                // Full jitter: a fraction of base * 2^(attempt-1).
                let ceiling = cfg.base_backoff.saturating_mul(1u32 << (attempt - 1));
                tokio::time::sleep(ceiling.mul_f64(jitter_fraction())).await;
            }
        }
    }
    tracing::error!(note_id = %job.note_id, error = %last, "webhook delivery failed");
}

/// Sends once. The `bool` reports whether trying again could help.
async fn attempt_once(
    job: &Job,
    cfg: &DispatcherConfig,
    client: &reqwest::Client,
) -> Result<(), (bool, String)> {
    let ts = now_unix();
    let mut req = client
        .post(&cfg.url)
        .header("content-type", "application/json")
        .header(HEADER_ID, &job.note_id)
        .header(HEADER_TIMESTAMP, ts.to_string())
        .header(HEADER_SIGNATURE, sign(&cfg.key, &job.note_id, ts, &job.body));
    if let Some(trace_id) = &job.trace_id {
        // W3C trace context, so the delivery shares the request's trace id.
        req = req.header(
            "traceparent",
            format!("00-{trace_id}-{}-01", random_span_id()),
        );
    }
    let resp = req
        .body(job.body.clone())
        .send()
        .await
        .map_err(|e| (true, e.to_string()))?; // network error or timeout: retry
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        Ok(())
    } else if status == 429 || status >= 500 {
        Err((true, format!("receiver answered {status}")))
    } else {
        // 4xx: the receiver refused this message; resending will not help.
        Err((false, format!("receiver answered {status}")))
    }
}

fn random_span_id() -> String {
    use std::io::Read;
    let mut b = [0u8; 8];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    hex(&b)
}

fn jitter_fraction() -> f64 {
    use std::io::Read;
    let mut b = [0u8; 8];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    (u64::from_le_bytes(b) as f64) / (u64::MAX as f64)
}

//! The process logger: JSON lines on stdout, the current request's `trace_id`
//! and `request_id` on every line emitted inside its span, and a secret-key
//! redaction helper so a careless log of a token cannot leak it.

use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::EnvFilter;

/// What a sensitive value becomes once redacted.
pub const REDACTED: &str = "[REDACTED]";

/// Key fragments that mark a value as a secret. A value whose key contains one
/// should be logged through `redact`.
const SENSITIVE: &[&str] = &[
    "authorization",
    "cookie",
    "password",
    "secret",
    "token",
    "signature",
    "api_key",
    "apikey",
];

/// Whether a log field key names a secret.
pub fn is_sensitive(key: &str) -> bool {
    let key = key.to_lowercase();
    SENSITIVE.iter().any(|s| key.contains(s))
}

/// The value to log for `key`: the original, or `[REDACTED]` when the key names
/// a secret.
pub fn redact<'a>(key: &str, value: &'a str) -> &'a str {
    if is_sensitive(key) {
        REDACTED
    } else {
        value
    }
}

/// Installs the global JSON subscriber. Idempotent across the process: a second
/// call (for example from a test) is a no-op. `level` is one of debug, info,
/// warn, error.
pub fn init(level: &str, service: &str, version: &str, env: &str) {
    let level = match level.to_lowercase().as_str() {
        "warning" => "warn".to_string(),
        other => other.to_string(),
    };
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(true)
        .with_span_list(false)
        .with_span_events(FmtSpan::NONE)
        .flatten_event(true)
        .try_init();
    tracing::info!(service, version, environment = env, "logging initialised");
}

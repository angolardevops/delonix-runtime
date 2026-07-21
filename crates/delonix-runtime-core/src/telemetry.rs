//! Observability foundation of the Delonix Runtime (C1): STRUCTURED logging via
//! `tracing` (slice 1) + **distributed spans via OpenTelemetry/OTLP** (slice 3,
//! this one), to replace the ad-hoc `eprintln!`/`println!` scattered across the engine
//! and give an SRE traces correlatable across thousands of nodes.
//!
//! A container runtime competing with containerd/CRI-O must emit logs and
//! traces that an SRE can filter and correlate; `println` in a terminal is not
//! enough. This layer connects the `fmt` subscriber (always) and, when requested, an
//! OTLP layer that exports the `tracing` spans to an OpenTelemetry collector.
//!
//! ## Honest nuance: batch exporter vs. synchronous CLI
//!
//! The OTLP exporter runs on a **batch model**: spans are buffered and
//! dispatched periodically. We deliberately chose the `BatchSpanProcessor`
//! of `opentelemetry_sdk`, which runs on a **dedicated thread** (`futures_executor::
//! block_on`) and a **blocking** HTTP client (`reqwest/blocking`) — so it **does not
//! need an ambient tokio runtime** at the moment of installation. This is what
//! allows connecting OTLP both in the `delonix-cri` server (which has tokio) and in the CLI
//! `delonix` (synchronous, without tokio) without faking anything.
//!
//! **The real limitation is in the synchronous CLI**: the batch only dispatches after
//! `scheduled_delay` (~5s) or on an explicit `force_flush`/`shutdown`. The CLI
//! `delonix` is short-lived and its `main`/entry-points do **not** call
//! `shutdown()` (they stay outside the territory of this layer), so a fast
//! invocation may terminate BEFORE the first flush and lose the spans. The path where
//! OTLP delivers reliably is the **`delonix-cri` server** (long-running
//! process — the batch timer flushes in a cycle while the process lives). The
//! opposite is not promised for the synchronous CLI.
//!
//! ## Transport
//!
//! OTLP/HTTP protobuf, `reqwest` client **without a TLS backend** (plain-text
//! endpoint, typical of a local collector). It is a supply-chain decision: it avoids
//! dragging `aws-lc-sys`/`openssl`/`native-tls` (C + OpenSSL license) into a
//! minimal public runtime. An `https://` endpoint is not supported by design.

use std::sync::atomic::{AtomicBool, Ordering};

static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Keeps the `TracerProvider` alive for the whole process. If it were dropped, the
/// `BatchSpanProcessor` would shut down the export thread and the spans would stop
/// going out. Stored here (besides the global provider) so the lifetime is
/// explicit and independent of the global `opentelemetry` state.
static OTLP_PROVIDER: std::sync::OnceLock<opentelemetry_sdk::trace::SdkTracerProvider> =
    std::sync::OnceLock::new();

/// Installs the global `tracing` subscriber from the environment. **Idempotent**
/// and concurrency-proof: calling it twice (or from two binaries in the same
/// test process) does not panic — the second call is a no-op.
///
/// Control via environment (no new flags on the CLI):
/// - **`DELONIX_LOG`** (or `RUST_LOG` as an alternative) — filter by level/target,
///   `env_filter` syntax (e.g.: `info`, `delonix_net=debug,warn`). Default `info`.
/// - **`DELONIX_LOG_FORMAT=json`** — emits structured JSON (one line per event,
///   for ingestion by Loki/ELK); any other value (or absent) → readable text.
/// - **`DELONIX_OTLP_ENDPOINT`** — when set (and non-empty), adds an OTLP
///   layer that exports the spans to that collector (e.g.: `http://localhost:4318`).
///   The `/v1/traces` path is appended if missing. Absent → only `fmt` (no OTLP,
///   no cost). See the CLI-sync nuance at the top of the module.
///
/// Must be called ONCE, at the startup of each binary (`delonix`, `delonix-cri`).
pub fn init() {
    // Avoids reinstalling (and the panic of an already-set `set_global_default`) when two
    // paths call `init` — `try_init` already fails silently, but the guard makes
    // the intent explicit and avoids the cost of rebuilding the filter.
    if INITIALISED.swap(true, Ordering::SeqCst) {
        return;
    }

    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter, Layer};

    // Priority: DELONIX_LOG > RUST_LOG > "info". `env` (not `env_or`) so it can
    // fall back to RUST_LOG when DELONIX_LOG is not set.
    let filter = EnvFilter::try_from_env("DELONIX_LOG")
        .or_else(|_| EnvFilter::try_from_default_env()) // RUST_LOG
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let json = std::env::var("DELONIX_LOG_FORMAT").ok().as_deref() == Some("json");

    // Logs go to stderr (stdout is reserved for the user's OUTPUT — `ls`,
    // `describe` tables — which must NOT be polluted by log lines). Boxed to
    // unify the two types (json vs text) into a single `dyn Layer`.
    let fmt_layer = if json {
        fmt::layer().with_writer(std::io::stderr).json().boxed()
    } else {
        fmt::layer().with_writer(std::io::stderr).boxed()
    };

    // OPTIONAL OTLP layer: `None` (without `DELONIX_OTLP_ENDPOINT`) is a no-op — the
    // `Option<Layer>` implements `Layer`. The `EnvFilter` applies to the whole
    // registry, so it governs both the `fmt` and the OTLP.
    let otlp_layer = build_otlp_layer();

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otlp_layer)
        .try_init();
}

/// Builds the OTLP layer if `DELONIX_OTLP_ENDPOINT` is set; otherwise `None`.
///
/// **Soft** failure: if the endpoint is invalid or the exporter fails to build,
/// it warns via `tracing` (which is already on its way to being installed — the warning goes out via
/// `fmt`) and returns `None`. It never `panic`s at startup because of telemetry.
fn build_otlp_layer<S>() -> Option<Box<dyn tracing_subscriber::Layer<S> + Send + Sync>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a> + Send + Sync,
{
    let endpoint = std::env::var("DELONIX_OTLP_ENDPOINT").ok()?;
    if endpoint.trim().is_empty() {
        return None;
    }
    let endpoint = normalise_traces_endpoint(endpoint.trim());

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
    use tracing_subscriber::Layer as _;

    let exporter = match SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(&endpoint)
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = %e,
                endpoint = %endpoint,
                "OTLP: exporter failed to build — continuing with logs only (fmt)"
            );
            return None;
        }
    };

    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name())
        .build();

    // `with_batch_exporter` uses the default BatchSpanProcessor: dedicated thread +
    // `block_on` (without an ambient tokio runtime). See the nuance at the top of the module.
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("delonix-runtime");

    // Keeps the provider alive: in the `opentelemetry` global (macros) and in our
    // `OnceLock`. Since the whole `init()` is idempotent, there is no real race here.
    opentelemetry::global::set_tracer_provider(provider.clone());
    let _ = OTLP_PROVIDER.set(provider);

    Some(tracing_opentelemetry::layer().with_tracer(tracer).boxed())
}

/// Service name for the spans' `Resource` attributes — distinguishes the CLI from
/// the CRI server in the collector. Derived from the executable name (`delonix` /
/// `delonix-cri`); `OTEL_SERVICE_NAME` takes priority (OpenTelemetry convention).
fn service_name() -> String {
    if let Ok(name) = std::env::var("OTEL_SERVICE_NAME") {
        if !name.trim().is_empty() {
            return name;
        }
    }
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(std::path::Path::file_name)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "delonix-runtime".to_string())
}

/// Normalizes the endpoint to the OTLP/HTTP spans path. Since we pass the value
/// programmatically (`with_endpoint`), the SDK uses it VERBATIM (does not append
/// `/v1/traces`); so we append it ourselves if missing, so the user can
/// give just the collector base (`http://host:4318`). Pure function → testable.
fn normalise_traces_endpoint(endpoint: &str) -> String {
    const SIGNAL_PATH: &str = "/v1/traces";
    if endpoint.contains(SIGNAL_PATH) {
        endpoint.to_string()
    } else {
        format!("{}{}", endpoint.trim_end_matches('/'), SIGNAL_PATH)
    }
}

#[cfg(test)]
mod tests {
    use super::normalise_traces_endpoint;

    #[test]
    fn acrescenta_signal_path_a_base() {
        assert_eq!(
            normalise_traces_endpoint("http://localhost:4318"),
            "http://localhost:4318/v1/traces"
        );
    }

    #[test]
    fn tolera_barra_final_na_base() {
        assert_eq!(
            normalise_traces_endpoint("http://localhost:4318/"),
            "http://localhost:4318/v1/traces"
        );
    }

    #[test]
    fn preserva_endpoint_ja_completo() {
        assert_eq!(
            normalise_traces_endpoint("http://collector:4318/v1/traces"),
            "http://collector:4318/v1/traces"
        );
    }
}

//! Distributed tracing, OpenTelemetry-compatible.
//!
//! Every request is given a W3C trace id (`telemetry::new_trace_id`) that the
//! HTTP middleware scopes for the duration of handling. Because the id lives in
//! a task-local, any work the request starts — a use case, an outbound webhook
//! delivery — can read it and carry it onward: the dispatcher sets a
//! `traceparent` header built from it, so the whole causal chain shares one
//! trace id, and so does every structured log line emitted while the scope is
//! active (`logging` records it on the request span).
//!
//! The ids are the W3C Trace Context shape (32-hex trace id, 16-hex span id),
//! so a collector or a downstream service reads them without translation.
//! Exporting spans to an OpenTelemetry collector (OTLP) is the documented
//! extension point: point the standard `OTEL_EXPORTER_OTLP_ENDPOINT` at one and
//! add the exporter in `main`. Nothing here blocks startup when no collector is
//! configured — tracing is always in-process, export is opt-in.

use crate::notes::{hex, CreateInput, Note, NotesApi, NotesError, Page};
use async_trait::async_trait;
use std::sync::Arc;
use tracing::Instrument;

tokio::task_local! {
    static TRACE_ID: String;
}

/// A fresh 16-byte (32-hex) trace id.
pub fn new_trace_id() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("/dev/urandom");
    hex(&b)
}

/// The trace id of the current request, if one is in scope.
pub fn current_trace_id() -> Option<String> {
    TRACE_ID.try_with(|t| t.clone()).ok()
}

/// Runs `fut` with `trace_id` in scope, so `current_trace_id` returns it and
/// the request span carries it.
pub async fn with_trace<F, T>(trace_id: String, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    TRACE_ID.scope(trace_id, fut).await
}

/// Wraps the use cases with one span each, so a trace shows the business step
/// between the HTTP span and the calls it makes — without the `notes` package
/// importing any telemetry.
pub struct TracedNotes {
    inner: Arc<dyn NotesApi>,
}

impl TracedNotes {
    pub fn new(inner: Arc<dyn NotesApi>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl NotesApi for TracedNotes {
    async fn create(&self, input: CreateInput) -> Result<Note, NotesError> {
        self.inner
            .create(input)
            .instrument(tracing::info_span!("notes.create"))
            .await
    }

    async fn get(&self, id: &str) -> Result<Note, NotesError> {
        self.inner
            .get(id)
            .instrument(tracing::info_span!("notes.get"))
            .await
    }

    async fn list(&self, offset: i64, limit: i64) -> Result<Page, NotesError> {
        self.inner
            .list(offset, limit)
            .instrument(tracing::info_span!("notes.list"))
            .await
    }
}

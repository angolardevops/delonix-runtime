//! The HTTP transport: routing, request decoding, error mapping and the
//! cross-cutting middleware (request id, trace scope, access log, body limit).
//! It translates HTTP to use-case calls and back; no business rule lives here.

use crate::health::{Readiness, State as ReadyState};
use crate::notes::{CreateInput, NotesApi, NotesError};
use crate::telemetry;
use crate::webhook::{self, Dedup, VerifyError};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// An inbound webhook delivery is bounded independently of the API limit.
const WEBHOOK_MAX_BODY: usize = 256 << 10;

tokio::task_local! {
    static REQUEST_ID: String;
}

/// The handler's dependencies, cloned into every request.
#[derive(Clone)]
pub struct AppState {
    pub notes: Arc<dyn NotesApi>,
    pub readiness: Readiness,
    pub webhook_key: Option<Arc<Vec<u8>>>,
    pub dedup: Arc<Dedup>,
    pub max_body_bytes: usize,
}

/// The route table. `api/openapi.yaml` must list exactly these; the test in
/// `tests/openapi.rs` fails on any drift. Patterns use the OpenAPI `{id}` form.
pub fn routes_table(webhook_enabled: bool) -> Vec<(&'static str, &'static str)> {
    let mut rs = vec![
        ("GET", "/api/v1/health/live"),
        ("GET", "/api/v1/health/ready"),
        ("POST", "/api/v1/notes"),
        ("GET", "/api/v1/notes"),
        ("GET", "/api/v1/notes/{id}"),
    ];
    if webhook_enabled {
        rs.push(("POST", "/api/v1/webhooks/inbound"));
    }
    rs
}

/// Builds the complete router with its middleware.
pub fn router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/api/v1/health/live", get(live))
        .route("/api/v1/health/ready", get(ready))
        .route("/api/v1/notes", post(create_note).get(list_notes))
        .route("/api/v1/notes/:id", get(get_note));
    if state.webhook_key.is_some() {
        router = router.route("/api/v1/webhooks/inbound", post(inbound_webhook));
    }
    router
        .fallback(not_found)
        .layer(axum::middleware::from_fn(request_context))
        .with_state(state)
}

// ---- middleware ------------------------------------------------------------

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Assigns a request id (keeping a short, plain client-sent one) and a trace
/// id, scopes both for the handler, logs one access line, and echoes the ids on
/// the response. The access log records the path, never the query string or
/// headers.
async fn request_context(req: Request<Body>, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| valid_request_id(v))
        .map(str::to_string)
        .unwrap_or_else(|| {
            use crate::notes::hex;
            use std::io::Read;
            let mut b = [0u8; 8];
            let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
            hex(&b)
        });
    let trace_id = telemetry::new_trace_id();

    let start = Instant::now();
    let span = tracing::info_span!(
        "http.request",
        trace_id = %trace_id,
        request_id = %request_id,
        method = %method,
        path = %path,
    );
    let rid = request_id.clone();
    let tid = trace_id.clone();
    let mut resp = telemetry::with_trace(tid, async move {
        REQUEST_ID
            .scope(rid, async move {
                use tracing::Instrument;
                next.run(req).instrument(span).await
            })
            .await
    })
    .await;

    let status = resp.status().as_u16();
    let is_health = path.starts_with("/api/v1/health/");
    let elapsed = start.elapsed().as_millis();
    if is_health {
        tracing::debug!(status, duration_ms = elapsed as u64, "request");
    } else {
        tracing::info!(status, duration_ms = elapsed as u64, "request");
    }
    let headers = resp.headers_mut();
    if let Ok(v) = request_id.parse() {
        headers.insert("x-request-id", v);
    }
    if let Ok(v) = trace_id.parse() {
        headers.insert("x-trace-id", v);
    }
    resp
}

// ---- error shape -----------------------------------------------------------

fn request_id() -> String {
    REQUEST_ID.try_with(|r| r.clone()).unwrap_or_default()
}

/// The one error shape every endpoint answers with.
fn error(
    status: StatusCode,
    code: &str,
    message: &str,
    fields: Option<HashMap<String, String>>,
) -> Response {
    let mut err = serde_json::Map::new();
    err.insert("code".into(), json!(code));
    err.insert("message".into(), json!(message));
    if let Some(fields) = fields {
        if !fields.is_empty() {
            err.insert("fields".into(), json!(fields));
        }
    }
    let rid = request_id();
    if !rid.is_empty() {
        err.insert("request_id".into(), json!(rid));
    }
    (status, Json(json!({ "error": err }))).into_response()
}

fn use_case_error(err: NotesError) -> Response {
    match err {
        NotesError::Validation(fields) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            "invalid input",
            Some(fields.into_iter().collect()),
        ),
        NotesError::NotFound => error(StatusCode::NOT_FOUND, "not_found", "note not found", None),
        NotesError::Backend(msg) => {
            tracing::error!(error = %msg, "use case failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error",
                None,
            )
        }
    }
}

/// Reads a bounded body, answering 413 when it is too large.
async fn read_body(body: Body, limit: usize) -> Result<Bytes, Response> {
    axum::body::to_bytes(body, limit).await.map_err(|_| {
        error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "request body too large",
            None,
        )
    })
}

// ---- handlers --------------------------------------------------------------

async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found", "no such route", None)
}

async fn live() -> Response {
    (StatusCode::OK, Json(json!({ "status": "alive" }))).into_response()
}

async fn ready(State(st): State<AppState>) -> Response {
    match st.readiness.state() {
        ReadyState::Ready => (StatusCode::OK, Json(json!({ "status": "ready" }))).into_response(),
        other => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": other.as_str() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateNoteRequest {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}

async fn create_note(State(st): State<AppState>, req: Request<Body>) -> Response {
    let bytes = match read_body(req.into_body(), st.max_body_bytes).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let parsed: CreateNoteRequest = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "malformed_json",
                "request body is not the expected JSON",
                None,
            )
        }
    };
    match st
        .notes
        .create(CreateInput {
            title: parsed.title,
            body: parsed.body,
        })
        .await
    {
        Ok(note) => {
            let location = format!("/api/v1/notes/{}", note.id);
            (
                StatusCode::CREATED,
                [(header::LOCATION, location)],
                Json(note),
            )
                .into_response()
        }
        Err(e) => use_case_error(e),
    }
}

async fn get_note(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.notes.get(&id).await {
        Ok(note) => (StatusCode::OK, Json(note)).into_response(),
        Err(e) => use_case_error(e),
    }
}

async fn list_notes(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let mut fields = HashMap::new();
    let offset = match q.get("offset") {
        None => 0,
        Some(v) => v.parse::<i64>().unwrap_or_else(|_| {
            fields.insert("offset".to_string(), "must be an integer".to_string());
            0
        }),
    };
    let limit = match q.get("limit") {
        None => crate::notes::DEFAULT_PAGE_SIZE,
        Some(v) => v.parse::<i64>().unwrap_or_else(|_| {
            fields.insert("limit".to_string(), "must be an integer".to_string());
            crate::notes::DEFAULT_PAGE_SIZE
        }),
    };
    if !fields.is_empty() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            "invalid input",
            Some(fields),
        );
    }
    match st.notes.list(offset, limit).await {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(e) => use_case_error(e),
    }
}

/// Accepts a signed delivery. `note.create` runs the same use case as
/// `POST /api/v1/notes`; `ping` only proves the signature. A delivery id
/// already accepted is acknowledged again without being processed again.
async fn inbound_webhook(State(st): State<AppState>, req: Request<Body>) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = match read_body(body, WEBHOOK_MAX_BODY).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let key = st.webhook_key.as_ref().expect("route only mounted with a key");
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let id = header(webhook::HEADER_ID);
    let now = webhook::now_unix();
    match webhook::verify(
        key,
        &id,
        &header(webhook::HEADER_TIMESTAMP),
        &header(webhook::HEADER_SIGNATURE),
        &bytes,
        now,
    ) {
        Err(VerifyError::MissingHeaders) => {
            return error(
                StatusCode::BAD_REQUEST,
                "missing_signature",
                "webhook headers are missing",
                None,
            )
        }
        Err(_) => {
            tracing::warn!("webhook rejected");
            return error(
                StatusCode::UNAUTHORIZED,
                "invalid_signature",
                "webhook signature is not valid",
                None,
            );
        }
        Ok(()) => {}
    }
    if !st.dedup.first_seen(&id, now) {
        return StatusCode::NO_CONTENT.into_response();
    }
    let event: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "malformed_json",
                "webhook body is not JSON",
                None,
            )
        }
    };
    match event.get("type").and_then(|t| t.as_str()) {
        Some("ping") => {}
        Some("note.create") => {
            let data = event.get("data").cloned().unwrap_or(serde_json::Value::Null);
            let parsed: CreateNoteRequest = match serde_json::from_value(data) {
                Ok(v) => v,
                Err(_) => {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "malformed_json",
                        "webhook data is not a note",
                        None,
                    )
                }
            };
            if let Err(e) = st
                .notes
                .create(CreateInput {
                    title: parsed.title,
                    body: parsed.body,
                })
                .await
            {
                // Not processed: a retry of this id must be processed, not
                // acknowledged as a duplicate.
                st.dedup.forget(&id);
                return use_case_error(e);
            }
        }
        _ => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_event",
                "unknown webhook event type",
                None,
            )
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

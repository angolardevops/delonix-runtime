//! One request that creates a note leaves one trace across the incoming
//! request and the outbound webhook it triggers: the delivery carries a
//! `traceparent` built from the request's trace id, which is the same id the
//! response echoes in `x-trace-id`. That is what makes a failure findable from
//! either side.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use common::{spawn, state, test_key};
use __MODULE__::httpapi::router;
use __MODULE__::webhook::{Dispatcher, DispatcherConfig};
use tokio::sync::mpsc;

#[tokio::test]
async fn one_trace_across_request_and_outbound_call() {
    // A target server that records the traceparent of each delivery.
    let (tx, mut rx) = mpsc::channel::<String>(4);
    let target_app = Router::new().route(
        "/hook",
        post(move |headers: axum::http::HeaderMap| {
            let tx = tx.clone();
            async move {
                let tp = headers
                    .get("traceparent")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let _ = tx.send(tp).await;
                StatusCode::NO_CONTENT
            }
        }),
    );
    let target = spawn(target_app).await;

    // The app, with the outbound dispatcher pointed at the target.
    let dispatcher = Arc::new(Dispatcher::start(DispatcherConfig {
        url: format!("{target}/hook"),
        key: test_key(),
        timeout: Duration::from_secs(2),
        max_attempts: 3,
        base_backoff: Duration::from_millis(1),
        queue_size: 8,
    }));
    let base = spawn(router(state(false, Some(dispatcher)))).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v1/notes"))
        .json(&serde_json::json!({ "title": "traced" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let trace_id = resp
        .headers()
        .get("x-trace-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(!trace_id.is_empty());

    let traceparent = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the outbound webhook never arrived")
        .unwrap();
    assert!(
        traceparent.contains(&trace_id),
        "outbound traceparent {traceparent:?} does not carry trace {trace_id}"
    );
}

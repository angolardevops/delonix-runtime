//! Shared test helpers: a real server on an ephemeral port and the in-memory
//! wiring the HTTP tests drive.

#![allow(dead_code)]

use std::sync::Arc;

use __MODULE__::health::Readiness;
use __MODULE__::httpapi::AppState;
use __MODULE__::memstore::InMemoryStore;
use __MODULE__::notes::{NoteService, NotesApi, Publisher};
use __MODULE__::webhook::Dedup;

/// The 32-byte key the webhook tests sign with.
pub fn test_key() -> Vec<u8> {
    vec![7u8; 32]
}

/// A ready service with an in-memory store. `webhook` mounts the inbound
/// endpoint with `test_key`. `publisher` is wired into the use cases.
pub fn state(webhook: bool, publisher: Option<Arc<dyn Publisher>>) -> AppState {
    let store = Arc::new(InMemoryStore::new());
    let notes: Arc<dyn NotesApi> = Arc::new(NoteService::new(store, publisher));
    let readiness = Readiness::new();
    readiness.set_ready();
    AppState {
        notes,
        readiness,
        webhook_key: webhook.then(|| Arc::new(test_key())),
        dedup: Arc::new(Dedup::new(16)),
        max_body_bytes: 1024,
    }
}

/// Serves `app` on an ephemeral port and returns its base URL.
pub async fn spawn(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

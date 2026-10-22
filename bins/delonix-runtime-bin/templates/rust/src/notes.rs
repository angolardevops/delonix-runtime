//! The example capability: create, read and list short notes.
//!
//! This module holds the rules and the use cases, and names what it needs from
//! the outside world as two small traits (`Store`, `Publisher`). It imports
//! nothing about HTTP, telemetry, caching or a storage technology — the
//! architecture test fails the build if it ever does.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Limits of a note, enforced by the use case (not by the transport).
pub const MAX_TITLE_LEN: usize = 200;
pub const MAX_BODY_LEN: usize = 10_000;
pub const MAX_PAGE_SIZE: i64 = 100;
pub const DEFAULT_PAGE_SIZE: i64 = 20;

/// One stored note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

/// One page of a listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub items: Vec<Note>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

/// What a caller may set on a new note.
#[derive(Clone, Debug, Default)]
pub struct CreateInput {
    pub title: String,
    pub body: String,
}

/// The ways a use case can fail. `Validation` lists every field that failed,
/// so a client fixes a request in one round trip; `NotFound` and `Backend` map
/// to a 404 and a 500 in the transport.
#[derive(Debug)]
pub enum NotesError {
    Validation(BTreeMap<String, String>),
    NotFound,
    Backend(String),
}

impl std::fmt::Display for NotesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotesError::Validation(fields) => {
                let parts: Vec<String> =
                    fields.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                write!(f, "invalid note: {}", parts.join("; "))
            }
            NotesError::NotFound => write!(f, "note not found"),
            NotesError::Backend(msg) => write!(f, "backend error: {msg}"),
        }
    }
}
impl std::error::Error for NotesError {}

/// The persistence port. The in-memory adapter lives in `memstore`; a database
/// adapter implements the same three methods.
#[async_trait]
pub trait Store: Send + Sync {
    async fn save(&self, note: Note) -> Result<(), NotesError>;
    async fn get(&self, id: &str) -> Result<Note, NotesError>;
    /// Returns one page, newest first, and the total count.
    async fn list(&self, offset: i64, limit: i64) -> Result<(Vec<Note>, i64), NotesError>;
}

/// Tells the outside world that a note exists. It must not block the caller on
/// the network: the outbound webhook adapter and the RabbitMQ publisher queue.
pub trait Publisher: Send + Sync {
    fn note_created(&self, note: &Note);
}

/// What the transport (and the cache/tracing decorators) reach the use cases
/// through. `NoteService`, `cache::CachedNotes` and `telemetry::TracedNotes`
/// all implement it, so they compose in any order.
#[async_trait]
pub trait NotesApi: Send + Sync {
    async fn create(&self, input: CreateInput) -> Result<Note, NotesError>;
    async fn get(&self, id: &str) -> Result<Note, NotesError>;
    async fn list(&self, offset: i64, limit: i64) -> Result<Page, NotesError>;
}

/// The use cases.
pub struct NoteService {
    store: Arc<dyn Store>,
    publisher: Option<Arc<dyn Publisher>>,
    new_id: Box<dyn Fn() -> String + Send + Sync>,
    now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl NoteService {
    /// Wires the use cases to their ports. A `None` publisher publishes nothing.
    pub fn new(store: Arc<dyn Store>, publisher: Option<Arc<dyn Publisher>>) -> Self {
        Self {
            store,
            publisher,
            new_id: Box::new(random_id),
            now: Box::new(Utc::now),
        }
    }

    /// Overrides the clock and id source — used by tests.
    pub fn with_clock(
        mut self,
        new_id: impl Fn() -> String + Send + Sync + 'static,
        now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static,
    ) -> Self {
        self.new_id = Box::new(new_id);
        self.now = Box::new(now);
        self
    }
}

#[async_trait]
impl NotesApi for NoteService {
    async fn create(&self, input: CreateInput) -> Result<Note, NotesError> {
        let title = input.title.trim().to_string();
        let mut fields = BTreeMap::new();
        if title.is_empty() {
            fields.insert("title".into(), "is required".into());
        } else if title.chars().count() > MAX_TITLE_LEN {
            fields.insert(
                "title".into(),
                format!("must be at most {MAX_TITLE_LEN} characters"),
            );
        }
        if input.body.chars().count() > MAX_BODY_LEN {
            fields.insert(
                "body".into(),
                format!("must be at most {MAX_BODY_LEN} characters"),
            );
        }
        if !fields.is_empty() {
            return Err(NotesError::Validation(fields));
        }
        let note = Note {
            id: (self.new_id)(),
            title,
            body: input.body,
            created_at: (self.now)(),
        };
        self.store.save(note.clone()).await?;
        if let Some(pub_) = &self.publisher {
            pub_.note_created(&note);
        }
        Ok(note)
    }

    async fn get(&self, id: &str) -> Result<Note, NotesError> {
        self.store.get(id).await
    }

    async fn list(&self, offset: i64, limit: i64) -> Result<Page, NotesError> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            let mut fields = BTreeMap::new();
            fields.insert("limit".into(), format!("must be between 1 and {MAX_PAGE_SIZE}"));
            return Err(NotesError::Validation(fields));
        }
        if offset < 0 {
            let mut fields = BTreeMap::new();
            fields.insert("offset".into(), "must not be negative".into());
            return Err(NotesError::Validation(fields));
        }
        let (items, total) = self.store.list(offset, limit).await?;
        Ok(Page {
            items,
            total,
            offset,
            limit,
        })
    }
}

/// 16 random bytes as hex. From `/dev/urandom`; a non-random id is worse than a
/// panic at startup, which is why a read failure is fatal.
pub fn random_id() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("/dev/urandom");
    hex(&bytes)
}

/// Lower-case hex, no dependency.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

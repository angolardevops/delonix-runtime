//! The use cases over the in-memory store: create trims and publishes, every
//! invalid field is reported and nothing is published, list pages and bounds.

use std::sync::{Arc, Mutex};

use __MODULE__::memstore::InMemoryStore;
use __MODULE__::notes::{CreateInput, Note, NoteService, NotesApi, NotesError, Publisher, MAX_BODY_LEN};

#[derive(Default)]
struct Recorder {
    got: Mutex<Vec<Note>>,
}
impl Publisher for Recorder {
    fn note_created(&self, note: &Note) {
        self.got.lock().unwrap().push(note.clone());
    }
}

#[tokio::test]
async fn create_stores_publishes_and_trims() {
    let rec = Arc::new(Recorder::default());
    let svc = NoteService::new(Arc::new(InMemoryStore::new()), Some(rec.clone()));
    let note = svc
        .create(CreateInput { title: "  hello  ".into(), body: "world".into() })
        .await
        .unwrap();
    assert_eq!(note.title, "hello");
    assert!(!note.id.is_empty());

    let got = svc.get(&note.id).await.unwrap();
    assert_eq!(got, note);
    let published = rec.got.lock().unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].id, note.id);
}

#[tokio::test]
async fn create_rejects_every_invalid_field_and_publishes_nothing() {
    let rec = Arc::new(Recorder::default());
    let svc = NoteService::new(Arc::new(InMemoryStore::new()), Some(rec.clone()));
    let err = svc
        .create(CreateInput { title: " ".into(), body: "x".repeat(MAX_BODY_LEN + 1) })
        .await
        .unwrap_err();
    match err {
        NotesError::Validation(fields) => {
            assert!(fields.contains_key("title"), "{fields:?}");
            assert!(fields.contains_key("body"), "{fields:?}");
        }
        other => panic!("want a validation error, got {other}"),
    }
    assert!(rec.got.lock().unwrap().is_empty());
}

#[tokio::test]
async fn list_pages_and_bounds() {
    let svc = NoteService::new(Arc::new(InMemoryStore::new()), None);
    for title in ["a", "b", "c"] {
        svc.create(CreateInput { title: title.into(), body: String::new() })
            .await
            .unwrap();
    }
    let page = svc.list(1, 1).await.unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.items.len(), 1);
    assert!(svc.list(0, 1000).await.is_err(), "an oversized page was accepted");
}

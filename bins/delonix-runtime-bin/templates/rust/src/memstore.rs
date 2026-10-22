//! The in-memory adapter of `notes::Store`. Enough for the example and for
//! tests; it keeps nothing across restarts. Swap it for a database adapter by
//! implementing the same trait — the use cases do not change.

use crate::notes::{Note, NotesError, Store};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

/// Keeps notes in a map guarded by a mutex.
#[derive(Default)]
pub struct InMemoryStore {
    notes: Mutex<HashMap<String, Note>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for InMemoryStore {
    async fn save(&self, note: Note) -> Result<(), NotesError> {
        self.notes.lock().unwrap().insert(note.id.clone(), note);
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Note, NotesError> {
        self.notes
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or(NotesError::NotFound)
    }

    async fn list(&self, offset: i64, limit: i64) -> Result<(Vec<Note>, i64), NotesError> {
        let guard = self.notes.lock().unwrap();
        let mut all: Vec<Note> = guard.values().cloned().collect();
        drop(guard);
        // Newest first; ties broken by id so pages are stable.
        all.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let total = all.len() as i64;
        let offset = offset.max(0) as usize;
        if offset >= all.len() {
            return Ok((Vec::new(), total));
        }
        let end = (offset + limit.max(0) as usize).min(all.len());
        Ok((all[offset..end].to_vec(), total))
    }
}

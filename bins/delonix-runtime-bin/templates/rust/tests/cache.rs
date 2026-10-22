//! The cache: the in-process backend, the cache-aside decorator, and that a
//! backend failure is a slow path and never a failed request.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use __MODULE__::cache::{Cache, CachedNotes, InMemoryCache, RedisBackend, RedisCache};
use __MODULE__::memstore::InMemoryStore;
use __MODULE__::notes::{CreateInput, Note, NoteService, NotesApi, NotesError, Page};

#[tokio::test]
async fn inmemory_roundtrip_and_miss() {
    let cache = InMemoryCache::new();
    assert!(cache.get("absent").await.is_none());
    cache.set("k", b"v", Duration::from_secs(10)).await;
    assert_eq!(cache.get("k").await.as_deref(), Some(&b"v"[..]));
}

#[tokio::test]
async fn inmemory_entry_expires_after_ttl() {
    let clock = Arc::new(Mutex::new(0.0f64));
    let c2 = clock.clone();
    let cache = InMemoryCache::with_clock(4096, move || *c2.lock().unwrap());
    cache.set("k", b"v", Duration::from_secs(10)).await;
    *clock.lock().unwrap() = 9.9;
    assert_eq!(cache.get("k").await.as_deref(), Some(&b"v"[..]));
    *clock.lock().unwrap() = 10.0;
    assert!(cache.get("k").await.is_none());
}

#[tokio::test]
async fn inmemory_evicts_the_oldest_when_full() {
    let cache = InMemoryCache::with_capacity(2);
    cache.set("a", b"1", Duration::from_secs(100)).await;
    cache.set("b", b"2", Duration::from_secs(100)).await;
    cache.set("c", b"3", Duration::from_secs(100)).await; // evicts "a"
    assert!(cache.get("a").await.is_none());
    assert_eq!(cache.get("b").await.as_deref(), Some(&b"2"[..]));
    assert_eq!(cache.get("c").await.as_deref(), Some(&b"3"[..]));
}

#[tokio::test]
async fn inmemory_rejects_non_positive_ttl() {
    let cache = InMemoryCache::new();
    cache.set("k", b"v", Duration::ZERO).await;
    assert!(cache.get("k").await.is_none());
}

/// A use-case object that counts how often the store is actually read.
struct CountingNotes {
    inner: NoteService,
    gets: AtomicUsize,
}
impl CountingNotes {
    fn new() -> Self {
        Self {
            inner: NoteService::new(Arc::new(InMemoryStore::new()), None),
            gets: AtomicUsize::new(0),
        }
    }
    fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl NotesApi for CountingNotes {
    async fn create(&self, input: CreateInput) -> Result<Note, NotesError> {
        self.inner.create(input).await
    }
    async fn get(&self, id: &str) -> Result<Note, NotesError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(id).await
    }
    async fn list(&self, offset: i64, limit: i64) -> Result<Page, NotesError> {
        self.inner.list(offset, limit).await
    }
}

#[tokio::test]
async fn create_prewarms_so_the_first_get_is_a_hit() {
    let inner = Arc::new(CountingNotes::new());
    let notes = CachedNotes::new(inner.clone(), Arc::new(InMemoryCache::new()), Duration::from_secs(100));
    let note = notes.create(CreateInput { title: "hello".into(), body: String::new() }).await.unwrap();
    let got = notes.get(&note.id).await.unwrap();
    assert_eq!(got, note);
    assert_eq!(inner.gets(), 0, "served entirely from the cache");
}

#[tokio::test]
async fn second_get_is_served_from_the_cache() {
    let inner = Arc::new(CountingNotes::new());
    let notes = CachedNotes::new(inner.clone(), Arc::new(InMemoryCache::new()), Duration::from_secs(100));
    // Created on the inner only: the cache is cold.
    let created = inner.create(CreateInput { title: "x".into(), body: String::new() }).await.unwrap();
    assert_eq!(notes.get(&created.id).await.unwrap(), created);
    assert_eq!(notes.get(&created.id).await.unwrap(), created);
    assert_eq!(inner.gets(), 1, "the store was read once; the second get was a hit");
}

#[tokio::test]
async fn unknown_note_still_raises_through_the_cache() {
    let notes = CachedNotes::new(
        Arc::new(CountingNotes::new()),
        Arc::new(InMemoryCache::new()),
        Duration::from_secs(100),
    );
    assert!(matches!(notes.get("nope").await, Err(NotesError::NotFound)));
}

#[tokio::test]
async fn a_corrupt_cache_entry_is_ignored() {
    let inner = Arc::new(CountingNotes::new());
    let cache = Arc::new(InMemoryCache::new());
    let created = inner.create(CreateInput { title: "x".into(), body: String::new() }).await.unwrap();
    cache.set(&format!("note:v1:{}", created.id), b"not json", Duration::from_secs(100)).await;
    let notes = CachedNotes::new(inner.clone(), cache, Duration::from_secs(100));
    assert_eq!(notes.get(&created.id).await.unwrap(), created, "fell through to the store");
    assert_eq!(inner.gets(), 1);
}

/// Models the port contract under a backend outage: never fails the caller.
struct BrokenCache;
#[async_trait]
impl Cache for BrokenCache {
    async fn get(&self, _key: &str) -> Option<Vec<u8>> {
        None
    }
    async fn set(&self, _key: &str, _value: &[u8], _ttl: Duration) {}
    async fn healthy(&self) -> bool {
        false
    }
    async fn close(&self) {}
}

#[tokio::test]
async fn a_dead_cache_backend_still_serves_from_the_store() {
    let inner = Arc::new(CountingNotes::new());
    let created = inner.create(CreateInput { title: "x".into(), body: String::new() }).await.unwrap();
    let notes = CachedNotes::new(inner.clone(), Arc::new(BrokenCache), Duration::from_secs(100));
    assert_eq!(notes.get(&created.id).await.unwrap(), created);
    assert_eq!(notes.get(&created.id).await.unwrap(), created);
    assert_eq!(inner.gets(), 2, "every read falls through; nothing is ever cached");
}

/// A redis backend whose every call fails, to prove RedisCache swallows it.
struct RaisingRedis;
#[async_trait]
impl RedisBackend for RaisingRedis {
    async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>, String> {
        Err("down".into())
    }
    async fn set(&self, _key: &str, _value: &[u8], _ttl_secs: u64) -> Result<(), String> {
        Err("down".into())
    }
    async fn ping(&self) -> Result<(), String> {
        Err("down".into())
    }
}

#[tokio::test]
async fn redis_cache_swallows_backend_errors() {
    let cache = RedisCache::new(Box::new(RaisingRedis));
    assert!(cache.get("k").await.is_none());
    cache.set("k", b"v", Duration::from_secs(10)).await; // must not panic
    assert!(!cache.healthy().await);
    cache.close().await; // must not panic
}

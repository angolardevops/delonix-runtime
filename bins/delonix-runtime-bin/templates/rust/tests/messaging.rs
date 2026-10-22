//! Messaging without a broker: the wire format, the fan-out publisher, the
//! bounded publish queue, and the consumer's ack / retry / dead-letter
//! decision. The AMQP glue is behind the `amqp` feature and is exercised by the
//! CI integration run against the compose `rabbitmq` service, not here.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use __MODULE__::messaging::{
    events::{self, ROUTING_KEY},
    CompositePublisher, Handler, Incoming, PublisherConfig, RabbitConsumer, RabbitPublisher,
    SeqOpener, Sink,
};
use __MODULE__::notes::{Note, Publisher};

fn a_note(id: &str) -> Note {
    Note {
        id: id.into(),
        title: "t".into(),
        body: "b".into(),
        created_at: chrono::Utc::now(),
    }
}

// ---- events ----

#[test]
fn event_roundtrips_and_names_the_note() {
    let event = events::note_created(&a_note("n1"));
    assert_eq!(event["type"], "note.created");
    assert_eq!(event["data"]["id"], "n1");
    assert_eq!(events::decode(&events::encode(&event)).unwrap(), event);
}

#[test]
fn decode_rejects_a_poison_body() {
    for body in [&b"not json"[..], b"123", br#"{"no":"type"}"#, br#"{"type":5}"#] {
        assert!(events::decode(body).is_err(), "{body:?}");
    }
}

// ---- CompositePublisher ----

#[derive(Default)]
struct Recording {
    seen: Mutex<Vec<String>>,
}
impl Publisher for Recording {
    fn note_created(&self, note: &Note) {
        self.seen.lock().unwrap().push(note.id.clone());
    }
}

#[test]
fn composite_forwards_to_every_publisher() {
    let a = Arc::new(Recording::default());
    let b = Arc::new(Recording::default());
    CompositePublisher::new(vec![a.clone(), b.clone()]).note_created(&a_note("x"));
    assert_eq!(*a.seen.lock().unwrap(), ["x"]);
    assert_eq!(*b.seen.lock().unwrap(), ["x"]);
}

// ---- RabbitPublisher (fake sink) ----

type Published = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

#[derive(Clone, Default)]
struct FakeSink {
    published: Published,
    fail_times: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}
impl FakeSink {
    fn failing(times: usize) -> Self {
        let s = Self::default();
        s.fail_times.store(times, Ordering::SeqCst);
        s
    }
}
#[async_trait]
impl Sink for FakeSink {
    async fn publish(&self, routing_key: &str, body: &[u8]) -> Result<(), String> {
        if self.fail_times.load(Ordering::SeqCst) > 0 {
            self.fail_times.fetch_sub(1, Ordering::SeqCst);
            return Err("broker down".into());
        }
        self.published.lock().unwrap().push((routing_key.to_string(), body.to_vec()));
        Ok(())
    }
    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

fn fast() -> PublisherConfig {
    PublisherConfig {
        queue_size: 256,
        reopen_backoff: Duration::ZERO,
        reopen_backoff_max: Duration::ZERO,
    }
}

#[tokio::test]
async fn publisher_delivers_queued_notes() {
    let sink = FakeSink::default();
    let pub_ = RabbitPublisher::new("amqp://x", Arc::new(SeqOpener::new(vec![Box::new(sink.clone())])), fast());
    pub_.start();
    pub_.note_created(&a_note("n1"));
    pub_.note_created(&a_note("n2"));
    assert!(pub_.close(Duration::from_secs(1)).await);
    let published = sink.published.lock().unwrap();
    assert_eq!(published.iter().map(|(rk, _)| rk.clone()).collect::<Vec<_>>(), [ROUTING_KEY, ROUTING_KEY]);
    assert_eq!(events::decode(&published[0].1).unwrap()["data"]["id"], "n1");
    assert!(sink.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn publisher_reopens_the_sink_after_a_failure() {
    let broken = FakeSink::failing(1);
    let good = FakeSink::default();
    let opener = SeqOpener::new(vec![Box::new(broken.clone()), Box::new(good.clone())]);
    let pub_ = RabbitPublisher::new("amqp://x", Arc::new(opener), fast());
    pub_.start();
    pub_.note_created(&a_note("n1"));
    assert!(pub_.close(Duration::from_secs(1)).await);
    assert_eq!(good.published.lock().unwrap().len(), 1, "reopened and redelivered on the new sink");
}

#[tokio::test]
async fn publisher_drops_when_the_queue_is_full() {
    let sink = FakeSink::default();
    let mut cfg = fast();
    cfg.queue_size = 1;
    let pub_ = RabbitPublisher::new("amqp://x", Arc::new(SeqOpener::new(vec![Box::new(sink.clone())])), cfg);
    // No worker yet: the queue fills and extra notes are dropped, never block.
    pub_.note_created(&a_note("n1"));
    pub_.note_created(&a_note("n2"));
    pub_.note_created(&a_note("n3"));
    pub_.start();
    pub_.close(Duration::from_secs(1)).await;
    assert_eq!(sink.published.lock().unwrap().len(), 1);
}

// ---- RabbitConsumer::handle (fake delivery) ----

struct FakeIncoming {
    body: Vec<u8>,
    retry: u32,
    acked: bool,
    dead_lettered: bool,
    retried_with: Option<Duration>,
}
impl FakeIncoming {
    fn new(body: Vec<u8>, retry: u32) -> Self {
        Self { body, retry, acked: false, dead_lettered: false, retried_with: None }
    }
}
#[async_trait]
impl Incoming for FakeIncoming {
    fn body(&self) -> &[u8] {
        &self.body
    }
    fn retry_count(&self) -> u32 {
        self.retry
    }
    async fn ack(&mut self) {
        self.acked = true;
    }
    async fn dead_letter(&mut self) {
        self.dead_lettered = true;
    }
    async fn retry(&mut self, delay: Duration) {
        self.retried_with = Some(delay);
    }
}

struct OkHandler;
#[async_trait]
impl Handler for OkHandler {
    async fn handle(&self, _event: serde_json::Value) -> Result<(), String> {
        Ok(())
    }
}
struct BoomHandler;
#[async_trait]
impl Handler for BoomHandler {
    async fn handle(&self, _event: serde_json::Value) -> Result<(), String> {
        Err("transient".into())
    }
}

fn consumer(handler: Arc<dyn Handler>, max_retries: u32) -> RabbitConsumer {
    RabbitConsumer::new(handler, max_retries, Duration::from_millis(250))
}

fn good_body() -> Vec<u8> {
    events::encode(&events::note_created(&a_note("n1")))
}

#[tokio::test]
async fn handle_acks_a_processed_message() {
    let mut msg = FakeIncoming::new(good_body(), 0);
    consumer(Arc::new(OkHandler), 3).handle(&mut msg).await;
    assert!(msg.acked);
    assert!(!msg.dead_lettered);
}

#[tokio::test]
async fn handle_dead_letters_a_poison_message_at_once() {
    let mut msg = FakeIncoming::new(b"not an event".to_vec(), 0);
    consumer(Arc::new(OkHandler), 3).handle(&mut msg).await;
    assert!(msg.dead_lettered);
    assert!(!msg.acked);
    assert!(msg.retried_with.is_none());
}

#[tokio::test]
async fn handle_retries_a_transient_failure() {
    let mut msg = FakeIncoming::new(good_body(), 0);
    consumer(Arc::new(BoomHandler), 3).handle(&mut msg).await;
    assert_eq!(msg.retried_with, Some(Duration::from_millis(250)));
    assert!(!msg.dead_lettered);
    assert!(!msg.acked);
}

#[tokio::test]
async fn handle_dead_letters_when_retries_are_exhausted() {
    let mut msg = FakeIncoming::new(good_body(), 3);
    consumer(Arc::new(BoomHandler), 3).handle(&mut msg).await;
    assert!(msg.dead_lettered);
    assert!(!msg.acked);
    assert!(msg.retried_with.is_none());
}

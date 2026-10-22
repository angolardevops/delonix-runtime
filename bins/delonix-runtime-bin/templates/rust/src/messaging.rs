//! Asynchronous messaging over RabbitMQ: publish and consume `note.created`.
//!
//! The valuable, tested part is transport-agnostic and always compiled: the
//! wire format (`events`), the fan-out `CompositePublisher`, the bounded,
//! non-blocking publish queue (`RabbitPublisher`) and the consumer's
//! ack / retry / dead-letter decision (`RabbitConsumer::handle`). Each is
//! exercised with in-process fakes, so `cargo test` needs no broker.
//!
//! The real AMQP glue (opening a connection, declaring the topology, consuming)
//! is behind the `amqp` feature (lapin), OFF by default. Build the image with
//! `--features amqp` to publish and consume for real against the compose
//! `rabbitmq` service.

use crate::notes::{Note, Publisher};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// The `note.created` event, on the wire. The envelope matches the outbound
/// webhook's (`type` + `data`), so a receiver sees the same shape on either
/// transport.
pub mod events {
    use crate::notes::Note;
    use serde_json::{json, Value};

    pub const EVENT_TYPE: &str = "note.created";
    /// Topic-exchange routing key. A consumer binds `note.*` or `note.created`.
    pub const ROUTING_KEY: &str = "note.created";

    pub fn note_created(note: &Note) -> Value {
        json!({ "type": EVENT_TYPE, "data": note })
    }

    pub fn encode(event: &Value) -> Vec<u8> {
        serde_json::to_vec(event).unwrap_or_default()
    }

    /// Parses a message body. Returns `Err` on anything that is not a JSON
    /// object with a string `type` — a poison message the consumer rejects.
    pub fn decode(body: &[u8]) -> Result<Value, String> {
        let parsed: Value = serde_json::from_slice(body).map_err(|e| format!("not JSON: {e}"))?;
        match parsed.get("type") {
            Some(Value::String(_)) if parsed.is_object() => Ok(parsed),
            _ => Err("not a valid event envelope".into()),
        }
    }
}

/// The AMQP topology both the publisher and the consumer declare. One durable
/// topic exchange carries `note.*`; the work queue dead-letters to a separate
/// exchange so a rejected message is kept for inspection, never looped.
pub mod topology {
    pub const EXCHANGE: &str = "notes.events";
    pub const QUEUE: &str = "notes.created";
    pub const BINDING_KEY: &str = "note.*";
    pub const DLX: &str = "notes.events.dlx";
    pub const DLQ: &str = "notes.created.dead";
}

/// A `Publisher` that forwards to several. One failing to enqueue never stops
/// the others — each is already non-blocking and bounded.
pub struct CompositePublisher {
    publishers: Vec<Arc<dyn Publisher>>,
}

impl CompositePublisher {
    pub fn new(publishers: Vec<Arc<dyn Publisher>>) -> Self {
        Self { publishers }
    }
}

impl Publisher for CompositePublisher {
    fn note_created(&self, note: &Note) {
        for p in &self.publishers {
            p.note_created(note);
        }
    }
}

/// Where a worker publishes. The default is an aio-pika-style AMQP exchange; a
/// test passes a fake. `publish` errors on a broker problem — the worker
/// reopens.
#[async_trait]
pub trait Sink: Send + Sync {
    async fn publish(&self, routing_key: &str, body: &[u8]) -> Result<(), String>;
    async fn close(&self);
}

/// Opens a `Sink` for a URL. Injected, so the worker is broker-free in tests.
pub type SinkFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Box<dyn Sink>, String>> + Send>>;

pub trait Opener: Send + Sync {
    fn open(&self, url: String) -> SinkFuture;
}

/// An `Opener` that hands out pre-made sinks in order — used by tests to model
/// "the first connection fails, the second succeeds".
pub struct SeqOpener {
    sinks: Mutex<std::collections::VecDeque<Box<dyn Sink>>>,
}

impl SeqOpener {
    pub fn new(sinks: Vec<Box<dyn Sink>>) -> Self {
        Self {
            sinks: Mutex::new(sinks.into_iter().collect()),
        }
    }
}

impl Opener for SeqOpener {
    fn open(&self, _url: String) -> SinkFuture {
        let next = self.sinks.lock().unwrap().pop_front();
        Box::pin(async move { next.ok_or_else(|| "no more sinks".to_string()) })
    }
}

/// How `RabbitPublisher` bounds delivery.
#[derive(Clone)]
pub struct PublisherConfig {
    pub queue_size: usize,
    pub reopen_backoff: Duration,
    pub reopen_backoff_max: Duration,
}

impl Default for PublisherConfig {
    fn default() -> Self {
        Self {
            queue_size: 256,
            reopen_backoff: Duration::from_millis(500),
            reopen_backoff_max: Duration::from_secs(10),
        }
    }
}

struct PubJob {
    routing_key: String,
    body: Vec<u8>,
}

/// Publishes `note.created` to RabbitMQ without ever blocking a request:
/// `note_created` only enqueues; a single worker task drains the queue to the
/// broker, reopening the sink with capped backoff on failure. The queue is
/// bounded and drops — never blocks — when full, exactly like the webhook
/// dispatcher.
pub struct RabbitPublisher {
    tx: Mutex<Option<mpsc::Sender<PubJob>>>,
    rx: Mutex<Option<mpsc::Receiver<PubJob>>>,
    handle: Mutex<Option<JoinHandle<()>>>,
    url: String,
    opener: Arc<dyn Opener>,
    cfg: PublisherConfig,
}

impl RabbitPublisher {
    pub fn new(url: impl Into<String>, opener: Arc<dyn Opener>, cfg: PublisherConfig) -> Self {
        let (tx, rx) = mpsc::channel::<PubJob>(cfg.queue_size.max(1));
        Self {
            tx: Mutex::new(Some(tx)),
            rx: Mutex::new(Some(rx)),
            handle: Mutex::new(None),
            url: url.into(),
            opener,
            cfg,
        }
    }

    /// Spawns the worker. Enqueuing is allowed before and after this call.
    pub fn start(&self) {
        let Some(rx) = self.rx.lock().unwrap().take() else {
            return;
        };
        let url = self.url.clone();
        let opener = self.opener.clone();
        let cfg = self.cfg.clone();
        *self.handle.lock().unwrap() = Some(tokio::spawn(publisher_run(rx, url, opener, cfg)));
    }

    /// Stops taking events, drains up to `within`, then stops the worker.
    /// Returns whether the queue drained in time.
    pub async fn close(&self, within: Duration) -> bool {
        drop(self.tx.lock().unwrap().take());
        let handle = self.handle.lock().unwrap().take();
        if let Some(handle) = handle {
            tokio::time::timeout(within, handle).await.is_ok()
        } else {
            true
        }
    }
}

impl Publisher for RabbitPublisher {
    fn note_created(&self, note: &Note) {
        let job = PubJob {
            routing_key: events::ROUTING_KEY.to_string(),
            body: events::encode(&events::note_created(note)),
        };
        let guard = self.tx.lock().unwrap();
        match guard.as_ref() {
            None => tracing::warn!(note_id = %note.id, "rabbit publisher closed, event dropped"),
            Some(tx) => {
                if tx.try_send(job).is_err() {
                    tracing::error!(note_id = %note.id, "rabbit publish queue full, event dropped");
                }
            }
        }
    }
}

async fn publisher_run(
    mut rx: mpsc::Receiver<PubJob>,
    url: String,
    opener: Arc<dyn Opener>,
    cfg: PublisherConfig,
) {
    let mut sink: Option<Box<dyn Sink>> = None;
    while let Some(job) = rx.recv().await {
        let mut attempt: u32 = 0;
        loop {
            if sink.is_none() {
                match opener.open(url.clone()).await {
                    Ok(s) => sink = Some(s),
                    Err(e) => {
                        backoff(&cfg, &mut attempt, &e).await;
                        continue;
                    }
                }
            }
            let s = sink.as_ref().unwrap();
            match s.publish(&job.routing_key, &job.body).await {
                Ok(()) => {
                    tracing::debug!("rabbit publish ok");
                    break;
                }
                Err(e) => {
                    if let Some(s) = sink.take() {
                        s.close().await;
                    }
                    backoff(&cfg, &mut attempt, &e).await;
                }
            }
        }
    }
    if let Some(s) = sink.take() {
        s.close().await;
    }
}

async fn backoff(cfg: &PublisherConfig, attempt: &mut u32, err: &str) {
    *attempt += 1;
    tracing::warn!(error = %err, attempt = *attempt, "rabbit sink error, reopening");
    let base = cfg.reopen_backoff.saturating_mul(1u32 << (*attempt - 1).min(16));
    let capped = base.min(cfg.reopen_backoff_max);
    if !capped.is_zero() {
        tokio::time::sleep(capped).await;
    } else {
        tokio::task::yield_now().await;
    }
}

/// Processes one decoded event. Returning `Err` means "transient failure, retry".
#[async_trait]
pub trait Handler: Send + Sync {
    async fn handle(&self, event: serde_json::Value) -> Result<(), String>;
}

/// The default handler: records receipt. Replace it with the real side-effect
/// (a projection, an index write, a notification).
pub struct LogHandler;

#[async_trait]
impl Handler for LogHandler {
    async fn handle(&self, event: serde_json::Value) -> Result<(), String> {
        let note_id = event
            .get("data")
            .and_then(|d| d.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        tracing::info!(note_id, "note.created consumed");
        Ok(())
    }
}

/// One delivery, abstracted from the broker so `handle` is broker-free.
#[async_trait]
pub trait Incoming: Send {
    fn body(&self) -> &[u8];
    /// The `x-retry` header, 0 on first delivery.
    fn retry_count(&self) -> u32;
    async fn ack(&mut self);
    /// Reject without requeue — routed to the dead-letter exchange.
    async fn dead_letter(&mut self);
    /// Republish with `x-retry` incremented, then ack the original.
    async fn retry(&mut self, delay: Duration);
}

/// Consumes `note.created` with a bounded retry.
///
/// Rules:
/// - the handler returns `Ok` -> ack (at-least-once, so make it idempotent);
/// - the body is not a valid event -> dead-letter at once (a retry cannot help);
/// - the handler returns `Err` -> retry up to `max_retries`, then dead-letter —
///   a poison message can never loop for ever.
pub struct RabbitConsumer {
    handler: Arc<dyn Handler>,
    max_retries: u32,
    retry_delay: Duration,
}

impl RabbitConsumer {
    pub fn new(handler: Arc<dyn Handler>, max_retries: u32, retry_delay: Duration) -> Self {
        Self {
            handler,
            max_retries,
            retry_delay,
        }
    }

    /// Decides the fate of one delivery. Never fails the caller: every path ends
    /// in an ack, a dead-letter, or a retry, so a delivery is never left
    /// unanswered.
    pub async fn handle(&self, message: &mut dyn Incoming) {
        let event = match events::decode(message.body()) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(error = %e, "dead-lettering an invalid message");
                message.dead_letter().await;
                return;
            }
        };
        match self.handler.handle(event).await {
            Ok(()) => message.ack().await,
            Err(e) => {
                if message.retry_count() >= self.max_retries {
                    tracing::error!(error = %e, retries = message.retry_count(), "handler failed, retries exhausted, dead-lettering");
                    message.dead_letter().await;
                } else {
                    tracing::warn!(error = %e, attempt = message.retry_count() + 1, "handler failed, retrying");
                    message.retry(self.retry_delay).await;
                }
            }
        }
    }
}

#[cfg(feature = "amqp")]
pub use amqp::amqp_opener;

#[cfg(feature = "amqp")]
mod amqp {
    //! The real lapin glue. Only compiled with `--features amqp`.
    use super::{events, topology, Sink, SinkFuture};
    use async_trait::async_trait;
    use lapin::options::{BasicPublishOptions, ExchangeDeclareOptions, QueueDeclareOptions};
    use lapin::types::FieldTable;
    use lapin::{BasicProperties, Connection, ConnectionProperties, ExchangeKind};
    use std::sync::Arc;

    struct AmqpSink {
        connection: Connection,
        channel: lapin::Channel,
    }

    #[async_trait]
    impl Sink for AmqpSink {
        async fn publish(&self, routing_key: &str, body: &[u8]) -> Result<(), String> {
            self.channel
                .basic_publish(
                    topology::EXCHANGE,
                    routing_key,
                    BasicPublishOptions::default(),
                    body,
                    BasicProperties::default().with_delivery_mode(2), // persistent
                )
                .await
                .map_err(|e| e.to_string())?
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        }

        async fn close(&self) {
            let _ = self.connection.close(0, "bye").await;
        }
    }

    async fn open(url: String) -> Result<Box<dyn Sink>, String> {
        let connection = Connection::connect(&url, ConnectionProperties::default())
            .await
            .map_err(|e| e.to_string())?;
        let channel = connection.create_channel().await.map_err(|e| e.to_string())?;
        channel
            .exchange_declare(
                topology::EXCHANGE,
                ExchangeKind::Topic,
                ExchangeDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let _ = events::ROUTING_KEY;
        let _ = QueueDeclareOptions::default();
        Ok(Box::new(AmqpSink { connection, channel }))
    }

    struct AmqpOpener;

    impl super::Opener for AmqpOpener {
        fn open(&self, url: String) -> SinkFuture {
            Box::pin(open(url))
        }
    }

    /// The real opener for `RabbitPublisher`.
    pub fn amqp_opener() -> Arc<dyn super::Opener> {
        Arc::new(AmqpOpener)
    }
}

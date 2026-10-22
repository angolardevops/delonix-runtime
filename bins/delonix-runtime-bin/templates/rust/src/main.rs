//! The composition root: it reads the configuration, builds every adapter,
//! hands them to the use cases and the HTTP transport, and owns the process
//! lifecycle (signals, readiness, ordered shutdown).

use std::sync::Arc;
use std::time::Duration;

use __MODULE__::cache::{Cache, CachedNotes, InMemoryCache, RedisCache};
use __MODULE__::config;
use __MODULE__::health::Readiness;
use __MODULE__::httpapi::{self, AppState};
use __MODULE__::memstore::InMemoryStore;
use __MODULE__::notes::{NoteService, NotesApi, Publisher};
use __MODULE__::telemetry::TracedNotes;
use __MODULE__::webhook::{self, Dedup, Dispatcher, DispatcherConfig};
use __MODULE__::{logging, messaging};

/// The build's own version (Cargo sets `CARGO_PKG_VERSION`).
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    if let Err(code) = run().await {
        std::process::exit(code);
    }
}

async fn run() -> Result<(), i32> {
    let cfg = match config::load(|k| std::env::var(k).ok(), VERSION) {
        Ok(c) => c,
        Err(problems) => {
            eprintln!("configuration:");
            for p in problems {
                eprintln!("  - {p}");
            }
            return Err(1);
        }
    };
    logging::init(&cfg.log_level, &cfg.service_name, &cfg.version, cfg.env.as_str());

    // Inbound webhook key (optional): enables POST /api/v1/webhooks/inbound.
    let webhook_key = match &cfg.inbound_webhook_secret {
        None => None,
        Some(secret) => match webhook::parse_secret(secret) {
            Ok(k) => Some(Arc::new(k)),
            Err(e) => {
                eprintln!("WEBHOOK_INBOUND_SECRET: {e}");
                return Err(1);
            }
        },
    };

    // Publishers: the outbound webhook dispatcher and/or the RabbitMQ publisher.
    let mut publishers: Vec<Arc<dyn Publisher>> = Vec::new();
    let mut dispatcher: Option<Arc<Dispatcher>> = None;
    if let Some(url) = &cfg.outbound_webhook_url {
        let key = webhook::parse_secret(cfg.outbound_webhook_secret.as_deref().unwrap_or(""))
            .map_err(|e| {
                eprintln!("WEBHOOK_TARGET_SECRET: {e}");
                1
            })?;
        let d = Arc::new(Dispatcher::start(DispatcherConfig {
            url: url.clone(),
            key,
            timeout: Duration::from_secs(5),
            max_attempts: 4,
            base_backoff: Duration::from_millis(500),
            queue_size: 100,
        }));
        publishers.push(d.clone());
        dispatcher = Some(d);
    }

    #[cfg(feature = "amqp")]
    let rabbit = if let Some(url) = &cfg.rabbitmq_url {
        let p = Arc::new(messaging::RabbitPublisher::new(
            url.clone(),
            messaging::amqp_opener(),
            messaging::PublisherConfig::default(),
        ));
        p.start();
        publishers.push(p.clone());
        Some(p)
    } else {
        None
    };
    #[cfg(not(feature = "amqp"))]
    if cfg.rabbitmq_url.is_some() {
        tracing::warn!(
            "RABBITMQ_URL is set but this binary was built without the `amqp` feature; \
             messaging is off. Rebuild with `--features amqp` to publish to the broker."
        );
    }

    let publisher: Option<Arc<dyn Publisher>> = match publishers.len() {
        0 => None,
        1 => Some(publishers.pop().unwrap()),
        _ => Some(Arc::new(messaging::CompositePublisher::new(publishers))),
    };

    // The use cases, then the cache and tracing decorators from the outside.
    let store = Arc::new(InMemoryStore::new());
    let service: Arc<dyn NotesApi> = Arc::new(NoteService::new(store, publisher));
    let cache: Arc<dyn Cache> = match &cfg.redis_url {
        Some(url) => match RedisCache::from_url(url, Duration::from_secs(2)) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("REDIS_URL: {e}");
                return Err(1);
            }
        },
        None => Arc::new(InMemoryCache::new()),
    };
    let cached: Arc<dyn NotesApi> = Arc::new(CachedNotes::new(service, cache, cfg.cache_ttl));
    let notes: Arc<dyn NotesApi> = Arc::new(TracedNotes::new(cached));

    let readiness = Readiness::new();
    let state = AppState {
        notes,
        readiness: readiness.clone(),
        webhook_key,
        dedup: Arc::new(Dedup::new(4096)),
        max_body_bytes: cfg.max_body_bytes,
    };

    let app = httpapi::router(state);
    let listener = match tokio::net::TcpListener::bind(cfg.addr()).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("listen on {}: {e}", cfg.addr());
            return Err(1);
        }
    };
    readiness.set_ready();
    tracing::info!(addr = %cfg.addr(), "listening");

    // Ordered shutdown: stop being ready, let the load balancer notice, finish
    // in-flight requests, then drain the outbound queue.
    let drain = readiness.clone();
    let drain_delay = cfg.drain_delay;
    let shutdown = async move {
        wait_for_signal().await;
        tracing::info!("shutting down");
        drain.set_draining();
        tokio::time::sleep(drain_delay).await;
    };

    let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown);
    if let Err(e) = serve.await {
        tracing::error!(error = %e, "server error");
        return Err(1);
    }

    if let Some(d) = dispatcher {
        d.close(cfg.shutdown_timeout).await;
    }
    #[cfg(feature = "amqp")]
    if let Some(p) = rabbit {
        p.close(cfg.shutdown_timeout).await;
    }
    tracing::info!("stopped");
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

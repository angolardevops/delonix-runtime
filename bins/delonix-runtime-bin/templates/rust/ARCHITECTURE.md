# Architecture

A small hexagonal service: the use cases sit in the middle and know only their
ports; everything technological (HTTP, storage, cache, messaging, telemetry)
plugs in from the edge. The direction is enforced as a test
(`tests/architecture.rs`), so a wrong-way import fails `cargo test`.

## The layers

- **`notes`** — the domain and the use cases. It defines `Note`, the validation
  rules, and two ports: `Store` (persistence) and `Publisher` (announce a new
  note). It imports nothing about HTTP, telemetry, caching or a storage
  technology. `NotesApi` is the trait the outside world calls it through.
- **Adapters** — `memstore` implements `Store`; `webhook::Dispatcher` and
  `messaging::RabbitPublisher` implement `Publisher`; `cache` implements its own
  `Cache` port with an in-memory and a Redis adapter.
- **Decorators** — `cache::CachedNotes` and `telemetry::TracedNotes` wrap a
  `NotesApi` from the OUTSIDE, so caching and tracing are added without the
  `notes` module importing either. They compose in any order.
- **Transport** — `httpapi` turns HTTP into `NotesApi` calls and back. It
  reaches the use cases through the trait, never a storage adapter directly.
- **Composition root** — `main.rs` is the only place that names concrete
  adapters and wires them together.

## The dependency rules (enforced)

| Module | May NOT depend on |
|---|---|
| `notes` | transport, telemetry, cache, messaging, storage, any framework |
| `memstore` | transport, webhook, cache, messaging |
| `webhook` | transport, storage adapter |
| `messaging` | transport, storage adapter |
| `httpapi` | a storage adapter (it uses the `NotesApi` trait) |

## Cross-cutting concerns

- **Errors** — one JSON shape (`{ "error": { code, message, fields?, request_id } }`)
  for every endpoint; mapping lives only in `httpapi`.
- **Observability** — every request gets a request id and a W3C trace id, both
  echoed on the response and carried onto the outbound webhook (`traceparent`),
  so one request is one trace across its logs and the calls it triggers.
- **Resilience** — the outbound dispatcher and the RabbitMQ publisher never
  block a request: they enqueue on a bounded queue that drops (and logs) when
  full, and deliver in the background with capped-backoff retries.
- **Lifecycle** — readiness goes starting → ready → draining; on `SIGTERM` the
  service stops being ready, waits `DRAIN_DELAY`, finishes in-flight requests,
  then drains the outbound queue within `SHUTDOWN_TIMEOUT`.

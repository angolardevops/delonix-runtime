# __NAME__

A JSON-API reference service in Rust (axum + tokio). It comes complete: a small
example capability (notes), inbound and outbound webhooks, health probes,
request-id / access-log / JSON-error middleware, structured JSON logging with a
per-request trace id, a Redis read-through cache, a RabbitMQ messaging module, a
single-file container stack, and a test suite that runs with no external
service.

## Run it

```sh
cargo run                 # reads .env if present; listens on :__PORT__
make check                # fmt + clippy + cargo test (what CI runs)
./scripts/smoke.sh http://localhost:__PORT__
```

Or the whole stack (app + Redis + RabbitMQ) in one command:

```sh
docker compose up -d --build        # or: delonix compose up
```

`delonix init` generated a `.env` with real random secrets for the compose
services; it is git-ignored. `.env.example` documents every variable.

## The API

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/v1/health/live` | liveness |
| GET | `/api/v1/health/ready` | readiness (503 while starting/draining) |
| POST | `/api/v1/notes` | create a note |
| GET | `/api/v1/notes` | list, newest first (`offset`, `limit`) |
| GET | `/api/v1/notes/{id}` | get one |
| POST | `/api/v1/webhooks/inbound` | signed delivery (only with `WEBHOOK_INBOUND_SECRET`) |

`api/openapi.yaml` is the contract; `tests/openapi.rs` fails the build on any
drift between it and the routes.

## Layout

```
src/
  main.rs         composition root (config, wiring, lifecycle)
  config.rs       env -> validated Config
  notes.rs        the use cases + ports (Store, Publisher) — no I/O
  memstore.rs     in-memory Store adapter
  httpapi.rs      routing, decoding, error mapping, middleware
  webhook.rs      Standard Webhooks sign/verify, dedup, outbound dispatcher
  cache.rs        Cache port, in-memory + Redis adapters, cache-aside decorator
  messaging.rs    note.created events, publisher, consumer (RabbitMQ)
  telemetry.rs    W3C trace context + tracing spans
  logging.rs      JSON logging + secret redaction
tests/            api, notes, webhook, cache, messaging, config, logging,
                  trace, openapi, architecture
```

The dependency direction is enforced by `tests/architecture.rs`; see
`ARCHITECTURE.md`.

## Architecture: where to take this

The shape is hexagonal on purpose, so the obvious next steps are adapter swaps,
not rewrites:

- **Persistence** — replace `memstore::InMemoryStore` with a database adapter
  implementing `notes::Store` (sqlx/Postgres). The use cases and their tests do
  not change; wire the new adapter in `main.rs`.
- **Durable webhooks/events** — add an outbox table written in the same
  transaction as the note, and have a relay publish it, for delivery that
  survives a restart (today the queue is in memory).
- **OpenTelemetry export** — point `OTEL_EXPORTER_OTLP_ENDPOINT` at a collector
  and add the OTLP exporter in `telemetry`; the trace context is already W3C.
- **The broker for real** — build with `--features amqp` and run the consumer.

### What not to do yet

- Do not reach from `notes` into HTTP, the cache, messaging, or a database
  driver — `tests/architecture.rs` will fail. Put technology behind a port.
- Do not add a second source of truth for the routes: change `api/openapi.yaml`
  and the route table together.
- Do not rely on the in-memory store, cache or dedup across replicas or
  restarts — they are per-process. Swap the adapter before you scale out.
- Do not log secrets; pass anything sensitive through `logging::redact`.

## Messaging and the broker

The publisher/consumer logic is transport-agnostic and always tested with
in-process fakes, so `cargo test` needs no broker. The real RabbitMQ glue
(lapin) is behind the `amqp` cargo feature — build the image with it to publish
and consume for real:

```sh
cargo build --release --features amqp
```

## Code quality (SonarQube)

```sh
sudo sysctl -w vm.max_map_count=262144      # once, persist in /etc/sysctl.d
docker compose --profile quality up -d      # SonarQube on :9000 + its database
make scan                                   # after the server is UP
```

## On the internet without a public IP

`delonix-tunnel.yaml` puts the service online through an outbound tunnel — no
public IP, no router port, no certificate to manage (the provider serves HTTPS).
It is opt-in: `stack apply` on the manifest never opens it.

```sh
delonix stack apply -f delonix-tunnel.yaml   # open it (needs cloudflared on PATH)
delonix get gateways                         # the public URL
delonix delete gateways __NAME__-tunnel      # close it
```

As written it is a Cloudflare quick tunnel (no account, a random
`https://<words>.trycloudflare.com`). For a stable `https://app.example.org` on
your own Cloudflare-managed domain, create a named tunnel in the dashboard whose
service is `http://localhost:__PORT__`, keep its token in a Delonix secret, and
uncomment the `spec:` block in `delonix-tunnel.yaml`. `pinggy` (plain `ssh`, no
account) and `ngrok` are drop-in alternatives — change only `provider.type`.

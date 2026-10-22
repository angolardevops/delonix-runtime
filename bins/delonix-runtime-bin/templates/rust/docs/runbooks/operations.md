# Operations runbook

## Health and readiness
- `GET /api/v1/health/live` — the process answers. Used by the container
  `HEALTHCHECK`.
- `GET /api/v1/health/ready` — 200 only when ready; 503 while starting or
  draining. Point the load balancer here.

## Configuration
All configuration is environment variables (see `.env.example`). A bad value
stops the process at startup with every problem listed at once — read stderr.

## Graceful shutdown
On `SIGTERM`/`SIGINT` the service: marks readiness `draining` (so the balancer
stops routing), waits `DRAIN_DELAY`, finishes in-flight requests, then drains
the outbound webhook queue within `SHUTDOWN_TIMEOUT`. `STOPSIGNAL SIGTERM` and
the `exec` in the entrypoint make sure the signal reaches PID 1.

## Logs and tracing
Structured JSON on stdout. Every line emitted while handling a request carries
its `trace_id` and `request_id`; the same trace id is echoed on the response
(`x-trace-id`) and on the outbound webhook (`traceparent`), so one request is
one trace end to end. To export spans, run a collector and set
`OTEL_EXPORTER_OTLP_ENDPOINT` (a ready-made dev collector is in
`deploy/otel-collector.yaml`).

## Cache
`REDIS_URL` turns on the read-through cache; unset uses an in-process cache. A
down Redis is a slow path, never an error — every lookup falls through to the
store and the outage is logged once.

## Messaging
`RABBITMQ_URL` turns on publishing/consuming `note.created`. The broker glue is
behind the `amqp` cargo feature; a binary built without it logs a warning and
runs with messaging off. The work queue dead-letters poison and
retry-exhausted messages to `notes.created.dead` — inspect it there.

## SonarQube
`vm.max_map_count=262144` on the host, then `docker compose --profile quality
up -d` and `make scan` once `GET :9000/api/system/status` is `UP`.

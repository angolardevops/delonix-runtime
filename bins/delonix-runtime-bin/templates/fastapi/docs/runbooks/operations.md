# Operations runbook — __NAME__

## Start
- Local: `make run` (or `make dev` with auto-reload). Expect a `ready` log line,
  then uvicorn's `Uvicorn running on`.
- Delonix: `delonix build -t __NAME__:dev . && delonix stack apply`.
- Healthy when `GET /api/v1/health/ready` is 200.

## Common failures
| Symptom | Check |
|---|---|
| process exits at start with `configuration:` | the list names each bad variable |
| `address already in use` at start | another process holds `PORT` |
| ready 503 `draining` | a SIGTERM was received; the process is stopping |
| 413 on requests | body over `HTTP_MAX_BODY_BYTES` (256 KiB for webhooks) |
| 500 with `internal` | the `unhandled error serving request` log line has the stack and the same `request_id` |
| `webhook delivery failed` logs | receiver down or answering 4xx; see `webhook.deliveries{outcome}` |
| `webhook queue full` logs | receiver slower than note creation; events are being dropped |
| `Transient error ... exporting` warnings | the collector is unreachable; requests are not affected |

## Observability
Run a local collector and point the service at it:
```bash
docker run --rm -p 4318:4318 -v "$PWD/deploy/otel-collector.yaml:/etc/otelcol-contrib/config.yaml:ro" \
  otel/opentelemetry-collector-contrib:0.161.0
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 make run
```
Send a request, copy `trace_id` from its log line, and find the same id in the
collector output (`Trace ID`). Stop the collector: the service keeps serving;
the exporter logs warnings and drops the batches.

## Shutdown
SIGTERM (what `delonix container stop` sends) or SIGINT. Order: readiness 503 →
`DRAIN_DELAY` → in-flight requests finish → webhook queue drains → telemetry
flushes (at most 5 s). All bounded by `SHUTDOWN_TIMEOUT`; exit 0 on a complete
stop, 1 when the deadline cut it (the log says `stopped before in-flight work
finished`). A repeated SIGTERM changes nothing; a second SIGINT stops waiting.
Give the container runtime a stop timeout longer than `SHUTDOWN_TIMEOUT`.

## Deploy and roll back
Build a new tag (`delonix build -t __NAME__:<version> .`), change `spec.image`
in `delonix-manifest.yaml`, `delonix stack plan` then `delonix stack apply`. To
roll back, put the previous tag back and apply again. State is in memory, so a
restart empties it — expected for the example.

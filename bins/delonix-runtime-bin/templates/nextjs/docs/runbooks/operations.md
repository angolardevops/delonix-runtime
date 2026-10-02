# Operations runbook — __NAME__

## Start
- Local: `pnpm build && pnpm start`. Expect a `ready` log line (JSON) after the Next.js banner.
- Delonix: `delonix build -t __NAME__:dev . && delonix stack apply`.
- Healthy when `GET /api/v1/health/ready` is 200.

## Common failures
| Symptom | Check |
|---|---|
| process exits at start | the `fatal: configuration:` message lists each bad variable |
| ready 503 `draining` | a SIGTERM was received; the process is stopping |
| API answers 503 `shutting_down` | the drain delay is over; the process exits when requests in flight finish |
| 413 on requests | body over `HTTP_MAX_BODY_BYTES` (256 KiB for inbound webhooks) |
| `webhook delivery failed` logs | receiver down or answering 4xx; see `webhook.deliveries{outcome}` |
| `webhook dispatcher full` logs | receiver slower than note creation; events are being dropped |
| exit code 1 after a stop | `SHUTDOWN_TIMEOUT` cut the shutdown; the log names what was pending |
| exit code 143 after a stop | `NEXT_MANUAL_SIG_HANDLE` was not set: Next.js handled the signal |

## Observability
Run a local collector and point the server at it:
```bash
docker run --rm -p 4318:4318 -v "$PWD/deploy/otel-collector.yaml:/etc/otelcol-contrib/config.yaml:ro" \
  otel/opentelemetry-collector-contrib:0.161.0
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 pnpm start
```
Create a note, copy `trace_id` from its `request` log line, and find the same
id in the collector output (`Trace ID`), on the Next.js spans and on
`notes.create`. Spans are exported in batches, a few seconds after the
request. Stop the collector: the server keeps serving; only the export fails.

## Shutdown
SIGTERM (what `delonix container stop` sends) or SIGINT. Order: readiness 503 →
`DRAIN_DELAY` → new API requests 503 → in-flight requests finish → pending
webhooks drain → telemetry flushes (at most 3 s). Bounded by
`SHUTDOWN_TIMEOUT`; exit 0 on a complete stop, 1 when the deadline cut it. A
second signal exits immediately with 1. Give the platform's stop timeout at
least `DRAIN_DELAY` + `SHUTDOWN_TIMEOUT`.

## Deploy and roll back
Build a new tag (`delonix build -t __NAME__:<version> --build-arg VERSION=<version> .`),
change `spec.image` in `delonix-manifest.yaml`, `delonix stack plan` then
`delonix stack apply`. To roll back, put the previous tag back and apply
again. State is in memory, so a restart empties it — expected for the example.

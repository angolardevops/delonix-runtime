# Operations runbook — __NAME__

## Start
- Local: `pnpm build && pnpm start` (or `pnpm start:dev` to watch). Expect a
  `listening` log line.
- Delonix: `delonix build -t __NAME__:dev . && delonix stack apply`.
- Healthy when `GET /api/v1/health/ready` is 200.

## Common failures
| Symptom | Check |
|---|---|
| process exits at start with `fatal: configuration:` | the list names each bad variable |
| process exits at start with `EADDRINUSE` | another process holds `PORT` |
| ready stays 503 `starting` | the listener did not come up — look for the error above it |
| ready 503 `draining` | a SIGTERM was received; the process is stopping |
| 413 on requests | body over `HTTP_MAX_BODY_BYTES` |
| `webhook delivery failed` logs | receiver down or answering 4xx; see `webhook.deliveries{outcome}` |
| `webhook queue full` logs | receiver slower than note creation; events are being dropped |
| `telemetry: …` warnings | the collector is unreachable; requests are not affected |

## Observability
Run a local collector and point the service at it:
```bash
docker run --rm -p 4318:4318 -v "$PWD/deploy/otel-collector.yaml:/etc/otelcol-contrib/config.yaml:ro" \
  otel/opentelemetry-collector-contrib:0.161.0
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 pnpm start
```
Send a request, copy `trace_id` from its log line, and find the same id in the
collector output (`Trace ID`). Stop the collector: the service keeps serving;
only export fails (logged as `telemetry: …` warnings).

## Shutdown
SIGTERM (what `delonix container stop` sends) or SIGINT. Order: readiness 503 →
`DRAIN_DELAY` → in-flight requests finish → webhook queue drains → telemetry
flushes. Everything after the drain delay is bounded by `SHUTDOWN_TIMEOUT`;
exit 0 on a complete stop, 1 when the deadline cut it. A second signal kills
immediately.

## Deploy and roll back
Build a new tag (`delonix build -t __NAME__:<version> --build-arg VERSION=<version> .`),
change `spec.image` in `delonix-manifest.yaml`, `delonix stack plan` then
`delonix stack apply`. To roll back, put the previous tag back and apply
again. State is in memory, so a restart empties it — expected for the example.

# Operations runbook — __NAME__

## Start
- Local: `composer serve` (validates the configuration, migrates, `artisan
  serve` on `:__PORT__`) and, for outbound webhooks, `php artisan queue:work`.
- Delonix: create the `__NAME__-app` secret (APP_KEY), then
  `delonix build -t __NAME__:dev . && delonix stack apply`.
- Healthy when `GET /api/v1/health/ready` is 200.

## Common failures
| Symptom | Check |
|---|---|
| container exits at start | the `configuration:` output lists each bad variable |
| port refuses connections just after start | the entrypoint is still validating/migrating |
| ready 503 `starting` | schema not migrated: `php artisan migrate --force` |
| ready 503 `unavailable`, `dependency: database` | the SQLite file or its directory is missing or read-only (is the volume mounted?) |
| ready 503 `draining` | a SIGTERM was received; the server is stopping |
| 413 on requests | body over `HTTP_MAX_BODY_BYTES` |
| `webhook delivery attempt failed` logs | receiver down or answering 429/5xx; it is retried |
| `webhook delivery failed` logs | attempts exhausted or a final 4xx: `php artisan queue:failed`, then `queue:retry <id>` |
| jobs pile up in `jobs` | the worker is not running (`delonix container logs __NAME__-worker`) |
| `database is locked` | a long write holds SQLite; `busy_timeout` is 5 s — move to a server database for concurrency |

## Observability
Run a local collector and point the service at it:
```bash
docker run --rm -p 4318:4318 -v "$PWD/deploy/otel-collector.yaml:/etc/otelcol-contrib/config.yaml:ro" \
  otel/opentelemetry-collector-contrib:0.161.0
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 composer serve
```
Send a request, copy `trace_id` from its log line, and find the same id in the
collector output (`Trace ID`). Stop the collector: the service keeps serving;
each request logs an export error and spends up to `OTEL_EXPORTER_OTLP_TIMEOUT`
after its response was sent.

## Shutdown
SIGTERM (what `delonix container stop` sends) or SIGINT.

- **Web (FrankenPHP)**: readiness answers 503 `draining` and requests are
  still served for `DRAIN_DELAY`; then the listener closes, in-flight
  requests get up to `SHUTDOWN_TIMEOUT`, and the process exits 0. With
  `DRAIN_DELAY=0s` (default) the listener closes immediately. Behind a load
  balancer set `DRAIN_DELAY` to a little more than its probe interval.
- **Worker**: the image has `ext-pcntl`, so `queue:work` handles SIGTERM
  itself: it finishes the job in hand, then exits 0 (an idle worker logs
  `Worker STOPPED`). A delivery that is still cut — the stop timeout expired,
  or the host died — stays reserved and runs again after 90 s (`retry_after`):
  at-least-once, and the receiver de-duplicates by `webhook-id`.
- Telemetry is flushed per request and per job, so nothing waits at shutdown.

## Deploy and roll back
Build a new tag (`delonix build -t __NAME__:<version> --build-arg VERSION=<version> .`),
change `spec.image` of both containers in `delonix-manifest.yaml`,
`delonix stack plan` then `delonix stack apply`. The web container migrates
at start. To roll back, put the previous tag back and apply again — the
schema is not rolled back, so keep migrations backward compatible (add
columns nullable, remove them one release later). Back up the volume
(`__NAME__-data`) before a migration that rewrites data.

## Rotate secrets
`delonix secret create __NAME__-app --force --from-literal APP_KEY=… …`, then
restart both containers (the configuration is cached at container start).
For a webhook secret, the sender may sign with both keys during the change:
any one valid signature in `webhook-signature` is accepted.

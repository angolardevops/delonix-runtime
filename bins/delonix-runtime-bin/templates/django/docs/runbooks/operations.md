# Operations runbook — __NAME__

## Start
- Local: `make migrate` then `make run` (gunicorn). Expect `Listening at` and
  one `telemetry configured` line per worker.
- Delonix: `delonix build -t __NAME__:dev . && delonix stack apply`. The
  container migrates at start (`MIGRATE_ON_START=true` in the manifest);
  without that variable, run
  `delonix container exec __NAME__ /app/.venv/bin/python manage.py migrate`.
- Healthy when `GET /api/v1/health/ready` is 200.

## Common failures
| Symptom | Check |
|---|---|
| process exits at start with `ImproperlyConfigured: configuration:` | the message lists each bad variable |
| ready 503 `"dependency": "migrations"` | migrations not applied: run `manage.py migrate` (or set `MIGRATE_ON_START=true` for one replica) |
| ready 503 `"dependency": "database"` | the database does not answer: `DATABASE_URL`, network, credentials |
| ready 503 `draining` | a SIGTERM was received; the process is stopping |
| every request 301 → https | production defaults without `TRUSTED_PROXY`; README "Behind a proxy" |
| 400 `bad_request` on every request | Host header not in `ALLOWED_HOSTS` |
| 413 on requests | body over `HTTP_MAX_BODY_BYTES` |
| `webhook delivery failed` logs | receiver down or answering 4xx; see `webhook.deliveries{outcome}` |
| `webhook queue full` logs | receiver slower than note creation; events are being dropped |
| `database is locked` (SQLite) | more writers than SQLite takes; move to PostgreSQL |

## Migrations
Run once per release, before or while the new version starts, from one
place: `manage.py migrate`. `manage.py showmigrations` lists what is applied.
New replicas stay unready until the schema matches their code. Write
backwards-compatible migrations (add first, remove in a later release) so
the old version keeps working while the new one rolls out.

## Observability
Run a local collector and point the service at it:
```bash
docker run --rm -p 4318:4318 -v "$PWD/deploy/otel-collector.yaml:/etc/otelcol-contrib/config.yaml:ro" \
  otel/opentelemetry-collector-contrib:0.161.0
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 make run
```
Send a request, copy `trace_id` from its `request` log line, and find the
same id in the collector output (`Trace ID`). Stop the collector: the service
keeps serving; only export fails (logged by the SDK).

## Shutdown
SIGTERM (what `delonix container stop` sends). Order: readiness 503 →
`DRAIN_DELAY` → in-flight requests finish → webhook queue drains → telemetry
flushes. All bounded by `SHUTDOWN_TIMEOUT`, after which gunicorn kills the
workers; the exit code is 0 either way. SIGINT/SIGQUIT stop at once without
draining. With an OTLP endpoint that does not answer, the final telemetry
flush uses the rest of the budget (measured: about 8 s of a 15 s
`SHUTDOWN_TIMEOUT`); lower `SHUTDOWN_TIMEOUT` or fix the collector.

## Deploy and roll back
Build a new tag (`delonix build -t __NAME__:<version> --build-arg VERSION=<version> .`),
change `spec.image` in `delonix-manifest.yaml`, `delonix stack plan` then
`delonix stack apply`, then run the migrations. To roll back, put the
previous tag back and apply again — which works only if the migrations of
the newer version were backwards compatible; otherwise reverse them first
(`manage.py migrate <app> <previous migration>`). Data lives on the
`__NAME__-data` volume and survives a container replacement.

## Rotating secrets
- `SECRET_KEY`: set the new value and restart. With `SECRET_KEY_FILE`,
  delete the file and restart (a new key is generated). Nothing in this
  service is signed with it yet; once sessions or signed tokens exist, use
  Django's `SECRET_KEY_FALLBACKS`.
- Webhook secrets: senders may send several space-separated signatures
  during a rotation; this service verifies against one inbound secret.

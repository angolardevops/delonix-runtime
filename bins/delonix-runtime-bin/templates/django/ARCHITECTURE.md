# Architecture

A modular monolith with one capability, in Django's own shape: a project
package (`config`), one app per domain (`notes`), an adapter app
(`webhooks`), and a package for what every capability shares (`core`).

```
              gunicorn.conf.py ── config/wsgi.py (composition: telemetry, app, readiness)
                                        │
 HTTP ─► core.middleware ─► config/urls.py ─► notes/views.py ─► notes/services.py ─► notes/models.py (ORM)
          (probes, request id,                 (decode/encode)   (rules, paging,        │
           tracing, access log,                       ▲           note_created signal)  ▼
           error shape)                               │                 │            database
                                 webhooks/views.py ─► webhooks/inbound.py│
                                 (signature, raw body)  (dedup + use case)▼
                                                        webhooks/receivers.py ─► webhooks/dispatcher.py ─► receiver
```

## Dependency rules (enforced by `tests/test_architecture.py`)
- `notes` models/services/signals import nothing about HTTP (`django.http`,
  `django.urls`, views), nothing from `core` or `webhooks`, no HTTP client and
  no telemetry SDK. They may use `django.db` and the OpenTelemetry **API**.
- `notes/views.py` and `notes/urls.py` never import `django.db` or
  `notes.models`: storage is reached only through `notes.services`.
- `core` imports neither `notes` nor `webhooks`.
- `webhooks` uses `notes.services` and `notes.signals`, never notes' views,
  URLs or models.
- `config/env.py` is plain Python — no Django, no project package — because
  gunicorn reads it before Django exists.

Proof the gate works: add `from django.http import JsonResponse` to
`notes/services.py` and run `make test` — `test_architecture.py` fails naming
the file, the line and the import.

## Concessions to the framework, on purpose
- **The ORM in the use cases.** `notes/services.py` calls
  `Note.objects…` directly. A repository interface over the ORM would
  restate the ORM's API for a second implementation that does not exist; in
  Django the model layer *is* the persistence port, and tests run it against
  in-memory SQLite. What stays out of the models and the views is the rules:
  validation, limits, paging and events live in the services.
- **Signals as the outbound port.** The capability announces `note_created`
  (after the transaction commits); the webhooks app subscribes. Notes does
  not know a webhook exists.
- **Tracing through the OpenTelemetry API.** Services open their own
  `notes.*` span with `opentelemetry.trace` (the API package, a no-op
  without an SDK). The SDK, exporters and instrumentation are configured
  only in `core/telemetry.py`.
- **Module-level process state.** Readiness (`core.health.state`) and the
  dispatcher (`webhooks.receivers`) are per-process singletons, as Django's
  own `settings` and `connection` are.

## CSRF
`core.http.api_view` marks every API view `csrf_exempt`. The API is for
non-browser clients and neither sets nor reads a cookie, so a forged
cross-site request has no ambient credential to use; a CSRF token would
protect nothing and break every client. `CsrfViewMiddleware` remains
installed for browser-facing views added later. Adding cookie or session
authentication to the API removes the premise: drop the exemption then.

## Request path of the example
`POST /api/v1/notes` → `health_probes`, `request_id` → OpenTelemetry server
span → `access_log` → Django's security/common middleware → `api_view`
dispatches by method → `notes.views.create` parses the body strictly
(`core.http.json_object`) → `notes.services.create_note` opens the
`notes.create` span, validates, inserts, and registers the event for
commit → `webhooks.receivers.on_note_created` queues it with the current
trace context → the view answers 201. The dispatcher thread later delivers
with retries, as a `webhook.deliver` span of the same trace.

## Extending: add `DELETE /api/v1/notes/{note_id}`
1. `notes/services.py`: `delete_note(note_id)` — reuse the id parsing of
   `get_note`, raise `NoteNotFound` when absent, inside a `notes.delete` span.
2. `notes/views.py`: a `delete` view mapping `NoteNotFound` to 404 and
   answering 204.
3. `notes/urls.py`: `api_view(GET=views.detail, DELETE=views.delete)`.
4. `api/openapi.yaml`: document `delete` under `/api/v1/notes/{note_id}`.
5. Tests: a service test and an API test; `make test` runs the drift and
   architecture gates. No migration (no model change).

A second transport (gRPC, a management command, a queue consumer) plugs in
the same way the inbound webhook does: it calls `notes.services`.

## Why not more
No Django REST Framework, no serializer layer, no repository, no task queue:
three endpoints and one event do not need them, and each would be a
dependency to upgrade. Add DRF when you need content negotiation, browsable
docs or its auth/permission classes; add a queue (and an outbox table) when
an event must survive a restart — see docs/adr/0001 and 0002.

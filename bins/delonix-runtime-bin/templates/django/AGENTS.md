# AGENTS.md — working on __NAME__

Short on purpose. The README explains the service; this file tells an agent
(or a new contributor) how to change it safely.

## Map
- Capability and rules: `notes/services.py` (use cases), `notes/models.py` (ORM), `notes/signals.py` (events).
- Transport: `notes/views.py`, `notes/urls.py`, `config/urls.py` · webhooks adapter: `webhooks/`.
- Cross-cutting (no capability imports): `core/` — error shape, strict JSON, middleware, readiness, logging, telemetry.
- Configuration: `config/env.py` (parsing, validation) → `config/settings.py` · server lifecycle: `gunicorn.conf.py`, `config/wsgi.py`.
- Sources of truth: routes → `api/openapi.yaml`; dependency rules → `tests/test_architecture.py`;
  configuration → `config/env.py` + `.env.example` + the README table; decisions → `docs/adr/`.

## Verify every change
```bash
make check        # fmt-check, lint, migrations-check, deploy-check, test — must pass before a commit
make vuln         # when dependencies change (network)
```
After `make migrate` and `make run`, `make smoke` must print `smoke: OK`.

## Rules
- A new route goes through `core.http.api_view` and into `api/openapi.yaml` in the same change (the drift test fails otherwise).
- Views never import `django.db` or `notes.models`; rules and ORM calls live in `notes/services.py`.
- `notes` never imports `core`, `webhooks`, `django.http` or the telemetry SDK; other apps subscribe to its signals.
- A model change ships with its migration (`uv run python manage.py makemigrations`); never edit an applied migration.
- The server never runs migrations; do not add `migrate` to the image's CMD or to a gunicorn hook.
- New configuration: add it to `config/env.py` (validated), `.env.example` and the README table.
- Never log request bodies, headers or query strings; sensitive keys are redacted by key name.
- Secrets only from the environment. Never commit `.env`, a real `SECRET_KEY` or a real `whsec_` value.
- Dependencies change through `uv add` / `uv lock`; commit `uv.lock`, never edit it by hand.
- Do not enable gunicorn's `preload_app` (telemetry and the webhook thread must start after the fork).

## Out of scope for a routine task
Changing the webhook format, the error shape, the `/api/v1` contract in a
breaking way, the CSRF exemption of the API, or silencing another deploy
check needs an ADR in `docs/adr/` first.

# AGENTS.md — working on __NAME__

Short on purpose. The README explains the service; this file tells an agent
(or a new contributor) how to change it safely.

## Map
- Capability and rules: `src/__MODULE__/notes/{domain,ports,service}.py` (standard
  library only — no FastAPI, no httpx, no telemetry, no settings).
- Transport: `notes/api.py`, `webhooks/api.py`, `web/` · adapters: `notes/memory.py`,
  `webhooks/dispatcher.py`.
- Wiring: `app.py` (the only place that builds adapters) · process and signals: `server.py`.
- Sources of truth: routes → the code, mirrored in `api/openapi.json`; dependency
  rules → `tests/test_architecture.py`; configuration → `config.py` + `.env.example`;
  decisions → `docs/adr/`.

## Verify every change
```bash
make fmt-check lint typecheck test   # must pass before a commit
make build                           # the wheel still builds
make audit                           # when dependencies change (network)
```
With the server running (`make run`), `make smoke` must print `smoke: OK`.

## Rules
- A new or changed route or schema: run `make openapi` and commit `api/openapi.json`
  in the same change (the drift test fails otherwise).
- A new dependency of the use cases goes behind a `Protocol` in `notes/ports.py`;
  add the module to `CORE` in `tests/test_architecture.py` if it must stay pure.
- New configuration: a typed field in `config.py`, plus `.env.example` and the README table.
- Inside the package use relative imports (`from .domain import Note`); tests import
  the package by name, in the parenthesised form (see `tests/conftest.py`).
- Never log request bodies, headers or query strings; sensitive keys are redacted by name.
- Secrets only from the environment. Never commit `.env` or a real `whsec_` value.
- Dependencies change through `uv add` / `uv lock`; never edit `uv.lock` by hand.
- Do not call blocking I/O inside `async def` (Ruff's `ASYNC` rules catch the common cases).

## Out of scope for a routine task
Changing the webhook format, the error shape, or the `/api/v1` contract in a
breaking way needs an ADR in `docs/adr/` first.

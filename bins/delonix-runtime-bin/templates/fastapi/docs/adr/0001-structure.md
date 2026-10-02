# 0001 — Capability packages, Protocol ports, contrib OpenTelemetry

Status: accepted (template default)

## Context
FastAPI gives routing, validation, OpenAPI and dependency injection. It does
not say where rules live, and since 0.142 it also ships an OpenTelemetry
integration of its own, next to the long-standing contrib package.

## Decision
- One package per capability (`notes/`), holding its domain, ports
  (`typing.Protocol`), use cases, adapter and router. Use cases are plain
  classes with `async` methods and no framework import; routes get them through
  a `Depends` function reading `app.state`. Dependency direction is a test
  (`tests/test_architecture.py`, an `ast` walk), not a convention.
- The process entry is `python -m <package>` (`server.py`), not the `uvicorn`
  command line: a `uvicorn.Server` subclass flips readiness on the first
  signal, waits `DRAIN_DELAY`, and makes the exit code say whether the stop
  was complete. Plain uvicorn re-raises the signal after a graceful stop and
  exits 143 either way.
- Tracing uses `opentelemetry-instrumentation-fastapi` and `-httpx`. FastAPI's
  native integration is switched off (`telemetry={...}` in `app.py`).
- Type checking with mypy `--strict` and the Pydantic plugin.

## Alternatives
- Layer-per-directory (`routers/`, `services/`, `repositories/`): names layers
  instead of capabilities and spreads one change over three directories.
- FastAPI's native telemetry (0.142+): fewer dependencies, but it does not
  exist on 0.141, and running both attaches two exporters to one provider —
  measured: 10 spans exported for a request that produces 5.
- import-linter for the architecture gate: more expressive (transitive
  contracts), but one more tool and config format for a rule that fits in a
  page of test code.
- pyright instead of mypy: faster, but its PyPI package downloads a Node.js
  runtime on first use; mypy installs from `uv.lock` like everything else.
- Multiple uvicorn workers: the example keeps state in memory, per process.

## Trade-off
The server subclass depends on `uvicorn.Server.handle_exit`, `should_exit` and
`force_exit`; `tests/test_server.py` fails if a uvicorn upgrade renames them.
Moving to FastAPI's native telemetry later means
deleting the contrib dependency and the `telemetry=` argument together.

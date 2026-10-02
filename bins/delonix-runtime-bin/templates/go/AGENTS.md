# AGENTS.md — working on __NAME__

Short on purpose. The README explains the service; this file tells an agent
(or a new contributor) how to change it safely.

## Map
- Capability and rules: `internal/notes/` (no HTTP, no telemetry, no storage imports).
- Transport: `internal/httpapi/` · adapters: `internal/memstore/`, `internal/webhook/`.
- Wiring and lifecycle: `cmd/__NAME__/main.go` — the only place that builds adapters.
- Sources of truth: routes → `api/openapi.yaml`; dependency rules → `internal/archtest`;
  configuration → `internal/config` + `.env.example`; decisions → `docs/adr/`.

## Verify every change
```bash
make fmt-check vet race build   # must pass before a commit
make vuln                       # when dependencies change (network)
```
After starting the server, `make smoke` must print `smoke: OK`.

## Rules
- A new route updates `api/openapi.yaml` in the same change (the drift test fails otherwise).
- A new dependency of `internal/notes` goes behind an interface defined in `notes`.
- New configuration: add it to `internal/config` (validated), `.env.example` and the README table.
- Never log request bodies, headers or query strings; sensitive keys are redacted by key name.
- Secrets only from the environment. Never commit `.env` or a real `whsec_` value.
- Do not edit `go.sum` by hand; use `go get` / `go mod tidy`.

## Out of scope for a routine task
Changing the webhook format, the error shape, or the `/api/v1` contract in a
breaking way needs an ADR in `docs/adr/` first.

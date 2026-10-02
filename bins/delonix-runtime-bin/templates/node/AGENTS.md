# AGENTS.md — working on __NAME__

Short on purpose. The README explains the service; this file tells an agent
(or a new contributor) how to change it safely.

## Map
- Capability and rules: `src/notes/notes.ts` (no Fastify, no telemetry, no storage imports).
- Transport: `src/app.ts`, `src/notes/routes.ts`, `src/health/routes.ts`, `src/webhooks/routes.ts`, `src/http/errors.ts`.
- Adapters: `src/notes/memory-store.ts`, `src/webhooks/dispatcher.ts`.
- Wiring and lifecycle: `src/index.ts` — the only place that builds adapters. `src/instrumentation.ts` is loaded before it (`node --import`).
- Sources of truth: routes and contract → the route schemas (`api/openapi.json` is their checked-in copy);
  dependency rules → `test/architecture.test.ts`; configuration → `src/config.ts` + `.env.example`; decisions → `docs/adr/`.

## Verify every change
```bash
pnpm check        # format check, lint, typecheck, tests, build — must pass before a commit
pnpm smoke        # against a running instance; prints "smoke: OK"
```

## Rules
- A route or schema change regenerates the contract in the same change: `pnpm openapi:write` (the drift test fails otherwise).
- A new dependency of `src/notes/notes.ts` goes behind an interface declared in that file.
- New configuration: add it to `src/config.ts` (validated), `.env.example` and the README table.
- Never log request bodies, headers or query strings; sensitive keys are redacted by key name (`src/logging.ts`).
- Secrets only from the environment. Never commit `.env` or a real `whsec_` value.
- Dependencies change through `pnpm add`/`pnpm remove`; commit `pnpm-lock.yaml`, never edit it by hand.

## Out of scope for a routine task
Changing the webhook format, the error shape, or the `/api/v1` contract in a
breaking way needs an ADR in `docs/adr/` first.

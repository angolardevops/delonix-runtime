# AGENTS.md — working on __NAME__

Short on purpose. The README explains the service; this file tells an agent
(or a new contributor) how to change it safely.

## Map
- Capability and rules: `src/notes/note.ts`, `notes.service.ts` and the ports
  `note.repository.ts`, `note.publisher.ts` (no HTTP, no telemetry, no storage imports).
- Transport: `src/notes/notes.controller.ts`, `src/webhooks/inbound-webhook.controller.ts`,
  cross-cutting pieces in `src/http/` · adapters: `in-memory-note.repository.ts`,
  `src/webhooks/webhook-dispatcher.ts`.
- Wiring: the `*.module.ts` files say which adapter implements which port;
  `src/app.setup.ts` builds the application; `src/main.ts` owns the lifecycle.
- Sources of truth: routes → controllers, mirrored in `api/openapi.json`; dependency
  rules → `src/architecture.spec.ts`; configuration → `src/config/app-config.ts` +
  `.env.example`; decisions → `docs/adr/`.

## Verify every change
```bash
pnpm check              # format:check, lint, typecheck, test, build — must pass before a commit
pnpm audit --prod       # when dependencies change (network)
```
After `pnpm start`, `pnpm smoke` must print `smoke: OK`.

## Rules
- A new or changed route, DTO or response: run `pnpm openapi:write` and commit
  `api/openapi.json` in the same change (the drift test fails otherwise). Never edit
  that file by hand.
- A new dependency of the use cases goes behind an abstract class in `src/notes/`
  (the injection token), bound to an adapter in a module.
- New configuration: add it to `src/config/app-config.ts` (validated), `.env.example`
  and the README table. Nothing else reads `process.env`.
- Never log request bodies, headers or query strings; sensitive keys are redacted by key name.
- Secrets only from the environment. Never commit `.env` or a real `whsec_` value.
- Every `@nestjs/*` package stays on the same major. Do not edit `pnpm-lock.yaml`
  by hand; use `pnpm add` / `pnpm update`.
- A dependency with an install script must be reviewed and listed in
  `pnpm-workspace.yaml` (`allowBuilds`).

## Out of scope for a routine task
Changing the webhook format, the error shape, or the `/api/v1` contract in a
breaking way needs an ADR in `docs/adr/` first.

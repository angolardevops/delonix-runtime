# AGENTS.md — working on __NAME__

Short on purpose. The README explains the application; this file tells an
agent (or a new contributor) how to change it safely.

## Map
- Capability and rules: `lib/notes/` (no Next.js, no telemetry, no storage imports).
- Transport: `app/api/**/route.ts` over `lib/http/route.ts` · adapters and infrastructure: `lib/server/`.
- UI: `app/(list)/page.tsx` (Server Component), `app/(list)/note-form.tsx` (the only Client Component with state).
- Wiring: `lib/server/runtime.ts` — the only place that builds adapters; started by `instrumentation.ts` → `lib/server/boot.ts`.
- Sources of truth: routes → `api/openapi.yaml`; dependency rules → `tests/architecture.test.ts`;
  configuration → `lib/server/config.ts` + `.env.example`; decisions → `docs/adr/`.

## Verify every change
```bash
pnpm check                  # format check, lint, typecheck, tests, production build
pnpm smoke:production       # starts the built server, smoke, SIGTERM must exit 0
pnpm audit --prod --audit-level high   # when dependencies change (network)
```

## Rules
- A new or changed Route Handler updates `api/openapi.yaml` in the same change (the drift test fails otherwise).
- Wrap every Route Handler in `route()` from `lib/http/route.ts`: it gives the error shape, the request id, the access log and the in-flight count the shutdown waits for.
- Every module under `lib/server`, `lib/notes` and `lib/http` starts with `import "server-only"`. A Client Component (`"use client"`) imports none of them.
- Never add a `NEXT_PUBLIC_` variable for a secret; server configuration goes through `lib/server/config.ts` (validated), `.env.example` and the README table.
- State shared between `instrumentation.ts` and the routes lives on `globalThis` (see `lib/server/runtime.ts`): Next.js bundles them separately, so module-level variables and `instanceof` do not cross that boundary.
- A new dependency of `lib/notes` goes behind an interface declared in `lib/notes`.
- Never log request bodies, headers or query strings; sensitive keys are redacted by key name.
- Do not edit `pnpm-lock.yaml` by hand; use `pnpm add` / `pnpm update`. Keep it committed.
- `lib/project.ts` is excluded from Prettier on purpose; do not move the project name into other source files.

## Next.js writes below this line
From Next.js 16.3, `next dev` appends a `nextjs-agent-rules` block to this file
when it detects a coding agent (it points at the documentation shipped in
`node_modules/next/dist/docs/`). Commit it; `agentRules: false` in
`next.config.ts` turns it off.

## Out of scope for a routine task
Changing the webhook format, the error shape, the shutdown sequence, or the
`/api/v1` contract in a breaking way needs an ADR in `docs/adr/` first.

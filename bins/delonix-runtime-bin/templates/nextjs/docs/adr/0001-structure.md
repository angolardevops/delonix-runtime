# 0001 — App Router BFF, ports declared by their consumer

Status: accepted (template default)

## Context
The application has pages and a small HTTP API for them and for webhooks.
Next.js already provides routing, rendering and request handling; the
business rule needs a place that does not depend on it.

## Decision
Pages and Route Handlers live under `app/`. The capability lives in
`lib/notes` with the ports it needs; adapters and infrastructure in
`lib/server`; the shared HTTP behaviour (error shape, request id, limits) in
`lib/http/route.ts`. One composition root (`lib/server/runtime.ts`) builds the
adapters at startup and is kept on `globalThis`, because Next.js bundles
`instrumentation.ts` and the routes separately. Dependency direction is a
test (`tests/architecture.test.ts`), not a convention.

## Alternatives
- Business logic inside Route Handlers: fewer files, but the rules could not
  be tested or reused by the pages without HTTP.
- Server Actions instead of Route Handlers: fine for page-only mutations; the
  note creation is also a public, versioned HTTP contract and a webhook target.
- A custom Node server (`server.js` with `next()`): gives full control of the
  HTTP server, but cannot be combined with `output: "standalone"`.

## Trade-off
A `globalThis` singleton and errors matched by a `kind` field are less
elegant than module state and `instanceof`; they are what works across the
bundles of one Next.js server.

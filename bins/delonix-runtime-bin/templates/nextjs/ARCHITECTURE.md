# Architecture

A Next.js application with its backend-for-frontend, holding one capability.
The shape is ports and adapters inside the server half of the App Router:
interfaces are declared by the module that USES them.

```
 browser ──► app/(list)/note-form.tsx (Client Component) ── fetch ──┐
                                                                    ▼
 browser ──► app/(list)/page.tsx (Server Component)      app/api/v1/**/route.ts
                     │                                     lib/http/route.ts
                     └────────────► getRuntime() ◄──────────────────┘
                                         │
              lib/server/runtime.ts (composition root, on globalThis)
                                         │
                 lib/notes  ◄── lib/server/memory-note-store.ts (NoteStore)
              (rules, ports) ◄── lib/server/webhooks/dispatcher.ts (NotePublisher)

 instrumentation.ts ─► lib/server/boot.ts: config → telemetry → runtime → signals → ready
```

## What runs where
- **Server only**: everything under `lib/server`, `lib/notes`, `lib/http`,
  every `route.ts`, `instrumentation.ts`, and the Server Components. They
  import `server-only`, so importing one from a Client Component fails the
  build. Secrets and configuration are read here.
- **Browser**: `app/(list)/note-form.tsx` and `app/error.tsx` (`"use client"`).
  They receive no secret and talk to the server through `/api/v1`.
- The list page calls the use case in-process. It does not `fetch` its own
  Route Handler: that would be an extra HTTP hop to the same process.

## Dependency rules (enforced by `tests/architecture.test.ts`)
- `lib/notes` imports only its own files and the `server-only` marker. It
  owns `NoteStore` and `NotePublisher`.
- `lib/server` never imports `lib/http` or `app/`.
- Inside `lib/`, only `lib/server/boot.ts` imports Next.js (for `after`).
- `lib/server/config.ts` imports no other server module.
- Route Handlers get adapters from the runtime; they never construct one.
- Client Components import nothing from `lib/server`, `lib/notes`, `lib/http`.
- No source file reads a `NEXT_PUBLIC_` variable.
- Telemetry reaches the use cases by decoration (`TracedNotes`), not by
  imports inside `lib/notes`.

Proof the gate works: add `import { after } from "next/server";` to
`lib/notes/notes.ts` and run `pnpm test` — it fails naming the import.

## One process, several bundles
Next.js compiles `instrumentation.ts` and the routes into separate bundles,
so a module can be instantiated more than once in the same process. Three
consequences shape the code:
- the runtime and the request-context storage are kept on `globalThis`
  (`Symbol.for(...)` keys), so the signal handler drains the same objects the
  routes use;
- use-case errors are recognised by a `kind` field, not `instanceof`;
- `after` is imported in `boot.ts` and reaches the routes through the runtime.

## Request path of the example
`POST /api/v1/notes` → Next.js opens the request span and calls the handler →
`route()` assigns the request id, counts the request in flight → the handler
reads the body with a size limit and decodes it strictly (unknown fields
refused) → `TracedNotes.create` opens the use-case span →
`NotesService.create` validates, calls `NoteStore.save`, then
`NotePublisher.noteCreated` → the dispatcher registers the delivery with
`after()` and returns → the handler answers 201 → after the response is sent,
the delivery runs with retries as a child span of the same trace, and the
request stops counting as in flight.

## Extending: add `DELETE /api/v1/notes/{id}`
1. `lib/notes/notes.ts`: add `delete(id)` to `NoteStore` and to `NotesApi`,
   and a `NotesService.delete` use case (throw `NoteNotFoundError` when absent).
2. `lib/server/memory-note-store.ts`: implement `delete`;
   `lib/server/traced-notes.ts`: add the traced method.
3. `app/api/v1/notes/[id]/route.ts`: `export const DELETE = route(...)`
   answering 204.
4. `api/openapi.yaml`: document `delete` under `/api/v1/notes/{id}`.
5. UI: a small Client Component with a button on `app/notes/[id]/page.tsx`
   that calls the route and navigates back.
6. Tests: a use-case test and a Route Handler test; `pnpm test` runs the
   drift and architecture gates.

## A second transport
gRPC is not generated. This application is a BFF for its own pages; another
service that needs the notes capability should get its own backend service
(the `node`, `nestjs` or `go` templates) rather than a second transport here.
If one is needed anyway, it would call `getRuntime().notes` exactly as the
Route Handlers do.

## Why not more
No ORM, no state library, no DI container, no Server Actions: one capability
with one form does not need them. Server Actions would be the idiomatic next
step for mutations that only the pages use; the Route Handler exists here
because the same operation is also the public `/api/v1` contract.

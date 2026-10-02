# Architecture

A modular monolith with one capability, in Fastify's own shape: one
encapsulated plugin per concern, JSON schemas on every route, and the use
cases in a plain module the plugins receive as an option.

```
 src/instrumentation.ts  (loaded first: OpenTelemetry)
 src/index.ts            composition root: config · logger · adapters · signals
        │
        ▼
 src/app.ts ── plugins ──► health/routes.ts
        │                  notes/routes.ts ────► notes/notes.ts ◄── notes/memory-store.ts (NoteStore)
        │                  webhooks/routes.ts ──┘      ▲
        │                                              └── webhooks/dispatcher.ts (NotePublisher)
        └── http/errors.ts (one error shape)
```

## Dependency rules (enforced by `test/architecture.test.ts`)
- `notes/notes.ts` imports only `node:crypto`. It owns the `NoteStore` and
  `NotePublisher` ports.
- Route plugins reach the use cases through `NotesUseCases`, never a store.
- Adapters (`memory-store`, `dispatcher`) implement ports; they never import
  the transport.
- `config.ts` imports nothing from the capability or the transport.
- Telemetry reaches the use cases by wrapping them (`telemetry/traced-notes.ts`),
  not by imports inside `notes.ts`.

Proof the gate works: add `import "fastify";` to `src/notes/notes.ts` and run
`pnpm test` — the architecture test fails naming the import.

## Request path of the example
`POST /api/v1/notes` → Fastify validates the body against the route schema
(types, unknown fields) → `tracedNotes.create` opens the use-case span →
`NotesService.create` trims and validates the rules, calls `NoteStore.save`,
then `NotePublisher.noteCreated` → `WebhookDispatcher` queues and returns →
the route answers 201. The dispatcher later delivers with retries, as a child
span of the same trace.

## Extending: add `DELETE /api/v1/notes/:id`
1. `notes/notes.ts`: add `delete(id)` to `NoteStore` and a `NotesService.delete`
   use case (throw `NotFoundError` when absent); add it to `NotesUseCases`.
2. `notes/memory-store.ts`: implement `delete`.
3. `notes/routes.ts`: add the route with its schema; `telemetry/traced-notes.ts`
   gets the traced method.
4. `pnpm openapi:write` to regenerate `api/openapi.json`.
5. Tests: a use-case test and an `app.inject` test; `pnpm check`.

## Where a second transport would plug in
A gRPC or message-consumer transport is another module that receives the same
`NotesUseCases` in `src/index.ts`. None is generated (docs/adr/0001).

## Why not more
No DI container, no generic repository, no event bus: one capability does not
need them. Fastify's plugin encapsulation is the module boundary.

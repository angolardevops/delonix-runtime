# Architecture

A modular monolith with one capability, in NestJS's own shape: a module per
domain, controllers as transport, an injectable service holding the use cases,
and ports as abstract classes that double as injection tokens.

```
        src/main.ts ── lifecycle (listen, signals, readiness, shutdown)
              │
        src/app.setup.ts ── request id · access log · body parser (raw bytes kept)
              │
        AppModule.forRoot(config) ── global: exception filter, validation pipe, route interceptor
              │
   ┌──────────┴───────────────┬───────────────────────────┬──────────────┐
   ▼                          ▼                           ▼              ▼
 NotesModule            InboundWebhookModule       OutboundWebhookModule  HealthModule
 NotesController ──┐    InboundWebhookController   provides NotePublisher ReadinessService
                   ▼            │                        ▲
              NotesService ◄────┘ (same use case)        │ implements
              (TracedNotesService)                       │
                   │ depends on ports                    │
                   ├──► NoteRepository  ◄── InMemoryNoteRepository
                   └──► NotePublisher   ◄── WebhookDispatcher / NoopNotePublisher
```

## Dependency rules (enforced by `src/architecture.spec.ts`)
- The use cases and ports (`note.ts`, `notes.service.ts`, `note.repository.ts`,
  `note.publisher.ts`) import only `@nestjs/common` (for `@Injectable`),
  `node:crypto` and each other. No Express, no OpenTelemetry, no adapter, no DTO.
- Controllers reach the use cases through `NotesService`, never a repository.
- Adapters implement ports; they do not import the HTTP transport.
- `config` and `logging` import no feature; `health` does not import features.
- Telemetry reaches the use cases by decoration: `NotesModule` provides
  `NotesService` as `TracedNotesService` (a subclass that opens a span and
  calls `super`), so the rules never import OpenTelemetry.

Framework concession: the use-case class carries `@Injectable()`. That
decorator is Nest's DI marker and nothing else; keeping it avoids a factory
provider per use case, and the architecture test allows exactly that import.

Proof the gate works: add `import { trace } from "@opentelemetry/api";` to
`src/notes/notes.service.ts` and run `pnpm test:unit` — `architecture` fails
naming the file and the import.

## Request path of the example
`POST /api/v1/notes` → `requestIdMiddleware` and the access log (Express
middleware, before the body parser) → JSON body parser (size limit, raw bytes
kept) → `RequestValidationPipe` turns the body into `CreateNoteDto` (shape:
strings, no unknown properties) → `NotesController.create` →
`TracedNotesService.create` opens the `notes.create` span →
`NotesService.create` applies the rules, calls `NoteRepository.save`, then
`NotePublisher.noteCreated` → `WebhookDispatcher` queues and returns → 201
with `Location`. The dispatcher later delivers with retries, as a child span
of the same trace. Any error, from any layer, reaches `AllExceptionsFilter`,
which answers the one error shape.

Validation is split on purpose: DTOs check the shape of an HTTP body; the
rules (trimmed title, lengths, page bounds) live in the use case, because the
inbound webhook runs the same `create` without a DTO.

## Extending: add `DELETE /api/v1/notes/{id}`
1. `note.repository.ts`: add `abstract delete(id: string): Promise<boolean>`.
2. `notes.service.ts`: add `delete(id)`; throw `NoteNotFoundError` when the
   repository reports nothing was deleted.
3. `in-memory-note.repository.ts`: implement `delete`.
4. `src/telemetry/traced-notes.service.ts`: override `delete` with a span.
5. `notes.controller.ts`: `@Delete(":id") @HttpCode(204)` with its
   `@ApiNoContentResponse` / `@ApiNotFoundResponse`.
6. `pnpm openapi:write` and commit `api/openapi.json`.
7. Tests: a case in `notes.service.spec.ts` and one in `test/notes.e2e-spec.ts`;
   `pnpm check` runs the drift and architecture gates.

## A second transport (gRPC, a queue consumer)
Not generated. It would be another controller (or a Nest microservice
transport) in its own module, importing `NotesModule` and calling
`NotesService` — exactly what `InboundWebhookModule` does today. The use cases
do not change.

## Why not more
No ORM, no CQRS module, no generic repository, no event bus, no config
library: one capability does not need them. Add a layer when a second
capability shares something real — see docs/adr/0001.

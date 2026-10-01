# Architecture

A modular monolith with one capability. The shape is ports and adapters in
FastAPI's own idiom: routers per capability, dependencies (`Depends`) to hand
the use cases to the routes, `Protocol`s for the ports, and one composition
root. No dependency-injection container.

```
        ┌────────────── server.py (process: config, logging, telemetry, signals) ─────────┐
        │                       app.py (composition root, lifespan)                       │
        └──────┬──────────────────────────┬───────────────────────────────┬───────────────┘
               ▼                          ▼                               ▼
 HTTP ─► web/ middleware ─► notes/api.py ─► observability/traced.py ─► notes/service.py
          (request id,       (schemas,        (one span per use case)     │ uses ports.py
           body limit,        Depends)                                    ▼
           errors)         webhooks/api.py ─────────────────────► NoteStore ◄── notes/memory.py
                           (inbound, verify)                      NotePublisher ◄── webhooks/dispatcher.py
```

## Dependency rules (enforced by `tests/test_architecture.py`)
- `notes/domain.py`, `notes/ports.py`, `notes/service.py` import the standard
  library and each other, nothing else: no FastAPI, Pydantic, httpx,
  OpenTelemetry, settings. They own `NoteStore` and `NotePublisher`.
- `notes/memory.py` (adapter) imports only the domain.
- `webhooks/signature.py`, `webhooks/dedup.py` and `lifecycle.py` are standard
  library only.
- Only `app.py` names the storage adapter; routers reach the use cases through
  the `NotesUseCases` protocol and the `get_notes` dependency.
- Telemetry reaches the use cases by decoration (`observability.traced.TracedNotes`),
  not by imports inside `notes/service.py`.

Concession to the framework: request and response schemas are Pydantic models
in `notes/api.py` (that is how FastAPI validates and documents), while the
domain uses plain dataclasses; the router maps between them.

Proof the gate works: add `import fastapi` to `src/__MODULE__/notes/service.py`
and run `make test` — `test_architecture.py` fails naming the import.

## Request path of the example
`POST /api/v1/notes` → OpenTelemetry server span → `RequestContextMiddleware`
(request id, access log) → `BodyLimitMiddleware` → FastAPI validates the body
into `NoteIn` (unknown fields refused) → `create_note` → `TracedNotes.create`
opens the use-case span → `NoteService.create` validates the rules, calls
`NoteStore.save`, then `NotePublisher.note_created` → `WebhookDispatcher`
queues and returns → the route answers 201. The dispatcher's worker task
delivers later, with retries, as a child span of the same trace.

## Extending: add `DELETE /api/v1/notes/{id}`
1. `notes/ports.py`: add `async def delete(self, note_id: str) -> bool` to `NoteStore`.
2. `notes/service.py`: a `delete` use case raising `NoteNotFoundError` when absent.
3. `notes/memory.py`: implement `delete`.
4. `observability/traced.py` and the `NotesUseCases` protocol in `notes/api.py`:
   add the method; then the route (`@router.delete("/{note_id}", status_code=204,
   responses=NOT_FOUND)`).
5. `make openapi`, and commit `api/openapi.json` with the change.
6. Tests: a use-case test and an HTTP test; `make check` runs the drift and
   architecture gates.

## Where a second transport would plug in
A gRPC server, a CLI or a queue consumer is another caller of `NoteService`:
build it in `app.py` (or a sibling composition root) from the same store and
publisher, and start/stop it in the lifespan. Nothing in `notes/` changes.

## Why not more
No DI container, no generic repository, no event bus, no `services/` and
`repositories/` layers: one capability does not need them. Add a layer when a
second capability shares something real — see docs/adr/0001.

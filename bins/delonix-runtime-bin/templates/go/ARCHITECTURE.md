# Architecture

A modular monolith with one capability. The shape is ports and adapters, kept
as small as Go allows: interfaces are declared by the package that USES them.

```
            ┌──────────── cmd/__NAME__ (composition root) ────────────┐
            │ config · logging · telemetry · adapters · signals       │
            └───────┬─────────────────────┬───────────────────────────┘
                    ▼                     ▼
 HTTP ──► internal/httpapi ──► internal/notes ◄── internal/memstore (Store)
                    │    (Notes iface)   │   ▲
                    │                    └───┴── internal/webhook (Publisher)
                    └──► internal/webhook (inbound verification)
```

## Dependency rules (enforced by `internal/archtest`)
- `notes` imports only the standard library (minus `net/http`). It owns `Store`
  and `Publisher`.
- `httpapi` depends on `notes` through its own small `Notes` interface, never on
  a storage adapter.
- Adapters (`memstore`, `webhook`) implement ports; they never call the transport.
- `config` imports no internal package.
- Telemetry reaches the use cases by decoration (`telemetry.TracedNotes`), not
  by imports inside `notes`.

Proof the gate works: add `_ "net/http"` to `internal/notes/notes.go` and run
`go test ./internal/archtest` — it fails naming the import.

## Request path of the example
`POST /api/v1/notes` → `httpapi.createNote` decodes strictly (unknown fields
and oversized bodies refused) → `TracedNotes.Create` opens the use-case span →
`notes.Service.Create` validates, calls `Store.Save`, then `Publisher.NoteCreated`
→ `webhook.Dispatcher` queues and returns → handler answers 201. The
dispatcher later delivers with retries, as a child span of the same trace.

## Extending: add `DELETE /api/v1/notes/{id}`
1. `notes`: add `Delete(ctx, id) error` to `Store`, a `Service.Delete` use case
   (return `ErrNotFound` when absent).
2. `memstore`: implement `Delete`.
3. `httpapi`: add `Delete` to the `Notes` interface, a handler, and a route in
   `Routes`; `telemetry.TracedNotes` gets the traced method.
4. `api/openapi.yaml`: document `delete` under `/api/v1/notes/{id}`.
5. Tests: a use-case test and an HTTP test; `make race` runs the drift and
   architecture gates.

## Why not more
No framework, no DI container, no generic repository, no event bus: one
capability does not need them. Add a layer when a second capability shares
something real — see docs/adr/0001.

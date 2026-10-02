# Architecture

A Laravel application with one capability, arranged as ports and adapters
inside Laravel's own directories. The capability is plain PHP; Laravel is
the transport, the persistence and the queue around it.

```
                 app/Providers/AppServiceProvider (composition root)
                                    │ binds ports to adapters
 HTTP ─► app/Http ──────────► app/Notes ◄──────── app/Persistence (NoteRepository)
   routes/api.php        Notes (interface)             └─ app/Models (Eloquent)
   controllers           NoteService (use cases)
   Form Requests         NoteRepository, NotePublisher (ports)
   ErrorRenderer                 ▲                ▲
        │                        │                └─ app/Webhooks/QueuedNotePublisher
        └─► app/Webhooks ────────┘                      └─ app/Jobs/DeliverWebhook ─► receiver
            (inbound verification, InboundProcessor)         (run by `queue:work`)
 app/Telemetry decorates Notes (TracedNotes) and the HTTP client; app/Logging formats.
```

## Dependency rules (enforced by `tests/Architecture/DependencyDirectionTest.php`)
- `app/Notes` references nothing from `Illuminate\`, `Symfony\`, `OpenTelemetry\`
  or any other `App\` namespace. It owns `NoteRepository` and `NotePublisher`.
- `app/Http` reaches the use cases through `App\Notes\Notes`; never a model,
  a repository or a job.
- Adapters (`Persistence`, `Models`, `Webhooks`, `Jobs`) implement ports; they
  never call the transport.
- Telemetry reaches the use cases by decoration (`App\Telemetry\TracedNotes`),
  not by imports inside `app/Notes`.

Proof the gate works: add `use Illuminate\Support\Facades\DB;` to
`app/Notes/NoteService.php` and run `composer test` — it fails naming the file
and the reference.

## The Laravel concessions, stated
- **Eloquent is the persistence**, not an in-memory store: models and
  migrations are how a Laravel team stores things. It stays behind the
  `NoteRepository` port, so the use cases are tested without a database
  (`tests/Unit/NoteServiceTest.php`) and the mapping lives in one class.
- **Validation happens twice on purpose**: the Form Request gives HTTP callers
  Laravel's per-field messages; `Note::create` enforces the same rules for
  callers that are not HTTP (the inbound webhook).
- **`InboundProcessor` uses `DB::transaction` and a model directly**: the
  receipt and the effect must commit together, and that is an adapter concern.

## Request path of the example
`POST /api/v1/notes` → global middleware (`AssignRequestId`, `TraceRequest`,
`LogRequest`) → route middleware (`body.limit`, `json.object`) →
`StoreNoteRequest` validates → `NoteController::store` → `TracedNotes::create`
opens the use-case span → `NoteService::create` builds the `Note`, calls
`NoteRepository::save`, then `NotePublisher::noteCreated` →
`QueuedNotePublisher` inserts a `DeliverWebhook` job (trace context inside)
and returns → 201. Later the worker runs the job: signs, posts, retries.

## Extending: add `DELETE /api/v1/notes/{id}`
1. `app/Notes`: add `delete(string $id): void` to `NoteRepository` and to the
   `Notes` interface; implement it in `NoteService` (throw `NoteNotFound`).
2. `app/Persistence/EloquentNoteRepository`: implement `delete`.
3. `app/Telemetry/TracedNotes`: add the traced method (`notes.delete`).
4. `app/Http/Controllers/Api/V1/NoteController`: a `destroy` action answering
   204; `routes/api.php`: `Route::delete('notes/{id}', …)`.
5. `api/openapi.yaml`: document `delete` under `/api/v1/notes/{id}`.
6. Tests: a case in `tests/Unit/NoteServiceTest.php` and one in
   `tests/Feature/NotesApiTest.php`; `composer check` runs the contract and
   architecture gates.

## A second transport
gRPC, a console command or a queue consumer would be another caller of
`App\Notes\Notes`, next to `app/Http` — the use cases do not change. None is
generated; see docs/adr/0001.

## Why not more
No repository-per-model base class, no service layer per controller, no DTO
library, no event bus: one capability does not need them. Add a layer when a
second capability shares something real.

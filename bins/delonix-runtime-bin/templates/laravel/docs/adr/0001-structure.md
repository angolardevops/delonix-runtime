# 0001 — A plain-PHP capability inside Laravel's directories

Status: accepted (template default)

## Context
Laravel's default shape puts rules in controllers and models. That is quick,
and it makes the rules untestable without HTTP and a database, and lets any
class reach any other.

## Decision
Keep Laravel's layout (`app/Http`, `app/Models`, `app/Jobs`, `routes/`,
`database/migrations`) and add one directory, `app/Notes`, holding the
capability as plain PHP: the `Note` value, the use cases (`NoteService`
behind the `Notes` interface) and the two ports it needs (`NoteRepository`,
`NotePublisher`). Eloquent, the queue and telemetry are adapters bound in
`AppServiceProvider`. The direction of dependencies is a test, not a
convention.

## Alternatives
- **Fat models / controller logic** (stock Laravel): fewer files; rules tied
  to the ORM and the request cycle.
- **Full DDD layout** (`src/Domain`, `Application`, `Infrastructure`): fights
  the framework's generators and what Laravel developers expect to find.
- **Action classes per use case** (`CreateNote`, `ListNotes`…): equally
  idiomatic; one small service keeps the decorator for tracing to one class.

## Trade-off
A mapping step between `App\Models\Note` (row) and `App\Notes\Note` (value),
and validation written twice (Form Request and `Note::create`). In exchange
the use cases run in milliseconds without a database, and the inbound
webhook reuses them without going through HTTP.

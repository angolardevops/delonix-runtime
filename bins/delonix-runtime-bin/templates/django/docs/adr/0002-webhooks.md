# 0002 — Standard Webhooks; durable inbound de-duplication, in-memory outbound queue

Status: accepted (template default)

## Context
The service both receives and sends events. A home-made signature scheme
would force every peer to read this code. The service has a database, so
some delivery state can be durable at no extra moving part.

## Decision
- The Standard Webhooks format (HMAC-SHA256 over `id.timestamp.body`,
  `whsec_` secrets, 5-minute tolerance), verified over `request.body` (the
  raw bytes).
- **Inbound** de-duplication is a table (`webhooks.InboundDelivery`, unique
  `webhook_id`) written in the same transaction as the processing. It holds
  across restarts and replicas; rows older than twice the replay window are
  deleted on the way.
- **Outbound** delivery is one background thread per gunicorn worker with a
  bounded in-memory queue, `httpx` with a 5 s timeout, up to 4 attempts and
  full-jitter backoff. The event is queued from `transaction.on_commit`, so a
  rolled-back note is never announced. At-least-once within a process
  lifetime, at-most-once across a restart; the note id is the idempotency
  key.

## Alternatives
- Transactional outbox: an `Outbox` row written with the note, and a
  separate worker process (a management command) that delivers and marks
  rows. Required when an event must survive a crash. It needs a second
  process to deploy and monitor, which this template does not ship.
- A task queue (Celery, django-tasks with a database backend): the same
  durability with more machinery; reasonable once there is a second kind of
  background work.

## Trade-off
Events queued in a worker that is killed (SIGKILL, OOM, a shutdown longer
than `SHUTDOWN_TIMEOUT`) are lost, and a retry sleeping in backoff delays
the events behind it in that worker. Move to the outbox before relying on
delivery.

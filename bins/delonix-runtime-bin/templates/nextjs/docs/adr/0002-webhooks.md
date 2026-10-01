# 0002 — Standard Webhooks, in-memory delivery state, delivery after the response

Status: accepted (template default)

## Context
The application both receives and sends events. A home-made signature scheme
would force every peer to read this code. The outbound call must not delay
the response that created the note.

## Decision
Use the Standard Webhooks format (HMAC-SHA256 over `id.timestamp.body`,
`whsec_` secrets, 5-minute tolerance). The inbound Route Handler verifies over
the raw bytes before parsing. The outbound delivery is scheduled with
`after()` from `next/server`, runs on the server with `fetch`, a timeout per
attempt and bounded full-jitter retries, and carries the request's trace
context. Inbound de-duplication and pending deliveries live in memory;
delivery is at-least-once within a process lifetime, with the note id as
idempotency key.

## Alternatives
- Durable outbox: a table written in the same transaction as the note, and a
  worker that delivers from it. Required when an event must survive a
  restart; it needs the persistence this template does not ship.
- A message broker: more moving parts than one outbound URL justifies.
- Awaiting the delivery inside the request: simpler, but the receiver's
  latency and failures become the user's.

## Trade-off
Deliveries pending at a crash are lost, and de-duplication does not span
replicas or restarts. On a serverless platform `after()` depends on the
platform's `waitUntil`; this template is measured on a long-lived Node
server only. When persistence is added, move both to the database (a unique
key on `webhook-id`, an outbox table) before relying on them.

# ADR 0002 — Webhooks in the Standard Webhooks format, delivered off the request path

## Status
Accepted.

## Context
The service both receives and sends webhooks. Receiving needs a signature scheme
that resists replay and tampering; sending must never make a slow or dead
receiver slow down (or fail) the request that triggered the event.

## Decision
Use the published Standard Webhooks format (`webhook-id`, `webhook-timestamp`,
`webhook-signature`, `v1,base64(HMAC-SHA256(key, "id.ts.body"))`) so a sender or
receiver in any language interoperates. Verify over the RAW body bytes, bound
the timestamp to a tolerance window, and deduplicate accepted delivery ids in
memory so a sender's retry is acknowledged, not processed twice.

Send from a background dispatcher: `Publisher::note_created` only enqueues on a
bounded queue; a worker delivers with capped-backoff retries. A full queue drops
and logs rather than blocking. The webhook-id is the note id, so a receiver that
deduplicates by it sees each note once however many attempts it took.

## Consequences
Delivery is at-least-once while the process lives and at-most-once across a
restart (the queue is memory). A deployment that must not lose an event needs an
outbox table written in the same transaction as the note — out of scope for the
in-memory reference, but the port boundary makes it a drop-in.

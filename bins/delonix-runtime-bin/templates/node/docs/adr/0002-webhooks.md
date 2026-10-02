# 0002 — Standard Webhooks, in-memory delivery state

Status: accepted (template default)

## Context
The service both receives and sends events. A home-made signature scheme would
force every peer to read this code.

## Decision
Use the Standard Webhooks format (HMAC-SHA256 over `id.timestamp.body`,
`whsec_` secrets, 5-minute tolerance). Inbound de-duplication and the outbound
queue live in memory; outbound delivery is at-least-once within a process
lifetime, with the note id as idempotency key.

## Alternatives
- Durable outbox: a table written in the same transaction as the note, and a
  worker that delivers from it. Required when an event must survive a restart;
  it needs the persistence this template does not ship.
- A message broker: more moving parts than one outbound URL justifies.

## Trade-off
Events queued at a crash are lost, and de-duplication does not span replicas
or restarts. When persistence is added, move both to the database (a unique
key on `webhook-id`, an outbox table) before relying on them.

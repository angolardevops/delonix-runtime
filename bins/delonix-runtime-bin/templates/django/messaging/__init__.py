"""Asynchronous messaging over RabbitMQ (AMQP 0-9-1, via pika).

A worked pub/sub example around the same `note.created` event the webhooks
send: `publisher.Publisher` publishes it to a topic exchange, and the
`consume_events` management command reads it back from a queue — a stand-in for
the asynchronous side-effects a real service would run (projections, search
indexing, notifications).

OPTIONAL and graceful: with no RABBITMQ_URL the publisher is never built (the
`note_created` signal handler is a no-op) and the consumer command reports it
has nothing to do. The publisher mirrors the webhook dispatcher — a bounded
queue drained by one background thread, dropping instead of blocking when full
— and the consumer uses manual acknowledgements, a capped number of retries
and a dead-letter queue, so a poison message cannot loop for ever.

Delivery is AT-LEAST-ONCE: the consumer acknowledges only after the handler
returns, so a crash mid-handling redelivers. It is NOT exactly-once — make the
handler idempotent (the event carries the note id as a natural key).
"""

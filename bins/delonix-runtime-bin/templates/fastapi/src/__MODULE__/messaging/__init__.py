"""Asynchronous messaging over RabbitMQ (AMQP 0-9-1, via aio-pika).

A worked pub/sub example around the same `note.created` event the webhooks
send: `publisher.RabbitPublisher` publishes it to a topic exchange, and
`consumer.RabbitConsumer` reads it back from a queue — a stand-in for the
asynchronous side-effects a real service would run (projections, search
indexing, notifications).

OPTIONAL and graceful: with no `RABBITMQ_URL` neither is wired and the app
starts and serves normally. Both are bounded — a publish queue that drops
instead of blocking the request, a consumer with a prefetch limit, manual
acknowledgements, a capped number of retries and a dead-letter queue for
messages that never succeed, so a poison message cannot loop for ever.

Delivery is AT-LEAST-ONCE: the consumer acknowledges only after the handler
returns, so a crash mid-handling redelivers. It is NOT exactly-once — make the
handler idempotent (the event carries the note id as a natural key).
"""

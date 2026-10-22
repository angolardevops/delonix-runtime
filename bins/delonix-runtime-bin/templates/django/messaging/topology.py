"""The AMQP topology, declared the same way by the publisher and the consumer.

One durable topic exchange carries `note.*`. The work queue dead-letters to a
separate exchange whose only binding is the dead-letter queue, so a message the
consumer rejects (poison, or retries exhausted) is kept for inspection instead
of being dropped or requeued into a loop. Declarations are idempotent: whoever
connects first creates them, the other finds them.

`pika` is imported here and in the publisher/consumer glue only — never at
module import time — so the app and its tests run without the AMQP client when
RABBITMQ_URL is unset.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import pika

EXCHANGE = "notes.events"
QUEUE = "notes.created"
BINDING_KEY = "note.*"

DLX = "notes.events.dlx"
DLQ = "notes.created.dead"


def declare_exchange(channel: pika.adapters.blocking_connection.BlockingChannel) -> None:
    """Just the topic exchange — all the publisher needs. The consumer declares
    the full topology, so declaring only the exchange here keeps the publisher
    from needing the queue."""
    channel.exchange_declare(EXCHANGE, exchange_type="topic", durable=True)


def declare_topology(channel: pika.adapters.blocking_connection.BlockingChannel) -> None:
    """Declare the exchange, the dead-letter path and the work queue, and bind
    them. Idempotent — safe to call on every connect."""
    channel.exchange_declare(EXCHANGE, exchange_type="topic", durable=True)
    channel.exchange_declare(DLX, exchange_type="fanout", durable=True)
    channel.queue_declare(DLQ, durable=True)
    channel.queue_bind(DLQ, DLX)
    channel.queue_declare(QUEUE, durable=True, arguments={"x-dead-letter-exchange": DLX})
    channel.queue_bind(QUEUE, EXCHANGE, routing_key=BINDING_KEY)

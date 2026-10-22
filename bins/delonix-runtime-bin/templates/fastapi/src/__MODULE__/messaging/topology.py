"""The AMQP topology, declared the same way by the publisher and the consumer.

One durable topic exchange carries `note.*`. The work queue dead-letters to a
separate exchange whose only binding is the dead-letter queue, so a message the
consumer rejects (poison, or retries exhausted) is kept for inspection instead
of being dropped or requeued into a loop. Declarations are idempotent: whoever
connects first creates them, the other finds them.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import aio_pika

EXCHANGE = "notes.events"
QUEUE = "notes.created"
BINDING_KEY = "note.*"

DLX = "notes.events.dlx"
DLQ = "notes.created.dead"


async def declare_topology(channel: aio_pika.abc.AbstractChannel) -> tuple[Any, Any]:
    """Declare the exchange, the dead-letter path and the work queue, and bind
    them. Returns `(exchange, queue)`."""
    import aio_pika

    exchange = await channel.declare_exchange(EXCHANGE, aio_pika.ExchangeType.TOPIC, durable=True)
    dlx = await channel.declare_exchange(DLX, aio_pika.ExchangeType.FANOUT, durable=True)
    dead = await channel.declare_queue(DLQ, durable=True)
    await dead.bind(dlx)

    queue = await channel.declare_queue(
        QUEUE, durable=True, arguments={"x-dead-letter-exchange": DLX}
    )
    await queue.bind(exchange, routing_key=BINDING_KEY)
    return exchange, queue


async def declare_exchange(channel: aio_pika.abc.AbstractChannel) -> Any:
    """Just the topic exchange — all the publisher needs. Declaring the full
    topology here too means the queue exists even if the consumer starts last,
    so a message published before the consumer connects is not lost."""
    exchange, _ = await declare_topology(channel)
    return exchange

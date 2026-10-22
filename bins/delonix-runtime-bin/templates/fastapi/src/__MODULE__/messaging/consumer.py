"""Consume `note.created` from RabbitMQ, with manual acks and a bounded retry.

The per-message decision — acknowledge, retry, or dead-letter — lives in
`RabbitConsumer.handle`, which talks to an `_Incoming` message (ack / retry /
dead_letter), so it is tested with no broker. The aio-pika connect-and-consume
loop in `_run` is the thin glue that wraps each real delivery as an `_Incoming`.

Rules:
- the handler returns  -> ack (processed; at-least-once, so make it idempotent);
- the body is not a valid event -> dead-letter at once (a retry cannot fix it);
- the handler raises -> retry up to `max_retries` (republished with an
  incremented `x-retry` header), then dead-letter — a poison message can never
  loop for ever.
"""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Awaitable, Callable
from typing import Any, Protocol

from opentelemetry.metrics import Meter

from .events import decode

log = logging.getLogger(__name__)

#: Processes one decoded event. Raising means "transient failure, retry".
Handler = Callable[[dict[str, Any]], Awaitable[None]]


class _Incoming(Protocol):
    """One delivery, abstracted from aio-pika so `handle` is broker-free."""

    @property
    def body(self) -> bytes: ...
    @property
    def retry_count(self) -> int:
        """The `x-retry` header, 0 on first delivery."""
        ...

    async def ack(self) -> None: ...
    async def dead_letter(self) -> None:
        """Reject without requeue — routed to the dead-letter exchange."""
        ...

    async def retry(self, delay: float) -> None:
        """Republish with `x-retry` incremented, then ack the original."""
        ...


async def log_handler(event: dict[str, Any]) -> None:
    """The default handler: records receipt. Replace it with the real
    side-effect (a projection, an index write, a notification)."""
    note_id = event.get("data", {}).get("id")
    log.info("note.created consumed", extra={"note_id": note_id})


class RabbitConsumer:
    def __init__(
        self,
        *,
        url: str,
        meter: Meter,
        handler: Handler = log_handler,
        max_retries: int = 3,
        retry_delay: float = 1.0,
        prefetch: int = 16,
    ) -> None:
        self._url = url
        self._handler = handler
        self._max_retries = max_retries
        self._retry_delay = retry_delay
        self._prefetch = prefetch
        self._outcomes = meter.create_counter(
            "rabbit.consumed", description="RabbitMQ messages by outcome"
        )
        self._task: asyncio.Task[None] | None = None
        self._closer: Callable[[], Awaitable[None]] | None = None

    async def handle(self, message: _Incoming) -> None:
        """Decide the fate of one delivery. Never raises: every path ends in an
        ack, a dead-letter, or a retry, so a delivery is never left unanswered."""
        try:
            event = decode(message.body)
        except ValueError as exc:
            self._count("poison")
            log.warning("dead-lettering an invalid message", extra={"error": str(exc)})
            await message.dead_letter()
            return
        try:
            await self._handler(event)
        except Exception as exc:  # a handler failure is the retry signal
            if message.retry_count >= self._max_retries:
                self._count("dead_lettered")
                log.error(
                    "handler failed, retries exhausted, dead-lettering",
                    extra={"error": type(exc).__name__, "retries": message.retry_count},
                )
                await message.dead_letter()
            else:
                self._count("retried")
                log.warning(
                    "handler failed, retrying",
                    extra={"error": type(exc).__name__, "attempt": message.retry_count + 1},
                )
                await message.retry(self._retry_delay)
            return
        self._count("acked")
        await message.ack()

    def _count(self, outcome: str) -> None:
        self._outcomes.add(1, {"outcome": outcome})

    # ---- aio-pika glue (not unit-tested; exercised by the CI integration run) ----

    def start(self) -> None:
        self._task = asyncio.create_task(self._run(), name="rabbit-consumer")

    async def close(self) -> None:
        if self._closer is not None:
            await self._closer()
            self._closer = None
        if self._task is not None:
            self._task.cancel()
            await asyncio.gather(self._task, return_exceptions=True)

    async def _run(self) -> None:
        import aio_pika

        from .topology import declare_topology

        try:
            connection = await aio_pika.connect_robust(self._url)
        except Exception as exc:  # a down broker must not crash startup
            log.warning(
                "rabbit consumer could not connect; messages will not be consumed",
                extra={"error": type(exc).__name__},
            )
            return
        self._closer = connection.close
        channel = await connection.channel()
        await channel.set_qos(prefetch_count=self._prefetch)
        exchange, queue = await declare_topology(channel)

        async def on_message(message: aio_pika.abc.AbstractIncomingMessage) -> None:
            await self.handle(_AioPikaIncoming(message, exchange))

        await queue.consume(on_message)
        log.info("rabbit consumer ready")
        await asyncio.Future()  # run until cancelled


class _AioPikaIncoming:
    """Adapts an aio-pika delivery to `_Incoming`."""

    def __init__(self, message: Any, exchange: Any) -> None:
        self._message = message
        self._exchange = exchange

    @property
    def body(self) -> bytes:
        body: bytes = self._message.body
        return body

    @property
    def retry_count(self) -> int:
        value = (self._message.headers or {}).get("x-retry", 0)
        try:
            return int(value)
        except (TypeError, ValueError):
            return 0

    async def ack(self) -> None:
        await self._message.ack()

    async def dead_letter(self) -> None:
        await self._message.reject(requeue=False)

    async def retry(self, delay: float) -> None:
        import aio_pika

        from .events import ROUTING_KEY

        await asyncio.sleep(delay)
        republished = aio_pika.Message(
            body=self._message.body,
            delivery_mode=aio_pika.DeliveryMode.PERSISTENT,
            headers={"x-retry": self.retry_count + 1},
        )
        await self._exchange.publish(republished, routing_key=ROUTING_KEY)
        await self._message.ack()

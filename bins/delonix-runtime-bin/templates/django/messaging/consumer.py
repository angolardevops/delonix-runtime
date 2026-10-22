"""Consume `note.created` from RabbitMQ, with manual acks and a bounded retry.

The per-message decision — acknowledge, retry, or dead-letter — lives in
`Consumer.handle`, which talks to an `_Incoming` message (ack / retry /
dead_letter), so it is tested with no broker. The pika connect-and-consume
loop in `run` is the thin glue that wraps each real delivery as an `_Incoming`.

Rules:
- the handler returns  -> ack (processed; at-least-once, so make it idempotent);
- the body is not a valid event -> dead-letter at once (a retry cannot fix it);
- the handler raises -> retry up to `max_retries` (republished with an
  incremented `x-retry` header), then dead-letter — a poison message can never
  loop for ever.
"""

from __future__ import annotations

import contextlib
import logging
import time
from collections.abc import Callable
from typing import Any, Protocol

from opentelemetry import metrics

from messaging.events import decode

log = logging.getLogger("messaging.consumer")
consumed = metrics.get_meter("__NAME__.messaging").create_counter(
    "rabbit.consumed", description="RabbitMQ messages by outcome"
)

#: Processes one decoded event. Raising means "transient failure, retry".
Handler = Callable[[dict[str, Any]], None]


class _Incoming(Protocol):
    """One delivery, abstracted from pika so `handle` is broker-free."""

    @property
    def body(self) -> bytes: ...
    @property
    def retry_count(self) -> int:
        """The `x-retry` header, 0 on first delivery."""
        ...

    def ack(self) -> None: ...
    def dead_letter(self) -> None:
        """Reject without requeue — routed to the dead-letter exchange."""
        ...

    def retry(self, delay: float) -> None:
        """Republish with `x-retry` incremented, then ack the original."""
        ...


def log_handler(event: dict[str, Any]) -> None:
    """The default handler: records receipt. Replace it with the real
    side-effect (a projection, an index write, a notification)."""
    note_id = event.get("data", {}).get("id")
    log.info("note.created consumed", extra={"note_id": note_id})


class Consumer:
    def __init__(
        self,
        url: str,
        *,
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

    def handle(self, message: _Incoming) -> None:
        """Decide the fate of one delivery. Never raises: every path ends in an
        ack, a dead-letter, or a retry, so a delivery is never left unanswered."""
        try:
            event = decode(message.body)
        except ValueError as exc:
            self._count("poison")
            log.warning("dead-lettering an invalid message", extra={"error": str(exc)})
            message.dead_letter()
            return
        try:
            self._handler(event)
        except Exception as exc:  # a handler failure is the retry signal
            if message.retry_count >= self._max_retries:
                self._count("dead_lettered")
                log.error(
                    "handler failed, retries exhausted, dead-lettering",
                    extra={"error": type(exc).__name__, "retries": message.retry_count},
                )
                message.dead_letter()
            else:
                self._count("retried")
                log.warning(
                    "handler failed, retrying",
                    extra={"error": type(exc).__name__, "attempt": message.retry_count + 1},
                )
                message.retry(self._retry_delay)
            return
        self._count("acked")
        message.ack()

    def _count(self, outcome: str) -> None:
        consumed.add(1, {"outcome": outcome})

    # ---- pika glue (not unit-tested; exercised by the CI integration run) ----

    def run(self) -> None:
        """Connect, declare the topology and consume until interrupted. A down
        broker is logged, not raised, so the command exits cleanly."""
        import pika

        from messaging.topology import QUEUE, declare_topology

        try:
            params = pika.URLParameters(self._url)
            connection = pika.BlockingConnection(params)
        except Exception as exc:  # a down broker must not crash the command
            log.warning(
                "rabbit consumer could not connect; nothing will be consumed",
                extra={"error": type(exc).__name__},
            )
            return
        try:
            channel = connection.channel()
            declare_topology(channel)
            channel.basic_qos(prefetch_count=self._prefetch)

            def on_message(ch: Any, method: Any, properties: Any, body: bytes) -> None:
                self.handle(_PikaIncoming(ch, method, properties, body))

            channel.basic_consume(QUEUE, on_message)
            log.info("rabbit consumer ready")
            channel.start_consuming()
        finally:
            with contextlib.suppress(Exception):  # best effort on shutdown
                connection.close()


class _PikaIncoming:
    """Adapts a pika delivery to `_Incoming`."""

    def __init__(self, channel: Any, method: Any, properties: Any, body: bytes) -> None:
        self._channel = channel
        self._method = method
        self._properties = properties
        self._body = body

    @property
    def body(self) -> bytes:
        return self._body

    @property
    def retry_count(self) -> int:
        headers = getattr(self._properties, "headers", None) or {}
        try:
            return int(headers.get("x-retry", 0))
        except (TypeError, ValueError):
            return 0

    def ack(self) -> None:
        self._channel.basic_ack(self._method.delivery_tag)

    def dead_letter(self) -> None:
        self._channel.basic_reject(self._method.delivery_tag, requeue=False)

    def retry(self, delay: float) -> None:
        import pika

        from messaging.events import ROUTING_KEY
        from messaging.topology import EXCHANGE

        time.sleep(delay)
        self._channel.basic_publish(
            exchange=EXCHANGE,
            routing_key=ROUTING_KEY,
            body=self._body,
            properties=pika.BasicProperties(delivery_mode=2, headers={"x-retry": self.retry_count + 1}),
        )
        self._channel.basic_ack(self._method.delivery_tag)

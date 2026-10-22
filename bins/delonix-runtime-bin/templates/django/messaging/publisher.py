"""Publish `note.created` to RabbitMQ without ever blocking a request.

Same shape as the outbound webhook dispatcher: a bounded queue and ONE
background thread per process. `publish` only enqueues (and returns); the
worker drains the queue to the broker, reopening the connection with capped
backoff on failure. A full queue drops (and logs, and counts) instead of
blocking the request that created the note.

The AMQP specifics live behind `_Sink`, injected through `opener`, so the
queue/worker/backoff logic is tested with a fake sink and no broker. The
default sink opens a pika topic exchange with publisher confirms and
persistent messages.

Delivery semantics match the webhook dispatcher: AT-LEAST-ONCE while the
process lives, AT-MOST-ONCE across a restart (the queue is in memory; a
graceful stop drains it within the shutdown budget), never exactly-once. The
event carries the note id, so a consumer that de-duplicates by it is safe.
"""

from __future__ import annotations

import contextlib
import logging
import queue
import random
import threading
import time
from collections.abc import Callable
from typing import Any, Protocol

from opentelemetry import metrics

from messaging.events import ROUTING_KEY, encode, envelope

log = logging.getLogger("messaging.publisher")
publishes = metrics.get_meter("__NAME__.messaging").create_counter(
    "rabbit.publishes", description="RabbitMQ publishes by outcome"
)

_STOP = object()


class _Sink(Protocol):
    """Where the worker publishes. The default is a pika exchange; a test
    passes a fake. `publish` raises on a broker error — the worker reopens."""

    def publish(self, routing_key: str, body: bytes) -> None: ...
    def close(self) -> None: ...


#: Opens a sink for a URL. Injected, so the worker is broker-free in tests.
Opener = Callable[[str], _Sink]


class _PikaSink:
    """The default sink: a persistent publish to the topic exchange, with
    publisher confirms so `publish` only returns once the broker acked."""

    def __init__(self, connection: Any, channel: Any) -> None:
        self._connection = connection
        self._channel = channel

    def publish(self, routing_key: str, body: bytes) -> None:
        import pika

        self._channel.basic_publish(
            exchange="notes.events",
            routing_key=routing_key,
            body=body,
            properties=pika.BasicProperties(delivery_mode=2, content_type="application/json"),
        )

    def close(self) -> None:
        with contextlib.suppress(Exception):  # best effort on shutdown
            self._connection.close()


def pika_opener(url: str) -> _Sink:
    import pika

    from messaging.topology import declare_exchange

    params = pika.URLParameters(url)
    params.socket_timeout = 5
    params.blocked_connection_timeout = 5
    connection = pika.BlockingConnection(params)
    channel = connection.channel()
    channel.confirm_delivery()
    declare_exchange(channel)
    return _PikaSink(connection, channel)


class Publisher:
    def __init__(
        self,
        url: str,
        *,
        opener: Opener = pika_opener,
        queue_size: int = 256,
        max_attempts: int = 5,
        base_backoff: float = 0.5,
        max_backoff: float = 10.0,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self._url = url
        self._opener = opener
        self._queue: queue.Queue[tuple[str, bytes] | object] = queue.Queue(maxsize=queue_size)
        self._max_attempts = max_attempts
        self._base_backoff = base_backoff
        self._max_backoff = max_backoff
        self._sleep = sleep
        self._lock = threading.Lock()
        self._thread: threading.Thread | None = None
        self._sink: _Sink | None = None
        self._closed = False

    def publish(self, event_type: str, msg_id: str, data: dict[str, Any]) -> None:
        """Queue one event; never blocks on the broker. Starts the worker
        lazily, in the process that serves (after the gunicorn fork)."""
        body = encode(envelope(event_type, data))
        with self._lock:
            if self._closed:
                self._count("dropped")
                return
            if self._thread is None:
                self._thread = threading.Thread(target=self._run, name="rabbit-publisher", daemon=True)
                self._thread.start()
        try:
            self._queue.put_nowait((ROUTING_KEY, body))
        except queue.Full:
            self._count("dropped")
            log.error("rabbit publish queue full, event dropped", extra={"note_id": msg_id})

    def close(self, timeout: float) -> bool:
        """Stop taking events and wait up to `timeout` for the queue to drain.
        Returns False when events were left unpublished."""
        with self._lock:
            if self._closed:
                return True
            self._closed = True
            thread = self._thread
        if thread is None:
            return True
        deadline = time.monotonic() + timeout
        while True:  # the sentinel must fit behind the queued jobs
            try:
                self._queue.put(_STOP, timeout=max(deadline - time.monotonic(), 0.01))
                break
            except queue.Full:
                if time.monotonic() >= deadline:
                    break
        thread.join(max(deadline - time.monotonic(), 0))
        drained = not thread.is_alive()
        if not drained:
            log.error("rabbit queue not drained at shutdown", extra={"events_left": self._queue.qsize()})
        return drained

    def _run(self) -> None:
        try:
            while True:
                job = self._queue.get()
                if job is _STOP:
                    return
                assert isinstance(job, tuple)
                self._publish(job)
        finally:
            if self._sink is not None:
                self._sink.close()
                self._sink = None

    def _publish(self, job: tuple[str, bytes]) -> None:
        """Publish `job`, reopening the sink with capped backoff between
        attempts. Bounded by `max_attempts` so a dead broker cannot wedge the
        worker on one event for ever (the event is dropped and counted, exactly
        like a full queue); the next event — or the `_STOP` sentinel at close —
        then gets its turn."""
        routing_key, body = job
        for attempt in range(1, self._max_attempts + 1):
            try:
                sink = self._ensure_sink()
                sink.publish(routing_key, body)
            except Exception as exc:  # any broker error reopens the connection
                self._drop_sink(exc)
                if attempt == self._max_attempts:
                    self._count("dropped")
                    log.error("rabbit publish failed, event dropped", extra={"attempts": attempt})
                    return
                self._count("failed")
                backoff = min(self._base_backoff * 2 ** (attempt - 1), self._max_backoff)
                self._sleep(random.uniform(0, backoff))  # noqa: S311 — jitter, not crypto
                continue
            self._count("published")
            return

    def _ensure_sink(self) -> _Sink:
        if self._sink is None:
            self._sink = self._opener(self._url)
        return self._sink

    def _drop_sink(self, exc: Exception) -> None:
        log.warning("rabbit sink error, reopening", extra={"error": type(exc).__name__})
        if self._sink is not None:
            self._sink.close()
            self._sink = None

    def _count(self, outcome: str) -> None:
        publishes.add(1, {"outcome": outcome})

"""Publish `note.created` to RabbitMQ without ever blocking a request.

`RabbitPublisher` implements `notes.ports.NotePublisher`: `note_created` only
enqueues (and returns), a single worker task drains the queue to the broker.
The queue is bounded and drops — never blocks — when full, exactly like the
webhook dispatcher. The AMQP specifics live behind `_Sink`, injected, so the
queue/worker/backoff logic is tested with no broker; the default sink opens an
aio-pika topic exchange with publisher confirms and persistent messages.

`CompositePublisher` fans one note out to several publishers (the webhook
dispatcher AND this one), so wiring both does not mean choosing between them.
"""

from __future__ import annotations

import asyncio
import logging
import random
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Any, Protocol

from opentelemetry.metrics import Meter

from ..notes.domain import Note
from ..notes.ports import NotePublisher
from .events import ROUTING_KEY, encode, note_created

log = logging.getLogger(__name__)


class _Sink(Protocol):
    """Where a worker publishes. The default is an aio-pika exchange; a test
    passes a fake. `publish` raises on a broker error — the worker reopens."""

    async def publish(self, routing_key: str, body: bytes) -> None: ...
    async def aclose(self) -> None: ...


#: Opens a sink for a URL. Injected, so the worker is broker-free in tests.
Opener = Callable[[str], Awaitable["_Sink"]]


class _AioPikaSink:
    """The default sink: a persistent publish to the topic exchange, with
    publisher confirms so `publish` only returns once the broker acked."""

    def __init__(self, connection: Any, exchange: Any) -> None:
        self._connection = connection
        self._exchange = exchange

    async def publish(self, routing_key: str, body: bytes) -> None:
        import aio_pika

        message = aio_pika.Message(body=body, delivery_mode=aio_pika.DeliveryMode.PERSISTENT)
        await self._exchange.publish(message, routing_key=routing_key)

    async def aclose(self) -> None:
        await self._connection.close()


async def aio_pika_opener(url: str) -> _Sink:
    import aio_pika

    from .topology import declare_exchange

    connection = await aio_pika.connect_robust(url)
    channel = await connection.channel(publisher_confirms=True)
    exchange = await declare_exchange(channel)
    return _AioPikaSink(connection, exchange)


@dataclass(frozen=True, slots=True)
class _Job:
    routing_key: str
    body: bytes


class CompositePublisher:
    """A `NotePublisher` that forwards to several. One failing to enqueue never
    stops the others — each is already non-blocking and bounded."""

    def __init__(self, publishers: list[NotePublisher]) -> None:
        self._publishers = publishers

    def note_created(self, note: Note) -> None:
        for publisher in self._publishers:
            publisher.note_created(note)


class RabbitPublisher:
    def __init__(
        self,
        *,
        url: str,
        meter: Meter,
        opener: Opener,
        queue_size: int = 256,
        reopen_backoff: float = 0.5,
        reopen_backoff_max: float = 10.0,
        sleep: Callable[[float], Awaitable[None]] = asyncio.sleep,
        jitter: Callable[[], float] = random.random,
    ) -> None:
        self._url = url
        self._opener = opener
        self._outcomes = meter.create_counter(
            "rabbit.publishes", description="RabbitMQ publishes by outcome"
        )
        self._queue: asyncio.Queue[_Job] = asyncio.Queue(maxsize=queue_size)
        self._reopen_backoff = reopen_backoff
        self._reopen_backoff_max = reopen_backoff_max
        self._sleep = sleep
        self._jitter = jitter
        self._closed = False
        self._sink: _Sink | None = None
        self._worker: asyncio.Task[None] | None = None

    def start(self) -> None:
        self._worker = asyncio.create_task(self._run(), name="rabbit-publisher")

    def note_created(self, note: Note) -> None:
        if self._closed:
            self._count("dropped")
            return
        job = _Job(ROUTING_KEY, encode(note_created(note)))
        try:
            self._queue.put_nowait(job)
        except asyncio.QueueFull:
            self._count("dropped")
            log.error("rabbit publish queue full, event dropped", extra={"note_id": note.id})

    async def close(self, within: float) -> bool:
        self._closed = True
        drained = True
        try:
            await asyncio.wait_for(self._queue.join(), timeout=within)
        except TimeoutError:
            drained = False
            left = self._queue.qsize()
            for _ in range(left):
                self._count("dropped")
            log.error("rabbit queue not drained before the deadline", extra={"dropped": left})
        if self._worker is not None:
            self._worker.cancel()
            await asyncio.gather(self._worker, return_exceptions=True)
        if self._sink is not None:
            await self._sink.aclose()
            self._sink = None
        return drained

    async def _run(self) -> None:
        # One `task_done` per job, always, so `close`'s `queue.join()` returns
        # whether the publish succeeded, was dropped, or the worker is cancelled.
        while True:
            job = await self._queue.get()
            try:
                await self._publish(job)
            finally:
                self._queue.task_done()

    async def _publish(self, job: _Job) -> None:
        """Publish `job`, reopening the sink with capped backoff on failure.
        Retries until it succeeds or the task is cancelled — `close` bounds a
        dead broker by its own deadline (`queue.join` times out) and then
        cancels the worker, so this never hangs a shutdown."""
        attempt = 0
        while True:
            sink = await self._ensure_sink()
            try:
                await sink.publish(job.routing_key, job.body)
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # any broker error reopens
                await self._drop_sink(exc)
                self._count("failed")
                attempt += 1
                backoff = min(self._reopen_backoff * 2 ** (attempt - 1), self._reopen_backoff_max)
                await self._sleep(self._jitter() * backoff)
                continue
            self._count("published")
            return

    async def _ensure_sink(self) -> _Sink:
        if self._sink is None:
            self._sink = await self._opener(self._url)
        return self._sink

    async def _drop_sink(self, exc: Exception) -> None:
        log.warning("rabbit sink error, reopening", extra={"error": type(exc).__name__})
        if self._sink is not None:
            await self._sink.aclose()
            self._sink = None

    def _count(self, outcome: str) -> None:
        self._outcomes.add(1, {"outcome": outcome})

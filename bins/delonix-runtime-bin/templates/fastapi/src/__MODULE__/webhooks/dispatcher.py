"""Outbound webhooks: every created note is sent, signed, as `note.created`.

Delivery is AT-LEAST-ONCE while the process lives and AT-MOST-ONCE across a
restart. The queue is memory (an `asyncio.Queue` drained by one worker task):
what is queued when the process dies is lost, and a full queue drops (logs and
counts) instead of blocking the request that created the note. The
`webhook-id` is the note id, so a receiver that de-duplicates by it sees each
note once however many attempts it took. Never exactly-once. A deployment that
must not lose an event needs an outbox table written in the same transaction as
the note — see docs/adr/0002-webhooks.md.
"""

from __future__ import annotations

import asyncio
import json
import logging
import random
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass

import httpx
from opentelemetry import context as otel_context
from opentelemetry.context import Context
from opentelemetry.metrics import Meter
from opentelemetry.trace import SpanKind, StatusCode, Tracer

from ..notes.domain import Note
from .signature import HEADER_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, sign

log = logging.getLogger(__name__)


@dataclass(frozen=True, slots=True)
class _Job:
    note: Note
    # The context of the request that created the note, so the delivery is
    # a child span of it and shares its trace id. The request's task is gone
    # by the time the worker runs; only its context travels.
    context: Context


def _event_body(note: Note) -> bytes:
    created = note.created_at.isoformat(timespec="microseconds").replace("+00:00", "Z")
    event = {
        "type": "note.created",
        "data": {"id": note.id, "title": note.title, "body": note.body, "created_at": created},
    }
    return json.dumps(event, separators=(",", ":")).encode()


class _Retryable(Exception):
    pass


class WebhookDispatcher:
    """Implements `notes.ports.NotePublisher`. Bounded everywhere: queue size,
    attempts, per-attempt timeout, backoff, and the time `close` may take."""

    def __init__(
        self,
        *,
        url: str,
        key: bytes,
        client: httpx.AsyncClient,
        tracer: Tracer,
        meter: Meter,
        timeout: float = 5.0,
        max_attempts: int = 4,
        base_backoff: float = 0.5,
        queue_size: int = 100,
        sleep: Callable[[float], Awaitable[None]] = asyncio.sleep,
        now: Callable[[], float] = time.time,
        jitter: Callable[[], float] = random.random,
    ) -> None:
        self._url = url
        self._key = key
        self._client = client
        self._tracer = tracer
        self._outcomes = meter.create_counter(
            "webhook.deliveries", description="Outbound webhook deliveries by final outcome"
        )
        self._timeout = timeout
        self._max_attempts = max_attempts
        self._base_backoff = base_backoff
        self._queue: asyncio.Queue[_Job] = asyncio.Queue(maxsize=queue_size)
        self._sleep = sleep
        self._now = now
        self._jitter = jitter
        self._closed = False
        self._worker: asyncio.Task[None] | None = None

    def start(self) -> None:
        """Start the worker; call from inside the running event loop (lifespan)."""
        self._worker = asyncio.create_task(self._run(), name="webhook-dispatcher")

    def note_created(self, note: Note) -> None:
        if self._closed:
            self._count("dropped")
            log.warning("webhook dispatcher closed, event dropped", extra={"note_id": note.id})
            return
        try:
            self._queue.put_nowait(_Job(note, otel_context.get_current()))
        except asyncio.QueueFull:
            self._count("dropped")
            log.error("webhook queue full, event dropped", extra={"note_id": note.id})

    async def close(self, within: float) -> bool:
        """Stop taking events and wait up to `within` seconds for the queue to drain.
        Returns False, after dropping and counting what is left, when it did not."""
        self._closed = True
        drained = True
        try:
            await asyncio.wait_for(self._queue.join(), timeout=within)
        except TimeoutError:
            drained = False
            left = self._queue.qsize()
            for _ in range(left):
                self._count("dropped")
            log.error("webhook queue not drained before the deadline", extra={"dropped": left})
        if self._worker is not None:
            self._worker.cancel()
            await asyncio.gather(self._worker, return_exceptions=True)
        return drained

    async def _run(self) -> None:
        while True:
            job = await self._queue.get()
            try:
                await self._deliver(job)
            except Exception:
                log.exception("webhook worker failed", extra={"note_id": job.note.id})
            finally:
                self._queue.task_done()

    async def _deliver(self, job: _Job) -> None:
        body = _event_body(job.note)
        with self._tracer.start_as_current_span(
            "webhook.deliver",
            context=job.context,
            kind=SpanKind.PRODUCER,
            attributes={"webhook.event": "note.created"},
        ) as span:
            last_error = ""
            for attempt in range(1, self._max_attempts + 1):
                try:
                    await self._attempt(job.note.id, body)
                except _Retryable as exc:
                    last_error = str(exc)
                except _Final as exc:
                    last_error = str(exc)
                    break
                else:
                    span.set_attribute("webhook.attempts", attempt)
                    self._count("delivered")
                    log.info(
                        "webhook delivered", extra={"note_id": job.note.id, "attempts": attempt}
                    )
                    return
                if attempt < self._max_attempts:
                    # Full jitter: uniform in [0, base * 2^(attempt-1)].
                    await self._sleep(self._jitter() * self._base_backoff * 2 ** (attempt - 1))
            span.set_status(StatusCode.ERROR, "delivery failed")
            self._count("failed")
            log.error(
                "webhook delivery failed",
                extra={"note_id": job.note.id, "attempts": attempt, "error": last_error},
            )

    async def _attempt(self, msg_id: str, body: bytes) -> None:
        ts = int(self._now())
        headers = {
            "content-type": "application/json",
            HEADER_ID: msg_id,
            HEADER_TIMESTAMP: str(ts),
            HEADER_SIGNATURE: sign(self._key, msg_id, ts, body),
        }
        try:
            resp = await self._client.post(
                self._url, content=body, headers=headers, timeout=self._timeout
            )
        except httpx.TransportError as exc:  # connection refused, reset, timeout
            raise _Retryable(f"{type(exc).__name__}: {exc}") from exc
        if 200 <= resp.status_code < 300:
            return
        if resp.status_code == 429 or resp.status_code >= 500:
            raise _Retryable(f"receiver answered {resp.status_code}")
        # Other 4xx: the receiver refused this message; resending will not help.
        raise _Final(f"receiver answered {resp.status_code}")

    def _count(self, outcome: str) -> None:
        self._outcomes.add(1, {"outcome": outcome})


class _Final(Exception):
    pass

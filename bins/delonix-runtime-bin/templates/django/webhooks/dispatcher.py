"""Outbound webhooks: a bounded queue and one background thread per process.

Each event is POSTed to one URL, signed (Standard Webhooks), with a timeout
per attempt and a bounded number of attempts; network errors, 429 and 5xx
are retried with exponential backoff and full jitter, other 4xx are final.

Delivery semantics, stated plainly:
- AT-LEAST-ONCE while the process lives: a receiver can see an event twice
  (a timeout after it already accepted). The `webhook-id` is the note id, so
  a receiver that de-duplicates by it sees each note once.
- AT-MOST-ONCE across a restart: the queue is memory. What is queued when
  the process dies is lost; at a graceful stop the queue is drained within
  the shutdown budget, and what remains is dropped and counted.
- A full queue drops (and logs, and counts) instead of blocking the request
  that created the note.
Never exactly-once. Durable delivery needs an outbox table written in the
same transaction as the note, and a worker (docs/adr/0002-webhooks.md).
"""

from __future__ import annotations

import json
import logging
import queue
import random
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

import httpx
from opentelemetry import context as otel_context
from opentelemetry import metrics, trace
from opentelemetry.trace import SpanKind, Status, StatusCode

from webhooks.signature import HEADER_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, sign

log = logging.getLogger("webhooks.dispatcher")
tracer = trace.get_tracer("__NAME__.webhooks")
deliveries = metrics.get_meter("__NAME__.webhooks").create_counter(
    "webhook.deliveries", description="Outbound webhook deliveries by final outcome"
)

_STOP = object()


@dataclass(frozen=True)
class DispatcherConfig:
    url: str
    key: bytes
    timeout: float = 5.0
    max_attempts: int = 4
    base_backoff: float = 0.5
    queue_size: int = 100


@dataclass(frozen=True)
class _Job:
    event: dict[str, Any]
    msg_id: str
    parent: otel_context.Context


class Dispatcher:
    def __init__(
        self,
        config: DispatcherConfig,
        client: httpx.Client | None = None,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self.config = config
        self._client = client or httpx.Client(timeout=config.timeout)
        self._sleep = sleep
        self._queue: queue.Queue[_Job | object] = queue.Queue(maxsize=config.queue_size)
        self._lock = threading.Lock()
        self._thread: threading.Thread | None = None
        self._closed = False

    def publish(self, event_type: str, msg_id: str, data: dict[str, Any]) -> None:
        """Queue one event; never blocks on the network. The current trace
        context travels with it, so the delivery joins the request's trace."""
        job = _Job({"type": event_type, "data": data}, msg_id, otel_context.get_current())
        with self._lock:
            if self._closed:
                self._count("dropped")
                log.warning("webhook dispatcher closed, event dropped", extra={"webhook_id": msg_id})
                return
            if self._thread is None:
                # Started lazily, in the process that serves (after the fork).
                self._thread = threading.Thread(target=self._run, name="webhook-dispatcher", daemon=True)
                self._thread.start()
            try:
                self._queue.put_nowait(job)
            except queue.Full:
                self._count("dropped")
                log.error("webhook queue full, event dropped", extra={"webhook_id": msg_id})

    def close(self, timeout: float) -> bool:
        """Stop taking events and wait up to `timeout` seconds for the queue
        to drain. Returns False when events were left undelivered."""
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
            left = self._queue.qsize()
            log.error("webhook queue not drained at shutdown", extra={"events_left": left})
        return drained

    def _run(self) -> None:
        while True:
            job = self._queue.get()
            if job is _STOP:
                return
            assert isinstance(job, _Job)
            token = otel_context.attach(job.parent)
            try:
                self._deliver(job)
            except Exception:  # never let one event kill the worker thread
                log.exception("webhook delivery crashed", extra={"webhook_id": job.msg_id})
            finally:
                otel_context.detach(token)

    def _deliver(self, job: _Job) -> None:
        with tracer.start_as_current_span(
            "webhook.deliver", kind=SpanKind.PRODUCER, attributes={"webhook.event": job.event["type"]}
        ) as span:
            body = json.dumps(job.event, separators=(",", ":")).encode()
            last_error = ""
            for attempt in range(1, self.config.max_attempts + 1):
                retry, last_error = self._attempt(job.msg_id, body)
                if not last_error:
                    span.set_attribute("webhook.attempts", attempt)
                    self._count("delivered")
                    log.info("webhook delivered", extra={"webhook_id": job.msg_id, "attempts": attempt})
                    return
                if not retry or attempt == self.config.max_attempts:
                    break
                # Full jitter: uniform in [0, base * 2^(attempt-1)].
                self._sleep(random.uniform(0, self.config.base_backoff * 2 ** (attempt - 1)))  # noqa: S311
            span.set_status(Status(StatusCode.ERROR, "delivery failed"))
            self._count("failed")
            log.error(
                "webhook delivery failed",
                extra={"webhook_id": job.msg_id, "attempts": attempt, "error": last_error},
            )

    def _attempt(self, msg_id: str, body: bytes) -> tuple[bool, str]:
        """One POST. Returns (retry?, error or "")."""
        now = int(time.time())
        headers = {
            "content-type": "application/json",
            HEADER_ID: msg_id,
            HEADER_TIMESTAMP: str(now),
            HEADER_SIGNATURE: sign(self.config.key, msg_id, now, body),
        }
        try:
            response = self._client.post(self.config.url, content=body, headers=headers)
        except httpx.HTTPError as exc:  # network error or timeout
            return True, f"{type(exc).__name__}: {exc}"
        status = response.status_code
        if 200 <= status < 300:
            return False, ""
        # 429 and 5xx may pass later; another 4xx means the receiver refused
        # this message, and resending it will not help.
        return status == 429 or status >= 500, f"receiver answered {status}"

    def _count(self, outcome: str) -> None:
        deliveries.add(1, {"outcome": outcome})

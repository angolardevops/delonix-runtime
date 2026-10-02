"""Outbound delivery: signing, retries with full jitter, final 4xx, queue
bounds, draining at close."""

from __future__ import annotations

import threading
import time

import httpx
import pytest

from webhooks import signature
from webhooks.dispatcher import Dispatcher, DispatcherConfig

KEY = b"k" * 32


def make(handler, sleeps=None, **config):
    sleeps = [] if sleeps is None else sleeps
    client = httpx.Client(transport=httpx.MockTransport(handler))
    cfg = DispatcherConfig(url="https://receiver.test/hook", key=KEY, base_backoff=0.5, **config)
    return Dispatcher(cfg, client=client, sleep=sleeps.append)


def statuses(*codes):
    seen = []

    def handler(request):
        seen.append(request)
        return httpx.Response(codes[min(len(seen), len(codes)) - 1])

    return handler, seen


def test_a_delivery_is_signed_with_the_note_id():
    handler, seen = statuses(204)
    d = make(handler)
    d.publish("note.created", "n1", {"id": "n1"})
    assert d.close(5)
    req = seen[0]
    assert req.headers[signature.HEADER_ID] == "n1"
    signature.verify(
        KEY,
        "n1",
        req.headers[signature.HEADER_TIMESTAMP],
        req.headers[signature.HEADER_SIGNATURE],
        req.content,
        time.time(),
    )


def test_5xx_and_429_are_retried_with_full_jitter():
    handler, seen = statuses(500, 429, 204)
    sleeps: list[float] = []
    d = make(handler, sleeps)
    d.publish("note.created", "n1", {})
    assert d.close(5)
    assert len(seen) == 3
    assert 0 <= sleeps[0] <= 0.5 and 0 <= sleeps[1] <= 1.0


def test_other_4xx_is_final():
    handler, seen = statuses(400)
    d = make(handler)
    d.publish("note.created", "n1", {})
    assert d.close(5)
    assert len(seen) == 1


def test_network_errors_are_retried_up_to_the_bound():
    calls = []

    def handler(request):
        calls.append(request)
        raise httpx.ConnectError("refused")

    d = make(handler, max_attempts=3)
    d.publish("note.created", "n1", {})
    assert d.close(5)
    assert len(calls) == 3


def test_a_full_queue_drops_instead_of_blocking():
    gate = threading.Event()

    def handler(request):
        gate.wait(5)
        return httpx.Response(204)

    d = make(handler, queue_size=1)
    start = time.monotonic()
    for i in range(5):
        d.publish("note.created", f"n{i}", {})
    assert time.monotonic() - start < 1
    gate.set()
    assert d.close(5)


def test_close_reports_an_undrained_queue_and_later_events_are_dropped():
    gate = threading.Event()

    def handler(request):
        gate.wait(5)
        return httpx.Response(204)

    d = make(handler)
    d.publish("note.created", "n1", {})
    assert d.close(0.2) is False
    gate.set()
    d.publish("note.created", "n2", {})  # after close: dropped, never raises


@pytest.mark.parametrize("status", [200, 201, 202, 204])
def test_any_2xx_is_delivered(status):
    handler, seen = statuses(status)
    d = make(handler)
    d.publish("note.created", "n1", {})
    assert d.close(5)
    assert len(seen) == 1

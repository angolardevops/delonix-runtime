"""Messaging without a broker: the wire format, the bounded publish queue with
its background worker and reconnect, and the consumer's ack / retry /
dead-letter decision. The pika glue (`Publisher._PikaSink`, `Consumer.run`,
`_PikaIncoming`) is exercised by the CI integration run against the compose
`rabbitmq` service, not here."""

from __future__ import annotations

import threading
import time

import pytest

from messaging.consumer import Consumer
from messaging.events import ROUTING_KEY, decode, encode, envelope
from messaging.publisher import Publisher

# ---- events ----


def test_event_roundtrips_and_names_the_note():
    event = envelope("note.created", {"id": "n1", "title": "t"})
    assert event["type"] == "note.created"
    assert event["data"]["id"] == "n1"
    assert decode(encode(event)) == event


@pytest.mark.parametrize("body", [b"not json", b"123", b'{"no":"type"}', b'{"type":5}'])
def test_decode_rejects_a_poison_body(body):
    with pytest.raises(ValueError):
        decode(body)


# ---- Publisher (fake sink) ----


class FakeSink:
    """Records what it publishes. `fail_times` publishes raise first (to drive
    the reconnect path); `block` makes publish wait on an event (to fill the
    queue)."""

    def __init__(self, fail_times=0, block=None):
        self.published: list[tuple[str, bytes]] = []
        self.fail_times = fail_times
        self.block = block
        self.closes = 0
        self._lock = threading.Lock()

    def publish(self, routing_key, body):
        if self.block is not None:
            self.block.wait(5)
        with self._lock:
            if self.fail_times > 0:
                self.fail_times -= 1
                raise ConnectionError("broker down")
            self.published.append((routing_key, body))

    def close(self):
        self.closes += 1


def make(sink, **kw):
    return Publisher("amqp://ignored", opener=lambda url: sink, sleep=lambda d: None, **kw)


def test_a_published_event_reaches_the_sink_with_the_routing_key():
    sink = FakeSink()
    p = make(sink)
    p.publish("note.created", "n1", {"id": "n1"})
    assert p.close(5)
    assert len(sink.published) == 1
    routing_key, body = sink.published[0]
    assert routing_key == ROUTING_KEY
    assert decode(body) == {"type": "note.created", "data": {"id": "n1"}}


def test_the_worker_reconnects_after_a_sink_failure():
    sink = FakeSink(fail_times=2)
    p = make(sink)
    p.publish("note.created", "n1", {"id": "n1"})
    assert p.close(5)
    assert len(sink.published) == 1  # succeeded after two reopens
    assert sink.closes >= 2  # the failed sink was dropped each time


def test_a_full_queue_drops_instead_of_blocking():
    gate = threading.Event()
    sink = FakeSink(block=gate)
    p = make(sink, queue_size=1)
    start = time.monotonic()
    for i in range(5):
        p.publish("note.created", f"n{i}", {"id": f"n{i}"})
    assert time.monotonic() - start < 1  # never blocked on the sink
    gate.set()
    assert p.close(5)
    assert len(sink.published) <= 2  # at most the in-flight one plus the queued one


def test_after_close_a_publish_is_dropped_and_never_raises():
    sink = FakeSink()
    p = make(sink)
    assert p.close(5)
    p.publish("note.created", "n2", {"id": "n2"})  # no raise, no delivery
    assert sink.published == []


# ---- Consumer.handle (fake delivery) ----


class FakeIncoming:
    def __init__(self, body, retry_count=0):
        self._body = body
        self._retry_count = retry_count
        self.actions: list[str] = []
        self.retried_with: float | None = None

    @property
    def body(self):
        return self._body

    @property
    def retry_count(self):
        return self._retry_count

    def ack(self):
        self.actions.append("ack")

    def dead_letter(self):
        self.actions.append("dead_letter")

    def retry(self, delay):
        self.actions.append("retry")
        self.retried_with = delay


def a_message(retry_count=0):
    return FakeIncoming(encode(envelope("note.created", {"id": "n1"})), retry_count)


def test_a_processed_message_is_acked():
    seen = []
    msg = a_message()
    Consumer("amqp://x", handler=seen.append).handle(msg)
    assert msg.actions == ["ack"]
    assert seen == [{"type": "note.created", "data": {"id": "n1"}}]


def test_a_poison_message_is_dead_lettered_at_once():
    msg = FakeIncoming(b"not an event")
    Consumer("amqp://x").handle(msg)
    assert msg.actions == ["dead_letter"]


def test_a_handler_failure_is_retried_until_the_bound_then_dead_lettered():
    def boom(event):
        raise RuntimeError("transient")

    early = a_message(retry_count=0)
    Consumer("amqp://x", handler=boom, max_retries=3, retry_delay=2.0).handle(early)
    assert early.actions == ["retry"]
    assert early.retried_with == 2.0

    exhausted = a_message(retry_count=3)
    Consumer("amqp://x", handler=boom, max_retries=3).handle(exhausted)
    assert exhausted.actions == ["dead_letter"]

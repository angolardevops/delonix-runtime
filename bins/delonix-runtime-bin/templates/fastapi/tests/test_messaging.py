"""Messaging without a broker: the wire format, the fan-out publisher, the
bounded publish queue, and the consumer's ack / retry / dead-letter decision.
The AMQP glue (`_run`, `_AioPikaIncoming`) is exercised by the CI integration
run against the compose `rabbitmq` service, not here."""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from datetime import UTC, datetime
from typing import Any

import pytest
from opentelemetry.metrics import NoOpMeterProvider

from __MODULE__.messaging.consumer import RabbitConsumer
from __MODULE__.messaging.events import ROUTING_KEY, decode, encode, note_created
from __MODULE__.messaging.publisher import CompositePublisher, RabbitPublisher
from __MODULE__.notes.domain import Note

pytestmark = pytest.mark.anyio

METER = NoOpMeterProvider().get_meter("test")


def a_note(note_id: str = "n1") -> Note:
    return Note(id=note_id, title="t", body="b", created_at=datetime(2026, 1, 1, tzinfo=UTC))


# ---- events ----


def test_event_roundtrips_and_names_the_note() -> None:
    event = note_created(a_note())
    assert event["type"] == "note.created"
    assert event["data"]["id"] == "n1"
    assert decode(encode(event)) == event


@pytest.mark.parametrize("body", [b"not json", b"123", b'{"no":"type"}', b'{"type":5}'])
def test_decode_rejects_a_poison_body(body: bytes) -> None:
    with pytest.raises(ValueError):  # noqa: PT011 — any ValueError is the contract
        decode(body)


# ---- CompositePublisher ----


class RecordingPublisher:
    def __init__(self) -> None:
        self.seen: list[str] = []

    def note_created(self, note: Note) -> None:
        self.seen.append(note.id)


def test_composite_forwards_to_every_publisher() -> None:
    a, b = RecordingPublisher(), RecordingPublisher()
    CompositePublisher([a, b]).note_created(a_note("x"))
    assert a.seen == ["x"]
    assert b.seen == ["x"]


# ---- RabbitPublisher (fake sink) ----


class FakeSink:
    def __init__(self, fail_times: int = 0) -> None:
        self.published: list[tuple[str, bytes]] = []
        self.fail_times = fail_times
        self.closed = False

    async def publish(self, routing_key: str, body: bytes) -> None:
        if self.fail_times > 0:
            self.fail_times -= 1
            raise ConnectionError("broker down")
        self.published.append((routing_key, body))

    async def aclose(self) -> None:
        self.closed = True


def opener_for(*sinks: FakeSink) -> Callable[[str], Awaitable[FakeSink]]:
    seq = iter(sinks)

    async def opener(url: str) -> FakeSink:
        return next(seq)

    return opener


async def _immediate(_: float) -> None:
    return None


async def test_publisher_delivers_queued_notes() -> None:
    sink = FakeSink()
    pub = RabbitPublisher(url="amqp://x", meter=METER, opener=opener_for(sink))
    pub.start()
    pub.note_created(a_note("n1"))
    pub.note_created(a_note("n2"))
    assert await pub.close(within=1.0) is True
    assert [rk for rk, _ in sink.published] == [ROUTING_KEY, ROUTING_KEY]
    assert decode(sink.published[0][1])["data"]["id"] == "n1"
    assert sink.closed


async def test_publisher_reopens_the_sink_after_a_failure() -> None:
    broken, good = FakeSink(fail_times=1), FakeSink()
    pub = RabbitPublisher(
        url="amqp://x",
        meter=METER,
        opener=opener_for(broken, good),
        sleep=_immediate,
        jitter=lambda: 0.0,
    )
    pub.start()
    pub.note_created(a_note("n1"))
    assert await pub.close(within=1.0) is True
    assert len(good.published) == 1  # reopened and redelivered on the new sink


async def test_publisher_drops_when_the_queue_is_full() -> None:
    sink = FakeSink()
    pub = RabbitPublisher(url="amqp://x", meter=METER, opener=opener_for(sink), queue_size=1)
    # No worker running yet: the queue fills and extra notes are dropped, never block.
    pub.note_created(a_note("n1"))
    pub.note_created(a_note("n2"))
    pub.note_created(a_note("n3"))
    pub.start()
    await pub.close(within=1.0)
    assert len(sink.published) == 1


# ---- RabbitConsumer.handle (fake delivery) ----


class FakeIncoming:
    def __init__(self, body: bytes, retry_count: int = 0) -> None:
        self._body = body
        self._retry = retry_count
        self.acked = False
        self.dead_lettered = False
        self.retried_with: float | None = None

    @property
    def body(self) -> bytes:
        return self._body

    @property
    def retry_count(self) -> int:
        return self._retry

    async def ack(self) -> None:
        self.acked = True

    async def dead_letter(self) -> None:
        self.dead_lettered = True

    async def retry(self, delay: float) -> None:
        self.retried_with = delay


def consumer(handler: Callable[[dict[str, Any]], Awaitable[None]], **kw: object) -> RabbitConsumer:
    return RabbitConsumer(url="amqp://x", meter=METER, handler=handler, **kw)  # type: ignore[arg-type]


async def _ok(_: dict[str, Any]) -> None:
    return None


async def _boom(_: dict[str, Any]) -> None:
    raise RuntimeError("transient")


async def test_handle_acks_a_processed_message() -> None:
    msg = FakeIncoming(encode(note_created(a_note())))
    await consumer(_ok).handle(msg)
    assert msg.acked
    assert not msg.dead_lettered


async def test_handle_dead_letters_a_poison_message_at_once() -> None:
    msg = FakeIncoming(b"not an event")
    await consumer(_ok).handle(msg)
    assert msg.dead_lettered
    assert not msg.acked
    assert msg.retried_with is None


async def test_handle_retries_a_transient_failure() -> None:
    msg = FakeIncoming(encode(note_created(a_note())), retry_count=0)
    await consumer(_boom, max_retries=3, retry_delay=0.25).handle(msg)
    assert msg.retried_with == 0.25
    assert not msg.dead_lettered
    assert not msg.acked


async def test_handle_dead_letters_when_retries_are_exhausted() -> None:
    msg = FakeIncoming(encode(note_created(a_note())), retry_count=3)
    await consumer(_boom, max_retries=3).handle(msg)
    assert msg.dead_lettered
    assert not msg.acked
    assert msg.retried_with is None

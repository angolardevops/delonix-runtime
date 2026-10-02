"""Standard Webhooks: signature, inbound endpoint, outbound dispatcher."""

from __future__ import annotations

import asyncio
import base64
import json
import time

import httpx
import pytest
from fastapi.testclient import TestClient
from opentelemetry.sdk.metrics import MeterProvider
from opentelemetry.sdk.trace import TracerProvider

from __MODULE__.app import (
    create_app,
)
from __MODULE__.notes.domain import (
    Note,
)
from __MODULE__.webhooks.api import (
    WEBHOOK_MAX_BODY,
)
from __MODULE__.webhooks.dispatcher import (
    WebhookDispatcher,
)
from __MODULE__.webhooks.signature import (
    BadSignatureError,
    BadTimestampError,
    MissingHeadersError,
    parse_secret,
    sign,
    verify,
)

from .conftest import KEY, SECRET, Spans, make_settings


def signed(body: bytes, msg_id: str = "msg_1", ts: int | None = None) -> dict[str, str]:
    ts = int(time.time()) if ts is None else ts
    return {
        "webhook-id": msg_id,
        "webhook-timestamp": str(ts),
        "webhook-signature": sign(KEY, msg_id, ts, body),
        "content-type": "application/json",
    }


# --- signature ---------------------------------------------------------------


def test_parse_secret_requires_the_standard_form() -> None:
    assert parse_secret(SECRET) == KEY
    for bad in ["k" * 40, "whsec_not-base64!", "whsec_" + base64.b64encode(b"short").decode()]:
        with pytest.raises(ValueError, match="webhook secret"):
            parse_secret(bad)


def test_sign_matches_the_standard_webhooks_reference_vector() -> None:
    # The vector of standard-webhooks libraries/python/tests/test_webhooks.py::test_sign_function.
    key = parse_secret("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw")
    body = b'{"test": 2432232314}'
    got = sign(key, "msg_p5jXN8AQM9LWM0D4loKWxJek", 1614265330, body)
    assert got == "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="


def test_verify_refuses_missing_stale_and_forged() -> None:
    body = b"{}"
    now = 1_700_000_000
    good = sign(KEY, "id", now, body)
    verify(KEY, "id", str(now), f"v1,bogus {good}", body, now)  # rotation: any match
    with pytest.raises(MissingHeadersError):
        verify(KEY, "", str(now), good, body, now)
    with pytest.raises(BadTimestampError):
        verify(KEY, "id", str(now), good, body, now + 301)
    with pytest.raises(BadSignatureError):
        verify(KEY, "id", str(now), good, b"{ }", now)


# --- inbound ------------------------------------------------------------------


def test_inbound_is_off_without_a_secret(spans: Spans) -> None:
    with TestClient(create_app(make_settings(), spans.telemetry)) as client:
        resp = client.post("/api/v1/webhooks/inbound", content=b"{}")
    assert resp.status_code == 404
    assert resp.json()["error"]["code"] == "not_found"


def test_inbound_verifies_and_deduplicates(client: TestClient) -> None:
    body = json.dumps({"type": "note.create", "data": {"title": "from a webhook"}}).encode()
    headers = signed(body, "msg_create")
    url = "/api/v1/webhooks/inbound"
    assert client.post(url, content=body, headers=headers).status_code == 204
    # The sender's retry of the same id is acknowledged, not processed again.
    assert client.post(url, content=body, headers=headers).status_code == 204
    assert client.get("/api/v1/notes").json()["total"] == 1

    tampered = client.post(url, content=body + b" ", headers=headers)
    assert (tampered.status_code, tampered.json()["error"]["code"]) == (401, "invalid_signature")
    stale = client.post(url, content=body, headers=signed(body, "old", int(time.time()) - 600))
    assert stale.status_code == 401
    missing = client.post(url, content=body)
    assert missing.json()["error"]["code"] == "missing_signature"
    ping = json.dumps({"type": "ping"}).encode()
    assert client.post(url, content=ping, headers=signed(ping, "p1")).status_code == 204
    other = json.dumps({"type": "other"}).encode()
    assert client.post(url, content=other, headers=signed(other, "o1")).status_code == 422


def test_a_failed_delivery_is_processed_on_retry(client: TestClient) -> None:
    url = "/api/v1/webhooks/inbound"
    bad = json.dumps({"type": "note.create", "data": {"title": ""}}).encode()
    assert client.post(url, content=bad, headers=signed(bad, "msg_retry")).status_code == 422
    good = json.dumps({"type": "note.create", "data": {"title": "fixed"}}).encode()
    assert client.post(url, content=good, headers=signed(good, "msg_retry")).status_code == 204
    assert client.get("/api/v1/notes").json()["total"] == 1


def test_inbound_body_limit(client: TestClient) -> None:
    body = b'{"type":"ping","pad":"' + b"x" * WEBHOOK_MAX_BODY + b'"}'
    resp = client.post("/api/v1/webhooks/inbound", content=body, headers=signed(body))
    assert resp.status_code == 413


# --- outbound -----------------------------------------------------------------


def note(note_id: str = "n1") -> Note:
    from datetime import UTC, datetime

    return Note(id=note_id, title="t", body="", created_at=datetime(2026, 1, 1, tzinfo=UTC))


def dispatcher(
    handler: httpx.MockTransport, attempts: int = 4, queue: int = 8
) -> WebhookDispatcher:
    async def no_sleep(_: float) -> None:
        return None

    return WebhookDispatcher(
        url="http://receiver.test/hook",
        key=KEY,
        client=httpx.AsyncClient(transport=handler),
        tracer=TracerProvider().get_tracer("t"),
        meter=MeterProvider().get_meter("t"),
        max_attempts=attempts,
        queue_size=queue,
        sleep=no_sleep,
    )


@pytest.mark.anyio
async def test_retries_server_errors_then_delivers_with_one_idempotency_id() -> None:
    seen: list[httpx.Request] = []

    def handle(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(503 if len(seen) < 3 else 204)

    d = dispatcher(httpx.MockTransport(handle))
    d.start()
    d.note_created(note("n1"))
    assert await d.close(within=5)
    assert len(seen) == 3
    assert {r.headers["webhook-id"] for r in seen} == {"n1"}
    last = seen[-1]
    verify(
        KEY,
        "n1",
        last.headers["webhook-timestamp"],
        last.headers["webhook-signature"],
        last.content,
        time.time(),
    )
    assert json.loads(last.content)["type"] == "note.created"


@pytest.mark.anyio
async def test_a_client_error_is_not_retried() -> None:
    calls = 0

    def handle(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        return httpx.Response(400)

    d = dispatcher(httpx.MockTransport(handle))
    d.start()
    d.note_created(note())
    await d.close(within=5)
    assert calls == 1


@pytest.mark.anyio
async def test_network_errors_are_retried_up_to_the_limit() -> None:
    calls = 0

    def handle(request: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        raise httpx.ConnectError("refused", request=request)

    d = dispatcher(httpx.MockTransport(handle), attempts=3)
    d.start()
    d.note_created(note())
    await d.close(within=5)
    assert calls == 3


@pytest.mark.anyio
async def test_close_is_bounded_and_later_events_are_dropped() -> None:
    release = asyncio.Event()

    async def handle(_: httpx.Request) -> httpx.Response:
        await release.wait()
        return httpx.Response(204)

    d = dispatcher(httpx.MockTransport(handle), attempts=1)
    d.start()
    d.note_created(note("slow"))
    started = time.monotonic()
    assert await d.close(within=0.2) is False
    assert time.monotonic() - started < 2
    d.note_created(note("late"))  # must not raise


@pytest.mark.anyio
async def test_a_full_queue_drops_instead_of_blocking() -> None:
    d = dispatcher(httpx.MockTransport(lambda _: httpx.Response(204)), queue=1)
    # Worker not started: the queue fills.
    d.note_created(note("a"))
    d.note_created(note("b"))  # dropped, not blocking
    d.start()
    assert await d.close(within=5)

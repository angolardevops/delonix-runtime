"""One request that creates a note must leave ONE trace across the incoming
request, the use case, the outbound webhook call and the log lines — that is
what makes a failure findable from any of the three."""

from __future__ import annotations

import threading

import httpx
from fastapi.testclient import TestClient

from __MODULE__.app import (
    create_app,
)
from __MODULE__.webhooks.signature import (
    verify,
)

from .conftest import KEY, SECRET, JsonLines, Spans, make_settings


def test_one_trace_across_request_use_case_outbound_call_and_logs(
    spans: Spans, logs: JsonLines
) -> None:
    received: list[httpx.Request] = []
    arrived = threading.Event()

    def receiver(request: httpx.Request) -> httpx.Response:
        received.append(request)
        arrived.set()
        return httpx.Response(204)

    app = create_app(
        make_settings(
            webhook_target_url="https://receiver.test/hook", webhook_target_secret=SECRET
        ),
        spans.telemetry,
        webhook_client=httpx.AsyncClient(transport=httpx.MockTransport(receiver)),
    )
    with TestClient(app) as client:
        assert client.post("/api/v1/notes", json={"title": "traced"}).status_code == 201
        assert arrived.wait(5), "the outbound webhook never arrived"
    # Leaving the block ran the lifespan shutdown: the queue is drained.

    by_name = {s.name: s for s in spans.finished()}
    server = by_name.get("POST /api/v1/notes")
    assert server is not None, sorted(by_name)
    trace_id = server.context.trace_id
    for name in ("notes.create", "webhook.deliver", "POST"):
        assert name in by_name, sorted(by_name)
        assert by_name[name].context.trace_id == trace_id, name

    request = received[0]
    # traceparent: 00-<trace id>-<span id>-<flags>
    assert request.headers["traceparent"].split("-")[1] == format(trace_id, "032x")
    verify(
        KEY,
        request.headers["webhook-id"],
        request.headers["webhook-timestamp"],
        request.headers["webhook-signature"],
        request.content,
        now=float(request.headers["webhook-timestamp"]),
    )

    messages = {
        line["msg"] for line in logs.lines if line.get("trace_id") == format(trace_id, "032x")
    }
    assert {"request", "webhook delivered"} <= messages, messages

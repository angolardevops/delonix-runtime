"""One request that creates a note must leave ONE trace across the incoming
request, the use case, the outbound webhook call and the log lines — that is
what makes a failure findable from any of the three."""

from __future__ import annotations

import json
import logging
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import ClassVar

import pytest

from webhooks import receivers, signature

KEY = b"t" * 32


class Receiver(BaseHTTPRequestHandler):
    got: ClassVar[list[dict]] = []

    def do_POST(self):
        body = self.rfile.read(int(self.headers["content-length"]))
        signature.verify(
            KEY,
            self.headers[signature.HEADER_ID],
            self.headers[signature.HEADER_TIMESTAMP],
            self.headers[signature.HEADER_SIGNATURE],
            body,
            time.time(),
        )
        Receiver.got.append({"traceparent": self.headers.get("traceparent", ""), "body": body})
        self.send_response(204)
        self.end_headers()

    def log_message(self, *args):
        pass


class Capture(logging.Handler):
    def __init__(self):
        super().__init__()
        from core.logging import ContextFilter, JsonFormatter

        self.addFilter(ContextFilter())
        self.setFormatter(JsonFormatter())
        self.lines: list[dict] = []

    def emit(self, record):
        self.lines.append(json.loads(self.format(record)))


@pytest.mark.django_db(transaction=True)
def test_one_trace_across_request_use_case_outbound_call_and_logs(client, spans, app_config, monkeypatch):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Receiver)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    Receiver.got = []
    app_config(webhook_target_url=f"http://127.0.0.1:{server.server_port}/hook", webhook_target_key=KEY)
    monkeypatch.setattr(receivers, "_dispatcher", None)
    capture = Capture()
    logging.getLogger().addHandler(capture)
    try:
        response = client.post("/api/v1/notes", data={"title": "traced"}, content_type="application/json")
        assert response.status_code == 201
        assert receivers.close(5), "the outbound webhook was not delivered"
    finally:
        logging.getLogger().removeHandler(capture)
        server.shutdown()
        monkeypatch.setattr(receivers, "_dispatcher", None)

    finished = {s.name: s for s in spans.get_finished_spans()}
    server_span = next((s for s in finished.values() if s.kind.name == "SERVER"), None)
    assert server_span is not None, f"no server span; spans: {sorted(finished)}"
    trace_id = server_span.context.trace_id
    for name in ("notes.create", "webhook.deliver"):
        assert name in finished, f"missing span {name}; spans: {sorted(finished)}"
        assert finished[name].context.trace_id == trace_id, f"{name} is in another trace"
    clients = [s for s in finished.values() if s.kind.name == "CLIENT"]
    assert clients and all(s.context.trace_id == trace_id for s in clients), "outbound HTTP span not in the trace"

    assert Receiver.got, "the receiver got nothing"
    hex_id = format(trace_id, "032x")
    assert hex_id in Receiver.got[0]["traceparent"], "traceparent does not carry the request's trace"
    messages = {line["msg"] for line in capture.lines if line.get("trace_id") == hex_id}
    assert {"request", "webhook delivered"} <= messages, f"log lines in the trace: {messages}"

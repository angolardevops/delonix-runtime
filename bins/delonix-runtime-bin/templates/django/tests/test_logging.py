"""JSON lines, context fields, redaction, trace correlation."""

from __future__ import annotations

import json
import logging

from opentelemetry import trace

from core import context
from core.logging import REDACTED, ContextFilter, JsonFormatter


def render(message, **extra):
    logger = logging.getLogger("test.logging")
    record = logger.makeRecord(logger.name, logging.INFO, __file__, 1, message, (), None, extra=extra)
    ContextFilter(service="svc", version="1.2.3", environment="test").filter(record)
    return json.loads(JsonFormatter().format(record))


def test_a_line_has_the_service_identity():
    line = render("hello", status=200)
    assert line["msg"] == "hello" and line["level"] == "info" and line["status"] == 200
    assert (line["service"], line["version"], line["environment"]) == ("svc", "1.2.3", "test")


def test_sensitive_keys_are_redacted_at_any_depth():
    line = render("x", api_key="a", headers={"Authorization": "Bearer t", "accept": "json"}, password="p")
    assert line["api_key"] == REDACTED and line["password"] == REDACTED
    assert line["headers"] == {"Authorization": REDACTED, "accept": "json"}


def test_request_id_and_trace_ids_are_added():
    token = context.request_id.set("rid-1")
    try:
        with trace.get_tracer("test").start_as_current_span("s") as span:
            line = render("inside")
            ctx = span.get_span_context()
    finally:
        context.request_id.reset(token)
    assert line["request_id"] == "rid-1"
    assert line["trace_id"] == format(ctx.trace_id, "032x")
    assert line["span_id"] == format(ctx.span_id, "016x")


def test_django_logging_uses_the_configured_service_identity(settings):
    context_filter = settings.LOGGING["filters"]["context"]
    app = settings.APP
    assert (context_filter["service"], context_filter["version"], context_filter["environment"]) == (
        app.service_name,
        app.service_version,
        app.app_env,
    )

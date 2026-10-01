"""Logs are JSON, trace-correlated and redacted."""

from __future__ import annotations

import logging

from fastapi.testclient import TestClient

from .conftest import JsonLines


def test_lines_carry_identity_and_redact_secrets(logs: JsonLines) -> None:
    logging.getLogger("tests.logging").info(
        "hello", extra={"api_key": "abc", "Authorization": "Bearer x", "note_id": "n1"}
    )
    line = logs.lines[-1]
    assert line["msg"] == "hello"
    assert (line["service"], line["version"], line["environment"]) == ("svc", "test", "test")
    assert line["api_key"] == "[REDACTED]"
    assert line["Authorization"] == "[REDACTED]"
    assert line["note_id"] == "n1"


def test_access_log_has_request_and_trace_ids_but_no_query(
    client: TestClient, logs: JsonLines
) -> None:
    client.get("/api/v1/notes?limit=5&token=do-not-log", headers={"x-request-id": "req-1"})
    access = [
        line
        for line in logs.lines
        if line["msg"] == "request" and line.get("request_id") == "req-1"
    ]
    assert len(access) == 1
    line = access[0]
    assert line["path"] == "/api/v1/notes"
    assert "do-not-log" not in str(line)
    assert len(line["trace_id"]) == 32


def test_health_probes_are_not_traced_and_log_at_debug(client: TestClient, logs: JsonLines) -> None:
    client.get("/api/v1/health/live", headers={"x-request-id": "probe"})
    (line,) = [
        entry
        for entry in logs.lines
        if entry.get("request_id") == "probe" and entry["msg"] == "request"
    ]
    assert line["level"] == "debug"
    assert "trace_id" not in line

"""JSON log lines on stdout.

Every line has `time`, `level`, `logger`, `msg`, `service`, `version`,
`environment`; inside a request also `request_id`, and `trace_id`/`span_id`
when a span is active, so a line leads to its trace and back. Values passed
with `extra=` become fields; any field whose name contains one of
SENSITIVE_WORDS is replaced by "[REDACTED]" — nested dicts included.

No Django import here: gunicorn's own logger (gunicorn.conf.py) uses these
classes in the master process, before Django is loaded.
"""

from __future__ import annotations

import json
import logging
from datetime import UTC, datetime
from typing import Any

from opentelemetry import trace

from core import context

SENSITIVE_WORDS = ("authorization", "cookie", "password", "secret", "token", "signature", "api_key")
REDACTED = "[REDACTED]"

# Attributes every LogRecord has; anything else came from `extra=`.
_STANDARD = set(logging.LogRecord("", 0, "", 0, "", None, None).__dict__) | {"message", "asctime", "taskName"}
_CONTEXT = ("service", "version", "environment", "request_id", "trace_id", "span_id")


def is_sensitive(key: str) -> bool:
    lowered = key.lower()
    return any(word in lowered for word in SENSITIVE_WORDS)


def redact(value: Any, key: str = "") -> Any:
    if key and is_sensitive(key):
        return REDACTED
    if isinstance(value, dict):
        return {k: redact(v, str(k)) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [redact(v) for v in value]
    return value


class ContextFilter(logging.Filter):
    """Adds the service identity, the request id and the active trace."""

    def __init__(self, service: str = "__NAME__", version: str = "dev", environment: str = "development") -> None:
        super().__init__()
        self.static = {"service": service, "version": version, "environment": environment}

    def filter(self, record: logging.LogRecord) -> bool:
        for key, value in self.static.items():
            setattr(record, key, value)
        rid = context.request_id.get()
        if rid:
            record.request_id = rid
        span = trace.get_current_span().get_span_context()
        if span.is_valid:
            record.trace_id = format(span.trace_id, "032x")
            record.span_id = format(span.span_id, "016x")
        return True


class JsonFormatter(logging.Formatter):
    def format(self, record: logging.LogRecord) -> str:
        line: dict[str, Any] = {
            "time": datetime.fromtimestamp(record.created, tz=UTC).isoformat(timespec="milliseconds"),
            "level": record.levelname.lower(),
            "logger": record.name,
            "msg": record.getMessage(),
        }
        for key in _CONTEXT:
            if hasattr(record, key):
                line[key] = getattr(record, key)
        for key, value in record.__dict__.items():
            if key not in _STANDARD and key not in _CONTEXT and key not in line:
                line[key] = redact(value, key)
        if record.exc_info:
            line["exception"] = self.formatException(record.exc_info)
        return json.dumps(line, default=str, ensure_ascii=False)

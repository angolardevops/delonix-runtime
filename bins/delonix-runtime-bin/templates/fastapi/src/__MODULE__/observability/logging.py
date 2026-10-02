"""JSON logs on stdout, one object per line.

Standard library only (a `logging.Formatter`), not `python-json-logger` or
`structlog`: forty lines give the exact fields and the redaction rule below,
and every library that logs through `logging` (uvicorn, httpx, the
OpenTelemetry exporter) comes out in the same format without adapters.

Every line carries `service`, `version`, `environment`; inside a request also
`request_id`, `trace_id` and `span_id`. Extra fields whose key names a secret
are replaced by `[REDACTED]`, so `log.info("x", extra={"token": t})` cannot
leak `t`.
"""

from __future__ import annotations

import json
import logging
import sys
from datetime import UTC, datetime
from typing import Any

from opentelemetry import trace

from ..web.context import current_request_id

REDACTED = "[REDACTED]"
SENSITIVE = (
    "authorization",
    "cookie",
    "password",
    "secret",
    "token",
    "signature",
    "api_key",
    "apikey",
)

# Attributes every LogRecord has; anything else came from `extra=`.
_RESERVED = set(logging.LogRecord("", 0, "", 0, "", (), None).__dict__) | {
    "message",
    "asctime",
    "color_message",  # uvicorn's ANSI duplicate of the message
}


def _is_sensitive(key: str) -> bool:
    lowered = key.lower()
    return any(s in lowered for s in SENSITIVE)


class JsonFormatter(logging.Formatter):
    def __init__(self, service: str, version: str, environment: str) -> None:
        super().__init__()
        self._static = {"service": service, "version": version, "environment": environment}

    def format(self, record: logging.LogRecord) -> str:
        out: dict[str, Any] = {
            "time": datetime.fromtimestamp(record.created, UTC).isoformat(timespec="milliseconds"),
            "level": record.levelname.lower(),
            "msg": record.getMessage(),
            "logger": record.name,
            **self._static,
        }
        rid = current_request_id()
        if rid is not None:
            out["request_id"] = rid
        ctx = trace.get_current_span().get_span_context()
        if ctx.is_valid:
            out["trace_id"] = format(ctx.trace_id, "032x")
            out["span_id"] = format(ctx.span_id, "016x")
        for key, value in record.__dict__.items():
            if key in _RESERVED or key.startswith("_"):
                continue
            out[key] = REDACTED if _is_sensitive(key) else value
        if record.exc_info and record.exc_info[1] is not None:
            exc = record.exc_info[1]
            out["error"] = f"{type(exc).__name__}: {exc}"
            out["stack"] = self.formatException(record.exc_info)
        return json.dumps(out, default=str)


def configure_logging(level: str, service: str, version: str, environment: str) -> None:
    """Route every logger (ours, uvicorn's, the SDK's) to one JSON handler."""
    handler = logging.StreamHandler(sys.stdout)
    handler.setFormatter(JsonFormatter(service, version, environment))
    root = logging.getLogger()
    root.handlers[:] = [handler]
    root.setLevel(level.upper())
    for name in ("uvicorn", "uvicorn.error"):
        logging.getLogger(name).handlers.clear()
        logging.getLogger(name).propagate = True
    # The access log is ours (web.middleware): it has the request id and the
    # trace id, and never the query string.
    logging.getLogger("uvicorn.access").disabled = True

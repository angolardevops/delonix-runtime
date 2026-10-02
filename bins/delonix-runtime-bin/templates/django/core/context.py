"""Per-request values that log lines and error bodies need, carried in a
context variable so a thread serving one request never sees another's."""

from __future__ import annotations

from contextvars import ContextVar

request_id: ContextVar[str] = ContextVar("request_id", default="")

"""Pure ASGI middleware (no `BaseHTTPMiddleware`: it would run the endpoint in
another task and lose cancellation and context).

Order, outermost first, as `app.py` installs them:

1. OpenTelemetry (contrib) — the server span; everything below logs inside it.
2. `RequestContextMiddleware` — request id, access log, 500 catch-all,
   cancellation during a drain.
3. `BodyLimitMiddleware` — refuses bodies over `HTTP_MAX_BODY_BYTES` with 413.
   Uvicorn has no body limit of its own.
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
import secrets
import time
from typing import Any

from opentelemetry import trace
from opentelemetry.trace import StatusCode
from starlette.types import ASGIApp, Message, Receive, Scope, Send

from ..lifecycle import Readiness
from .context import request_id_var
from .errors import error_payload

log = logging.getLogger(__name__)

# A client-sent X-Request-Id is kept only when short and plain; anything else
# is replaced, so a header cannot inject content into the logs.
_VALID_REQUEST_ID = re.compile(r"^[A-Za-z0-9._-]{1,64}$")
HEALTH_PREFIX = "/api/v1/health/"


async def send_json(send: Send, status: int, payload: dict[str, Any]) -> None:
    body = json.dumps(payload).encode()
    await send(
        {
            "type": "http.response.start",
            "status": status,
            "headers": [
                (b"content-type", b"application/json"),
                (b"content-length", str(len(body)).encode()),
            ],
        }
    )
    await send({"type": "http.response.body", "body": body})


class RequestContextMiddleware:
    def __init__(self, app: ASGIApp, readiness: Readiness) -> None:
        self.app = app
        self.readiness = readiness

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return
        headers = dict(scope.get("headers", []))
        rid = headers.get(b"x-request-id", b"").decode("latin-1")
        if not _VALID_REQUEST_ID.match(rid):
            rid = secrets.token_hex(8)
        token = request_id_var.set(rid)
        status = 500
        started = False

        async def send_with_id(message: Message) -> None:
            nonlocal status, started
            if message["type"] == "http.response.start":
                started = True
                status = message["status"]
                message.setdefault("headers", [])
                message["headers"] = [*message["headers"], (b"x-request-id", rid.encode())]
            await send(message)

        begin = time.perf_counter()
        try:
            await self.app(scope, receive, send_with_id)
        except asyncio.CancelledError:
            # Uvicorn cancels requests still running when the graceful
            # shutdown timeout ends; the process then reports an incomplete stop.
            if self.readiness.state == "draining":
                self.readiness.mark_interrupted()
            raise
        except Exception as exc:
            span = trace.get_current_span()
            span.record_exception(exc)
            span.set_status(StatusCode.ERROR, "unhandled exception")
            log.exception("unhandled error serving request", extra={"path": scope["path"]})
            if not started:
                await send_json(send_with_id, 500, error_payload("internal", "internal error"))
        finally:
            path: str = scope["path"]
            log.log(
                logging.DEBUG if path.startswith(HEALTH_PREFIX) else logging.INFO,
                "request",
                extra={
                    "method": scope.get("method"),
                    "path": path,  # never the query string
                    "status": status,
                    "duration_ms": round((time.perf_counter() - begin) * 1000, 2),
                },
            )
            request_id_var.reset(token)


class BodyLimitMiddleware:
    """Buffers the request body up to `max_bytes` and answers 413 beyond it,
    whether the client announced a Content-Length or streams chunks."""

    def __init__(self, app: ASGIApp, max_bytes: int) -> None:
        self.app = app
        self.max_bytes = max_bytes

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return
        declared = dict(scope.get("headers", [])).get(b"content-length")
        if declared is not None and declared.isdigit() and int(declared) > self.max_bytes:
            await send_json(send, 413, error_payload("body_too_large", "request body too large"))
            return
        chunks: list[bytes] = []
        size = 0
        while True:
            message = await receive()
            if message["type"] == "http.disconnect":
                return
            chunk = message.get("body", b"")
            size += len(chunk)
            if size > self.max_bytes:
                await send_json(
                    send, 413, error_payload("body_too_large", "request body too large")
                )
                return
            chunks.append(chunk)
            if not message.get("more_body", False):
                break
        replayed = False

        async def replay() -> Message:
            nonlocal replayed
            if not replayed:
                replayed = True
                return {"type": "http.request", "body": b"".join(chunks), "more_body": False}
            return await receive()

        await self.app(scope, replay, send)

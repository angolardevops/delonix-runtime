"""Middleware, in the order config/settings.py installs them."""

from __future__ import annotations

import logging
import re
import secrets
import time
from collections.abc import Callable

from django.http import HttpRequest, HttpResponse

from core import context, http, views

log = logging.getLogger("core.request")

HEALTH_PREFIX = "/api/v1/health/"
_PROBES = {
    HEALTH_PREFIX + "live": views.live,
    HEALTH_PREFIX + "ready": views.ready,
}
# A client-sent X-Request-ID is kept only if it is short and plain; anything
# else is replaced, so a header cannot inject content into the logs.
_VALID_REQUEST_ID = re.compile(r"^[A-Za-z0-9._-]{1,64}$")

Middleware = Callable[[HttpRequest], HttpResponse]


def _log_request(request: HttpRequest, response: HttpResponse, start: float) -> None:
    """One access-log line: method, path — never the query string, headers or
    body — route, status and duration. Health probes log at debug."""
    match = getattr(request, "resolver_match", None)
    log.log(
        logging.DEBUG if request.path.startswith(HEALTH_PREFIX) else logging.INFO,
        "request",
        extra={
            "method": request.method,
            "path": request.path,
            "route": "/" + match.route if match else "",
            "status": response.status_code,
            "duration_ms": round((time.monotonic() - start) * 1000, 1),
        },
    )
    request._access_logged = True  # type: ignore[attr-defined]
    # This line is the record of the response; without the mark, Django's
    # django.request logger would add its own line for every 4xx/5xx.
    response._has_been_logged = True  # type: ignore[attr-defined]


def health_probes(get_response: Middleware) -> Middleware:
    """Outermost: answer the probes before tracing, host validation and the
    https redirect. A runtime probes 127.0.0.1 (or the container address)
    over plain HTTP, and a probe refused as a DisallowedHost or redirected to
    https would restart a healthy process."""

    def middleware(request: HttpRequest) -> HttpResponse:
        probe = _PROBES.get(request.path)
        if probe is not None and request.method in ("GET", "HEAD"):
            start = time.monotonic()
            response = probe(request)
            _log_request(request, response, start)
            return response
        return get_response(request)

    return middleware


def request_id(get_response: Middleware) -> Middleware:
    """Request id: header in (if plain), header out, in every log line and
    error body. Runs outside the tracing middleware, so it also covers what
    that middleware refuses (a disallowed Host)."""

    def middleware(request: HttpRequest) -> HttpResponse:
        rid = request.headers.get("X-Request-ID", "")
        if not _VALID_REQUEST_ID.match(rid):
            rid = secrets.token_hex(8)
        token = context.request_id.set(rid)
        start = time.monotonic()
        try:
            response = get_response(request)
            response["X-Request-ID"] = rid
            if not getattr(request, "_access_logged", False):
                _log_request(request, response, start)  # refused before access_log ran
            return response
        finally:
            context.request_id.reset(token)

    return middleware


def access_log(get_response: Middleware) -> Middleware:
    """Runs inside the tracing middleware (core.telemetry inserts it just
    before this one), so the "request" line carries the request's trace id."""

    def middleware(request: HttpRequest) -> HttpResponse:
        start = time.monotonic()
        response = get_response(request)
        _log_request(request, response, start)
        return response

    return middleware


def json_errors(get_response: Middleware) -> Middleware:
    """An exception a view did not handle: logged with its stack, answered as
    a bare 500 in the error shape (the stack never reaches the client)."""

    def middleware(request: HttpRequest) -> HttpResponse:
        return get_response(request)

    def process_exception(request: HttpRequest, exc: Exception) -> HttpResponse:
        log.error("unhandled exception", exc_info=exc, extra={"path": request.path})
        return http.error(500, "internal", "internal error")

    middleware.process_exception = process_exception  # type: ignore[attr-defined]
    return middleware

"""The API's HTTP conventions: one error shape, strict JSON bodies, and views
that answer only the methods they declare.

Error body, for every error the API returns:
    {"error": {"code": "...", "message": "...", "fields": {...}?, "request_id": "..."}}
"""

from __future__ import annotations

import json
from collections.abc import Callable
from typing import Any

from django.core.exceptions import RequestDataTooBig
from django.http import HttpRequest, HttpResponse, JsonResponse
from django.views.decorators.csrf import csrf_exempt

from core import context


def error(status: int, code: str, message: str, fields: dict[str, str] | None = None) -> JsonResponse:
    body: dict[str, Any] = {"code": code, "message": message}
    if fields:
        body["fields"] = fields
    body["request_id"] = context.request_id.get()
    return JsonResponse({"error": body}, status=status)


class BadRequestBody(Exception):
    """The body is not the JSON the endpoint expects (400 malformed_json) or
    is too large (413 body_too_large)."""

    def __init__(self, status: int, code: str, message: str) -> None:
        super().__init__(message)
        self.response_args = (status, code, message)

    def response(self) -> JsonResponse:
        return error(*self.response_args)


def _no_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    obj: dict[str, Any] = {}
    for key, value in pairs:
        if key in obj:
            raise ValueError(f"duplicate key {key!r}")
        obj[key] = value
    return obj


def _no_constants(name: str) -> Any:
    raise ValueError(f"{name} is not JSON")


def loads_strict(raw: bytes) -> Any:
    """Parse JSON refusing what the standard allows parsers to be lenient
    about: invalid UTF-8, duplicate keys, NaN/Infinity."""
    return json.loads(
        raw.decode("utf-8"),
        object_pairs_hook=_no_duplicate_keys,
        parse_constant=_no_constants,
    )


def read_body(request: HttpRequest) -> bytes:
    """The raw body, or BadRequestBody(413) past DATA_UPLOAD_MAX_MEMORY_SIZE."""
    try:
        return request.body
    except RequestDataTooBig:
        raise BadRequestBody(413, "body_too_large", "request body too large") from None


def json_object(request: HttpRequest, allowed: set[str]) -> dict[str, Any]:
    """The body as a JSON object with only `allowed` keys. Unknown keys,
    trailing data, and any non-object are refused (400), like the contract's
    `additionalProperties: false`."""
    raw = read_body(request)
    try:
        value = loads_strict(raw)
    except ValueError:  # includes JSONDecodeError and UnicodeDecodeError
        raise BadRequestBody(400, "malformed_json", "request body is not the expected JSON") from None
    if not isinstance(value, dict) or set(value) - allowed:
        raise BadRequestBody(400, "malformed_json", "request body is not the expected JSON")
    return value


Handler = Callable[..., HttpResponse]


def api_view(**handlers: Handler) -> Handler:
    """One URL, one handler per HTTP method: `api_view(GET=list_, POST=create)`.

    Other methods answer 405 in the error shape with an Allow header. The
    view is csrf_exempt: the API is for non-browser clients that send no
    cookies, so there is no ambient credential for a forged request to use
    (see ARCHITECTURE.md). `allowed_methods` is read by the contract test.
    """
    allowed = sorted(handlers)

    @csrf_exempt
    def view(request: HttpRequest, *args: Any, **kwargs: Any) -> HttpResponse:
        handler = handlers.get(request.method or "")
        if handler is None:
            response = error(405, "method_not_allowed", f"method not allowed; use {', '.join(allowed)}")
            response["Allow"] = ", ".join(allowed)
            return response
        try:
            return handler(request, *args, **kwargs)
        except BadRequestBody as exc:
            return exc.response()

    view.allowed_methods = allowed  # type: ignore[attr-defined]
    return view

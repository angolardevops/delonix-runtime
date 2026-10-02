"""One error shape for every endpoint:
`{"error": {"code", "message", "fields"?, "request_id"}}`.

FastAPI's own errors (validation, unknown routes, wrong methods) and the
domain errors are translated here; unexpected exceptions are answered by
`middleware.RequestContextMiddleware` as a bare 500.
"""

from __future__ import annotations

from typing import Any

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from starlette.exceptions import HTTPException as StarletteHTTPException

from ..notes.domain import NoteNotFoundError, ValidationFailedError
from .context import current_request_id


class ErrorDetail(BaseModel):
    code: str
    message: str
    fields: dict[str, str] | None = None
    request_id: str | None = None


class ErrorBody(BaseModel):
    error: ErrorDetail


def error_payload(code: str, message: str, fields: dict[str, str] | None = None) -> dict[str, Any]:
    detail: dict[str, Any] = {"code": code, "message": message}
    if fields:
        detail["fields"] = fields
    detail["request_id"] = current_request_id()
    return {"error": detail}


def error_response(
    status: int, code: str, message: str, fields: dict[str, str] | None = None
) -> JSONResponse:
    return JSONResponse(error_payload(code, message, fields), status_code=status)


# Referenced by the routes' `responses=`, so the OpenAPI contract documents the
# shape clients actually get instead of FastAPI's default 422 model.
_INTERNAL: dict[int | str, dict[str, Any]] = {
    500: {"model": ErrorBody, "description": "Unexpected error (`internal`)."},
}
INVALID_INPUT: dict[int | str, dict[str, Any]] = {
    422: {"model": ErrorBody, "description": "Invalid input (`validation_failed`)."},
    **_INTERNAL,
}
NOT_FOUND: dict[int | str, dict[str, Any]] = {
    404: {"model": ErrorBody, "description": "No such resource (`not_found`)."},
    **_INTERNAL,
}
INVALID_BODY: dict[int | str, dict[str, Any]] = {
    400: {"model": ErrorBody, "description": "Malformed JSON body (`malformed_json`)."},
    413: {"model": ErrorBody, "description": "Body over the limit (`body_too_large`)."},
    **INVALID_INPUT,
}

_CODES_BY_STATUS = {404: "not_found", 405: "method_not_allowed"}


def _field_name(loc: tuple[Any, ...]) -> str:
    # ("body", "title") -> "title"; ("query", "limit") -> "limit"
    return ".".join(str(part) for part in loc[1:]) or str(loc[0])


def install_error_handlers(app: FastAPI) -> None:
    @app.exception_handler(RequestValidationError)
    async def _validation(_: Request, exc: RequestValidationError) -> JSONResponse:
        errors = exc.errors()
        # A body that is not JSON, or JSON that is not an object, fails at the
        # location ("body",): the request as a whole is malformed, not a field.
        if any(
            e.get("type") == "json_invalid" or tuple(e.get("loc", ())) == ("body",) for e in errors
        ):
            return error_response(400, "malformed_json", "request body is not the expected JSON")
        fields: dict[str, str] = {}
        for e in errors:
            fields.setdefault(_field_name(tuple(e.get("loc", ("request",)))), str(e.get("msg", "")))
        return error_response(422, "validation_failed", "invalid input", fields)

    @app.exception_handler(ValidationFailedError)
    async def _domain_validation(_: Request, exc: ValidationFailedError) -> JSONResponse:
        return error_response(422, "validation_failed", "invalid input", exc.fields)

    @app.exception_handler(NoteNotFoundError)
    async def _not_found(_: Request, exc: NoteNotFoundError) -> JSONResponse:
        return error_response(404, "not_found", "note not found")

    @app.exception_handler(StarletteHTTPException)
    async def _http(_: Request, exc: StarletteHTTPException) -> JSONResponse:
        code = _CODES_BY_STATUS.get(exc.status_code, "http_error")
        message = "no such route" if exc.status_code == 404 else str(exc.detail)
        response = error_response(exc.status_code, code, message)
        if exc.headers:
            response.headers.update(exc.headers)
        return response

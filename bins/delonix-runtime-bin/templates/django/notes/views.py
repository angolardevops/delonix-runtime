"""HTTP transport of the notes capability: decode, call a service, encode.
No rule lives here, and no ORM call (tests/test_architecture.py)."""

from __future__ import annotations

import re

from django.http import HttpRequest, JsonResponse

from core import http
from notes import services

_INTEGER = re.compile(r"^-?[0-9]{1,9}$")


def _use_case_error(exc: Exception) -> JsonResponse:
    if isinstance(exc, services.ValidationFailed):
        return http.error(422, "validation_failed", "invalid input", exc.fields)
    if isinstance(exc, services.NoteNotFound):
        return http.error(404, "not_found", "note not found")
    raise exc


def create(request: HttpRequest) -> JsonResponse:
    data = http.json_object(request, allowed={"title", "body"})
    try:
        note = services.create_note(title=data.get("title"), body=data.get("body", ""))
    except services.ValidationFailed as exc:
        return _use_case_error(exc)
    response = JsonResponse(note.as_dict(), status=201)
    response["Location"] = f"/api/v1/notes/{note.as_dict()['id']}"
    return response


def list_(request: HttpRequest) -> JsonResponse:
    fields: dict[str, str] = {}
    params: dict[str, int] = {"offset": 0, "limit": services.DEFAULT_PAGE_SIZE}
    for name in params:
        raw = request.GET.get(name)
        if raw is None:
            continue
        if _INTEGER.match(raw):
            params[name] = int(raw)
        else:
            fields[name] = "must be an integer"
    if fields:
        return http.error(422, "validation_failed", "invalid input", fields)
    try:
        page = services.list_notes(offset=params["offset"], limit=params["limit"])
    except services.ValidationFailed as exc:
        return _use_case_error(exc)
    return JsonResponse(
        {
            "items": [n.as_dict() for n in page.items],
            "total": page.total,
            "offset": page.offset,
            "limit": page.limit,
        }
    )


def detail(request: HttpRequest, note_id: str) -> JsonResponse:
    try:
        note = services.get_note(note_id)
    except services.NoteNotFound as exc:
        return _use_case_error(exc)
    return JsonResponse(note.as_dict())

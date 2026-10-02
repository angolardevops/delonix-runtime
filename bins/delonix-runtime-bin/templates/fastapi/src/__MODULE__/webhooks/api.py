"""Inbound webhooks: `POST /api/v1/webhooks/inbound`.

Enabled only when `WEBHOOK_INBOUND_SECRET` is set (404 otherwise). Verified
over the raw request bytes; a timestamp more than 5 minutes off is refused; a
`webhook-id` already accepted is acknowledged again without being processed
again. Events: `ping` (proves the signature) and `note.create` (runs the same
use case as `POST /api/v1/notes`).
"""

from __future__ import annotations

import json
import logging
from typing import Any

from fastapi import APIRouter, Request, Response, status
from pydantic import ValidationError

from ..notes.api import NoteIn, get_notes
from ..web.errors import INVALID_BODY, ErrorBody, error_response
from .dedup import Dedup
from .signature import (
    HEADER_ID,
    HEADER_SIGNATURE,
    HEADER_TIMESTAMP,
    MissingHeadersError,
    WebhookError,
    verify,
)

log = logging.getLogger(__name__)

#: An inbound delivery is bounded independently of (and below) the API limit.
WEBHOOK_MAX_BODY = 256 * 1024

router = APIRouter(prefix="/api/v1/webhooks", tags=["webhooks"])

_EVENT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["type"],
    "properties": {
        "type": {"type": "string", "enum": ["ping", "note.create"]},
        "data": {"$ref": "#/components/schemas/NoteIn"},
    },
}


@router.post(
    "/inbound",
    status_code=status.HTTP_204_NO_CONTENT,
    summary="Receive a signed Standard Webhooks delivery",
    responses={
        **INVALID_BODY,
        401: {"model": ErrorBody, "description": "Signature or timestamp not valid."},
        404: {"model": ErrorBody, "description": "Inbound webhooks are not enabled."},
    },
    openapi_extra={
        "parameters": [
            {"name": h, "in": "header", "required": True, "schema": {"type": "string"}}
            for h in (HEADER_ID, HEADER_TIMESTAMP, HEADER_SIGNATURE)
        ],
        "requestBody": {
            "required": True,
            "content": {"application/json": {"schema": _EVENT_SCHEMA}},
        },
    },
)
async def inbound(request: Request) -> Response:
    key: bytes | None = request.app.state.inbound_webhook_key
    if key is None:
        return error_response(404, "not_found", "no such route")
    # The raw bytes, exactly as signed: never a re-parsed body.
    body = await request.body()
    if len(body) > WEBHOOK_MAX_BODY:
        return error_response(413, "body_too_large", "request body too large")
    msg_id = request.headers.get(HEADER_ID, "")
    try:
        verify(
            key,
            msg_id,
            request.headers.get(HEADER_TIMESTAMP, ""),
            request.headers.get(HEADER_SIGNATURE, ""),
            body,
            request.app.state.clock(),
        )
    except MissingHeadersError:
        return error_response(400, "missing_signature", "webhook headers are missing")
    except WebhookError as exc:
        log.warning("webhook rejected", extra={"reason": str(exc)})
        return error_response(401, "invalid_signature", "webhook signature is not valid")

    dedup: Dedup = request.app.state.dedup
    if not dedup.first_seen(msg_id, request.app.state.clock()):
        return Response(status_code=204)
    try:
        response = await _process(request, body)
    except BaseException:
        dedup.forget(msg_id)  # not processed: a retry of this id must be processed
        raise
    if response.status_code >= 400:
        dedup.forget(msg_id)
    return response


async def _process(request: Request, body: bytes) -> Response:
    try:
        event = json.loads(body)
    except ValueError:
        return error_response(400, "malformed_json", "webhook body is not JSON")
    kind = event.get("type") if isinstance(event, dict) else None
    if kind == "ping":
        return Response(status_code=204)
    if kind == "note.create":
        try:
            note_in = NoteIn.model_validate(event.get("data"))
        except ValidationError:
            return error_response(400, "malformed_json", "webhook data is not a note")
        # A domain error (an empty title) propagates to the shared handler.
        await get_notes(request).create(note_in.title, note_in.body)
        return Response(status_code=204)
    return error_response(422, "unknown_event", "unknown webhook event type")

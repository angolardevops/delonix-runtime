"""POST /api/v1/webhooks/inbound — only served when WEBHOOK_INBOUND_SECRET is
set (404 otherwise)."""

from __future__ import annotations

import logging
import time

from django.conf import settings
from django.http import HttpRequest, HttpResponse

from core import http
from notes import services as notes
from webhooks import inbound, signature

log = logging.getLogger("webhooks.views")

# An inbound delivery is bounded independently of the API body limit.
MAX_BODY = 256 * 1024


def receive(request: HttpRequest) -> HttpResponse:
    key = settings.APP.webhook_inbound_key
    if key is None:
        return http.error(404, "not_found", "no such route")
    declared = request.META.get("CONTENT_LENGTH") or "0"
    if declared.isdigit() and int(declared) > MAX_BODY:
        return http.error(413, "body_too_large", "request body too large")
    body = http.read_body(request)  # raw bytes: the signature covers them
    if len(body) > MAX_BODY:
        return http.error(413, "body_too_large", "request body too large")
    msg_id = request.headers.get(signature.HEADER_ID, "")
    try:
        signature.verify(
            key,
            msg_id,
            request.headers.get(signature.HEADER_TIMESTAMP, ""),
            request.headers.get(signature.HEADER_SIGNATURE, ""),
            body,
            time.time(),
        )
    except signature.MissingHeaders:
        return http.error(400, "missing_signature", "webhook headers are missing")
    except (signature.BadTimestamp, signature.BadSignature) as exc:
        log.warning("webhook rejected", extra={"reason": type(exc).__name__})
        return http.error(401, "invalid_signature", "webhook signature is not valid")
    try:
        event = http.loads_strict(body)
    except ValueError:
        return http.error(400, "malformed_json", "webhook body is not JSON")
    if not isinstance(event, dict):
        return http.error(400, "malformed_json", "webhook body is not a JSON object")
    try:
        inbound.handle(msg_id, event)
    except inbound.UnknownEvent:
        return http.error(422, "unknown_event", "unknown webhook event type")
    except notes.ValidationFailed as exc:
        return http.error(422, "validation_failed", "invalid input", exc.fields)
    return HttpResponse(status=204)

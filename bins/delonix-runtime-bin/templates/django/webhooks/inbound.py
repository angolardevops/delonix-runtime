"""Processing of a verified inbound delivery.

De-duplication and processing share one database transaction: the delivery
id is recorded only if processing succeeds, so a sender's retry of a failed
delivery is processed again, and a retry of an accepted one is acknowledged
without being processed twice (the unique key on `webhook_id`).
"""

from __future__ import annotations

import logging
from datetime import timedelta
from typing import Any

from django.db import IntegrityError, transaction
from django.utils import timezone

from notes import services as notes
from webhooks.models import InboundDelivery
from webhooks.signature import TOLERANCE_SECONDS

log = logging.getLogger("webhooks.inbound")

EVENT_TYPES = ("ping", "note.create")


class UnknownEvent(Exception):
    pass


def handle(msg_id: str, event: dict[str, Any]) -> bool:
    """Process one delivery. Returns False when `msg_id` was already
    processed. Raises UnknownEvent or notes.ValidationFailed (nothing is
    recorded then)."""
    kind = event.get("type")
    if kind not in EVENT_TYPES:
        raise UnknownEvent(str(kind))
    try:
        with transaction.atomic():
            InboundDelivery.objects.create(webhook_id=msg_id, received_at=timezone.now())
            if kind == "note.create":
                data = event.get("data")
                if not isinstance(data, dict):
                    raise notes.ValidationFailed({"data": "must be an object with a title"})
                notes.create_note(title=data.get("title"), body=data.get("body", ""))
    except IntegrityError:
        log.info("duplicate webhook acknowledged", extra={"webhook_id": msg_id})
        return False
    # Ids older than twice the replay window can never be replayed (the
    # timestamp check refuses them first), so they need not be kept.
    cutoff = timezone.now() - timedelta(seconds=2 * TOLERANCE_SECONDS)
    InboundDelivery.objects.filter(received_at__lt=cutoff).delete()
    log.info("webhook processed", extra={"webhook_id": msg_id, "event": kind})
    return True

"""Turns notes' events into RabbitMQ messages, when RABBITMQ_URL is set.

Mirrors webhooks/receivers.py: the publisher is built once per process, on the
first event, so its thread is started in the process that serves (a gunicorn
worker), never in the master before the fork. With RABBITMQ_URL unset the
publisher is never built and the signal handler is a no-op.
"""

from __future__ import annotations

import threading
from typing import Any

from django.conf import settings

from messaging.events import EVENT_TYPE
from messaging.publisher import Publisher
from notes.signals import note_created

_lock = threading.Lock()
_publisher: Publisher | None = None


def publisher() -> Publisher | None:
    """The process's publisher, or None when RabbitMQ pub/sub is off."""
    global _publisher
    url = settings.APP.rabbitmq_url
    if not url:
        return None
    with _lock:
        if _publisher is None:
            _publisher = Publisher(url)
        return _publisher


def on_note_created(sender: Any, note: Any, **kwargs: Any) -> None:
    target = publisher()
    if target is not None:
        data = note.as_dict()
        target.publish(EVENT_TYPE, data["id"], data)


def connect() -> None:
    note_created.connect(on_note_created, dispatch_uid="messaging.note_created")


def close(timeout: float) -> bool:
    """Drain the publish queue (called at worker exit)."""
    with _lock:
        target = _publisher
    return True if target is None else target.close(timeout)

"""Turns notes' events into outbound webhooks, when WEBHOOK_TARGET_URL is set.

The dispatcher is built once per process, on the first event, so its thread
is started in the process that serves (a gunicorn worker), never in the
master before the fork.
"""

from __future__ import annotations

import threading
from typing import Any

from django.conf import settings

from notes.signals import note_created
from webhooks.dispatcher import Dispatcher, DispatcherConfig

_lock = threading.Lock()
_dispatcher: Dispatcher | None = None


def dispatcher() -> Dispatcher | None:
    """The process's dispatcher, or None when outbound webhooks are off."""
    global _dispatcher
    app = settings.APP
    if not app.webhook_target_url or app.webhook_target_key is None:
        return None
    with _lock:
        if _dispatcher is None:
            _dispatcher = Dispatcher(DispatcherConfig(url=app.webhook_target_url, key=app.webhook_target_key))
        return _dispatcher


def on_note_created(sender: Any, note: Any, **kwargs: Any) -> None:
    target = dispatcher()
    if target is not None:
        data = note.as_dict()
        target.publish("note.created", data["id"], data)


def connect() -> None:
    note_created.connect(on_note_created, dispatch_uid="webhooks.note_created")


def close(timeout: float) -> bool:
    """Drain the outbound queue (called at worker exit)."""
    with _lock:
        target = _dispatcher
    return True if target is None else target.close(timeout)

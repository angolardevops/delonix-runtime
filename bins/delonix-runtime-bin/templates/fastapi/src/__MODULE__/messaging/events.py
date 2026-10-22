"""The `note.created` event, on the wire. Standard library only.

One place that says what the event looks like, so the publisher and the
consumer cannot drift. The envelope matches the outbound webhook's
(`type` + `data`), so a receiver sees the same shape on either transport."""

from __future__ import annotations

import json
from typing import Any

from ..notes.domain import Note

EVENT_TYPE = "note.created"
#: Topic-exchange routing key. A consumer binds `note.*` or `note.created`.
ROUTING_KEY = "note.created"


def note_created(note: Note) -> dict[str, Any]:
    created = note.created_at.isoformat(timespec="microseconds").replace("+00:00", "Z")
    return {
        "type": EVENT_TYPE,
        "data": {"id": note.id, "title": note.title, "body": note.body, "created_at": created},
    }


def encode(event: dict[str, Any]) -> bytes:
    return json.dumps(event, separators=(",", ":")).encode()


def decode(body: bytes) -> dict[str, Any]:
    """Parse a message body. Raises `ValueError` on anything that is not a
    JSON object with a string `type` — a poison message the consumer rejects."""
    try:
        parsed = json.loads(body)
    except json.JSONDecodeError as exc:
        raise ValueError(f"not JSON: {exc}") from exc
    if not isinstance(parsed, dict) or not isinstance(parsed.get("type"), str):
        raise ValueError("not a valid event envelope")
    return parsed

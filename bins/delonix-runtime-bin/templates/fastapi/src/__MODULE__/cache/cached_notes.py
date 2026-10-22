"""Cache-aside over the notes use cases: `get(id)` reads through the cache.

Wraps the use cases from the OUTSIDE, exactly like `observability.traced`, so
the notes package never imports the cache. A note is immutable once created,
so a cached note is never stale — only the TTL expires it. Only `get` is
cached: `list` changes whenever a note is created, and `create` returns a note
the caller already holds (it is written to the cache so the first read is a
hit). The cache is a slow path, never a failure path — `ports.Cache` swallows
backend errors, and a decode error here falls through to the store too.
"""

from __future__ import annotations

import json
import logging
from datetime import datetime
from typing import Protocol

from ..notes.domain import DEFAULT_PAGE_SIZE, Note, Page
from .ports import Cache

log = logging.getLogger(__name__)

_KEY_PREFIX = "note:v1:"


class _Notes(Protocol):
    """The use cases this decorator wraps (`NoteService` and `TracedNotes` fit)."""

    async def create(self, title: str, body: str = "") -> Note: ...
    async def get(self, note_id: str) -> Note: ...
    async def list(self, offset: int = 0, limit: int = DEFAULT_PAGE_SIZE) -> Page: ...


def _encode(note: Note) -> bytes:
    return json.dumps(
        {
            "id": note.id,
            "title": note.title,
            "body": note.body,
            "created_at": note.created_at.isoformat(),
        },
        separators=(",", ":"),
    ).encode()


def _decode(raw: bytes) -> Note:
    data = json.loads(raw)
    return Note(
        id=data["id"],
        title=data["title"],
        body=data["body"],
        created_at=datetime.fromisoformat(data["created_at"]),
    )


class CachedNotes:
    def __init__(self, inner: _Notes, cache: Cache, *, ttl_seconds: float) -> None:
        self._inner = inner
        self._cache = cache
        self._ttl = ttl_seconds

    async def create(self, title: str, body: str = "") -> Note:
        note = await self._inner.create(title, body)
        await self._cache.set(_KEY_PREFIX + note.id, _encode(note), self._ttl)
        return note

    async def get(self, note_id: str) -> Note:
        key = _KEY_PREFIX + note_id
        raw = await self._cache.get(key)
        if raw is not None:
            try:
                return _decode(raw)
            except (ValueError, KeyError):
                # A corrupt or old-shape entry: ignore it and read the store.
                log.warning("discarding an undecodable cache entry", extra={"note_id": note_id})
        note = await self._inner.get(note_id)  # raises NoteNotFoundError on a miss
        await self._cache.set(key, _encode(note), self._ttl)
        return note

    async def list(self, offset: int = 0, limit: int = DEFAULT_PAGE_SIZE) -> Page:
        return await self._inner.list(offset, limit)

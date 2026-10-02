"""The use cases of the notes capability. Standard library only."""

from __future__ import annotations

import secrets
from collections.abc import Callable
from datetime import UTC, datetime

from .domain import (
    MAX_BODY_LEN,
    MAX_PAGE_SIZE,
    MAX_TITLE_LEN,
    Note,
    NoteNotFoundError,
    Page,
    ValidationFailedError,
)
from .ports import NotePublisher, NoteStore


def _new_id() -> str:
    return secrets.token_hex(16)


def _now() -> datetime:
    return datetime.now(UTC)


class NoteService:
    """Create, read and list notes. `publisher=None` publishes nothing."""

    def __init__(
        self,
        store: NoteStore,
        publisher: NotePublisher | None = None,
        *,
        now: Callable[[], datetime] = _now,
        new_id: Callable[[], str] = _new_id,
    ) -> None:
        self._store = store
        self._publisher = publisher
        self._now = now
        self._new_id = new_id

    async def create(self, title: str, body: str = "") -> Note:
        title = title.strip()
        fields: dict[str, str] = {}
        if not title:
            fields["title"] = "is required"
        elif len(title) > MAX_TITLE_LEN:
            fields["title"] = f"must be at most {MAX_TITLE_LEN} characters"
        if len(body) > MAX_BODY_LEN:
            fields["body"] = f"must be at most {MAX_BODY_LEN} characters"
        if fields:
            raise ValidationFailedError(fields)
        note = Note(id=self._new_id(), title=title, body=body, created_at=self._now())
        await self._store.save(note)
        if self._publisher is not None:
            self._publisher.note_created(note)
        return note

    async def get(self, note_id: str) -> Note:
        note = await self._store.get(note_id)
        if note is None:
            raise NoteNotFoundError(note_id)
        return note

    async def list(self, offset: int = 0, limit: int = 20) -> Page:
        fields: dict[str, str] = {}
        if not 1 <= limit <= MAX_PAGE_SIZE:
            fields["limit"] = f"must be between 1 and {MAX_PAGE_SIZE}"
        if offset < 0:
            fields["offset"] = "must not be negative"
        if fields:
            raise ValidationFailedError(fields)
        items, total = await self._store.list(offset, limit)
        return Page(items=items, total=total, offset=offset, limit=limit)

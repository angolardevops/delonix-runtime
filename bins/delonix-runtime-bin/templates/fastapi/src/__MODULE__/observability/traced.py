"""One span per use case, so a trace shows the business step between the HTTP
span and the calls it makes — without the notes package importing telemetry
(the decorator wraps it from the outside)."""

from __future__ import annotations

from opentelemetry.trace import Tracer

from ..notes.domain import DEFAULT_PAGE_SIZE, Note, Page
from ..notes.service import NoteService


class TracedNotes:
    def __init__(self, inner: NoteService, tracer: Tracer) -> None:
        self._inner = inner
        self._tracer = tracer

    async def create(self, title: str, body: str = "") -> Note:
        with self._tracer.start_as_current_span("notes.create"):
            return await self._inner.create(title, body)

    async def get(self, note_id: str) -> Note:
        with self._tracer.start_as_current_span("notes.get"):
            return await self._inner.get(note_id)

    async def list(self, offset: int = 0, limit: int = DEFAULT_PAGE_SIZE) -> Page:
        with self._tracer.start_as_current_span("notes.list"):
            return await self._inner.list(offset, limit)

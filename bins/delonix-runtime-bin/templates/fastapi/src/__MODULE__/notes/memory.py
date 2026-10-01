"""In-memory adapter of `NoteStore`. Notes vanish on restart; replace it with a
database adapter for real use (see ARCHITECTURE.md)."""

from __future__ import annotations

from .domain import Note


class InMemoryNoteStore:
    """Safe inside one event loop: no method awaits between reading and writing."""

    def __init__(self) -> None:
        self._by_id: dict[str, Note] = {}
        self._order: list[str] = []  # insertion order, oldest first

    async def save(self, note: Note) -> None:
        if note.id not in self._by_id:
            self._order.append(note.id)
        self._by_id[note.id] = note

    async def get(self, note_id: str) -> Note | None:
        return self._by_id.get(note_id)

    async def list(self, offset: int, limit: int) -> tuple[list[Note], int]:
        newest_first = [self._by_id[i] for i in reversed(self._order)]
        return newest_first[offset : offset + limit], len(newest_first)

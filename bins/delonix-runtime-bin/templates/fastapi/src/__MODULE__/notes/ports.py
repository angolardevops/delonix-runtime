"""What the use cases need from the outside world, named by the use cases.

Adapters implement these structurally (no inheritance): `memory.InMemoryNoteStore`
for `NoteStore`, `webhooks.dispatcher.WebhookDispatcher` for `NotePublisher`.
"""

from __future__ import annotations

from typing import Protocol

from .domain import Note


class NoteStore(Protocol):
    """Persistence. A database adapter implements the same three methods."""

    async def save(self, note: Note) -> None: ...

    async def get(self, note_id: str) -> Note | None: ...

    async def list(self, offset: int, limit: int) -> tuple[list[Note], int]:
        """One page, newest first, and the total count."""
        ...


class NotePublisher(Protocol):
    """Tells the outside world a note exists. Must not wait on the network:
    the webhook adapter queues and returns."""

    def note_created(self, note: Note) -> None: ...

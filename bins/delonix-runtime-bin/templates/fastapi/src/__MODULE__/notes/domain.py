"""What a note is and the rules it obeys. Standard library only."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime

MAX_TITLE_LEN = 200
MAX_BODY_LEN = 10_000
MAX_PAGE_SIZE = 100
DEFAULT_PAGE_SIZE = 20


@dataclass(frozen=True, slots=True)
class Note:
    id: str
    title: str
    body: str
    created_at: datetime


@dataclass(frozen=True, slots=True)
class Page:
    items: list[Note]
    total: int
    offset: int
    limit: int


class NoteNotFoundError(LookupError):
    """No note with that id."""

    def __init__(self, note_id: str) -> None:
        super().__init__(f"note {note_id!r} not found")
        self.note_id = note_id


class ValidationFailedError(ValueError):
    """Every field that failed, so a client fixes a request in one round trip."""

    def __init__(self, fields: dict[str, str]) -> None:
        super().__init__("invalid input: " + "; ".join(f"{k}: {v}" for k, v in fields.items()))
        self.fields = fields

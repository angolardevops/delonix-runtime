"""HTTP transport of the notes capability: request/response schemas, the
router, and the dependency that hands it the use cases. No business rule
lives here — the limits are checked by `service.NoteService`."""

from __future__ import annotations

from datetime import datetime
from typing import Annotated, Protocol

from fastapi import APIRouter, Depends, Query, Request, Response, status
from pydantic import BaseModel, ConfigDict, Field

from ..web.errors import INVALID_BODY, INVALID_INPUT, NOT_FOUND
from .domain import (
    DEFAULT_PAGE_SIZE,
    MAX_BODY_LEN,
    MAX_PAGE_SIZE,
    MAX_TITLE_LEN,
    Note,
    Page,
)


class NotesUseCases(Protocol):
    """What the transport needs. `NoteService` and its traced decorator both fit."""

    async def create(self, title: str, body: str = "") -> Note: ...

    async def get(self, note_id: str) -> Note: ...

    async def list(self, offset: int = 0, limit: int = DEFAULT_PAGE_SIZE) -> Page: ...


def get_notes(request: Request) -> NotesUseCases:
    notes: NotesUseCases = request.app.state.notes
    return notes


NotesDep = Annotated[NotesUseCases, Depends(get_notes)]


class NoteIn(BaseModel):
    # Unknown fields are refused, so a typo in a field name is an error and
    # not a silently ignored value.
    model_config = ConfigDict(extra="forbid")

    title: str = Field(description=f"1 to {MAX_TITLE_LEN} characters after trimming.")
    body: str = Field(default="", description=f"At most {MAX_BODY_LEN} characters.")


class NoteOut(BaseModel):
    id: str
    title: str
    body: str
    created_at: datetime

    @classmethod
    def of(cls, note: Note) -> NoteOut:
        return cls(id=note.id, title=note.title, body=note.body, created_at=note.created_at)


class NotePage(BaseModel):
    items: list[NoteOut]
    total: int
    offset: int
    limit: int


router = APIRouter(prefix="/api/v1/notes", tags=["notes"])


@router.post(
    "",
    status_code=status.HTTP_201_CREATED,
    responses=INVALID_BODY,
    summary="Create a note",
)
async def create_note(payload: NoteIn, notes: NotesDep, response: Response) -> NoteOut:
    note = await notes.create(payload.title, payload.body)
    response.headers["Location"] = f"/api/v1/notes/{note.id}"
    return NoteOut.of(note)


@router.get("", responses=INVALID_INPUT, summary="List notes, newest first")
async def list_notes(
    notes: NotesDep,
    offset: Annotated[int, Query(ge=0)] = 0,
    limit: Annotated[int, Query(ge=1, le=MAX_PAGE_SIZE)] = DEFAULT_PAGE_SIZE,
) -> NotePage:
    page = await notes.list(offset, limit)
    return NotePage(
        items=[NoteOut.of(n) for n in page.items],
        total=page.total,
        offset=page.offset,
        limit=page.limit,
    )


@router.get("/{note_id}", responses=NOT_FOUND, summary="Get one note")
async def get_note(note_id: str, notes: NotesDep) -> NoteOut:
    return NoteOut.of(await notes.get(note_id))

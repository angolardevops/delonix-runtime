"""Use-case tests: the rules, without HTTP."""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from itertools import count

import pytest

from __MODULE__.notes.domain import (
    Note,
    NoteNotFoundError,
    ValidationFailedError,
)
from __MODULE__.notes.memory import (
    InMemoryNoteStore,
)
from __MODULE__.notes.service import (
    NoteService,
)

pytestmark = pytest.mark.anyio


class Recorder:
    def __init__(self) -> None:
        self.published: list[Note] = []

    def note_created(self, note: Note) -> None:
        self.published.append(note)


def service(publisher: Recorder | None = None) -> NoteService:
    ids = count(1)
    clock = count()
    start = datetime(2026, 1, 1, tzinfo=UTC)
    return NoteService(
        InMemoryNoteStore(),
        publisher,
        new_id=lambda: f"n{next(ids)}",
        now=lambda: start + timedelta(seconds=next(clock)),
    )


async def test_create_trims_the_title_stores_and_publishes() -> None:
    pub = Recorder()
    svc = service(pub)
    note = await svc.create("  hello  ", "world")
    assert note.title == "hello"
    assert await svc.get(note.id) == note
    assert pub.published == [note]


@pytest.mark.parametrize(
    ("title", "body", "fields"),
    [
        ("   ", "", {"title": "is required"}),
        ("x" * 201, "", {"title": "must be at most 200 characters"}),
        ("ok", "b" * 10_001, {"body": "must be at most 10000 characters"}),
        ("", "b" * 10_001, {"title": "is required", "body": "must be at most 10000 characters"}),
    ],
)
async def test_create_lists_every_invalid_field(
    title: str, body: str, fields: dict[str, str]
) -> None:
    pub = Recorder()
    with pytest.raises(ValidationFailedError) as err:
        await service(pub).create(title, body)
    assert err.value.fields == fields
    assert pub.published == []


async def test_title_limit_counts_characters_not_bytes() -> None:
    note = await service().create("é" * 200)
    assert len(note.title) == 200


async def test_get_unknown_raises_not_found() -> None:
    with pytest.raises(NoteNotFoundError):
        await service().get("missing")


async def test_list_is_newest_first_and_paginated() -> None:
    svc = service()
    for i in range(5):
        await svc.create(f"t{i}")
    page = await svc.list(offset=1, limit=2)
    assert [n.title for n in page.items] == ["t3", "t2"]
    assert (page.total, page.offset, page.limit) == (5, 1, 2)


@pytest.mark.parametrize(
    ("offset", "limit", "field"), [(0, 0, "limit"), (0, 101, "limit"), (-1, 10, "offset")]
)
async def test_list_refuses_out_of_range(offset: int, limit: int, field: str) -> None:
    with pytest.raises(ValidationFailedError) as err:
        await service().list(offset, limit)
    assert field in err.value.fields

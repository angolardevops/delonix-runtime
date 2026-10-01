"""The use cases, without HTTP."""

from __future__ import annotations

import pytest

from notes import services
from notes.signals import note_created

pytestmark = pytest.mark.django_db


def test_create_trims_the_title_and_stores_the_note(django_capture_on_commit_callbacks):
    with django_capture_on_commit_callbacks(execute=True):
        note = services.create_note(title="  hello ", body="world")
    assert note.title == "hello"
    assert services.get_note(note.id.hex).body == "world"


@pytest.mark.parametrize(
    ("title", "body", "fields"),
    [
        ("", "", {"title": "is required"}),
        ("   ", "", {"title": "is required"}),
        (None, "", {"title": "is required"}),
        (5, "", {"title": "must be a string"}),
        ("x" * 201, "", {"title": "must be at most 200 characters"}),
        ("", "b" * 10_001, {"title": "is required", "body": "must be at most 10000 characters"}),
        ("ok", 3, {"body": "must be a string"}),
    ],
)
def test_create_reports_every_invalid_field(title, body, fields):
    with pytest.raises(services.ValidationFailed) as info:
        services.create_note(title=title, body=body)
    assert info.value.fields == fields


def test_limits_are_characters_not_bytes():
    services.create_note(title="é" * 200)


def test_the_event_is_sent_once_after_commit(django_capture_on_commit_callbacks):
    seen = []

    def receiver(sender, note, **kwargs):
        seen.append(note.id)

    note_created.connect(receiver)
    try:
        with django_capture_on_commit_callbacks(execute=False) as callbacks:
            note = services.create_note(title="t")
        assert seen == []  # nothing before the commit
        for callback in callbacks:
            callback()
        assert seen == [note.id]
    finally:
        note_created.disconnect(receiver)


def test_list_pages_newest_first():
    ids = [services.create_note(title=f"n{i}").id for i in range(5)]
    page = services.list_notes(offset=1, limit=2)
    assert page.total == 5
    assert [n.id for n in page.items] == [ids[3], ids[2]]


@pytest.mark.parametrize(("offset", "limit"), [(0, 0), (0, 101), (-1, 10)])
def test_list_refuses_out_of_range_paging(offset, limit):
    with pytest.raises(services.ValidationFailed):
        services.list_notes(offset=offset, limit=limit)


@pytest.mark.parametrize("note_id", ["does-not-exist", "0" * 32])
def test_get_unknown_or_malformed_id_is_not_found(note_id):
    with pytest.raises(services.NoteNotFound):
        services.get_note(note_id)

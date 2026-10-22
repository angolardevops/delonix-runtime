"""The read-through cache over note lookups: a hit skips the database, a
create warms it, and a cache outage only makes reads fall through to the store.

These run with the default in-process cache (no REDIS_URL), so they need no
Redis; the graceful-degradation test forces a backend error by hand."""

from __future__ import annotations

import pytest
from django.core.cache import cache

from notes import services

pytestmark = pytest.mark.django_db


@pytest.fixture(autouse=True)
def _clear_cache():
    cache.clear()
    yield
    cache.clear()


def test_the_default_backend_is_the_in_process_cache_without_redis(settings):
    assert "locmem" in settings.CACHES["default"]["BACKEND"]


def test_a_get_is_served_from_the_cache_on_the_second_read(django_assert_num_queries):
    note = services.create_note(title="t", body="b")
    cache.clear()  # drop the warm-up entry so the first get reads the store
    with django_assert_num_queries(1):
        services.get_note(note.id.hex)
    with django_assert_num_queries(0):  # second read is a cache hit
        again = services.get_note(note.id.hex)
    assert again.id == note.id
    assert again.title == "t"


def test_create_warms_the_cache_so_the_first_get_is_a_hit(django_assert_num_queries):
    note = services.create_note(title="warm", body="")
    with django_assert_num_queries(0):
        got = services.get_note(note.id.hex)
    assert got.title == "warm"


def test_a_cache_outage_falls_through_to_the_store(monkeypatch, django_assert_num_queries):
    note = services.create_note(title="t", body="b")
    cache.clear()

    def boom(*args, **kwargs):
        raise RuntimeError("redis down")

    # Both reads and writes raise: the lookup must still return from the store.
    monkeypatch.setattr(services.cache, "get", boom)
    monkeypatch.setattr(services.cache, "set", boom)
    with django_assert_num_queries(1):
        got = services.get_note(note.id.hex)
    assert got.id == note.id


def test_a_miss_reads_the_store_and_raises_for_an_unknown_note():
    import uuid

    with pytest.raises(services.NoteNotFound):
        services.get_note(uuid.uuid4().hex)


def test_an_undecodable_cache_entry_is_ignored(monkeypatch, django_assert_num_queries):
    note = services.create_note(title="t", body="b")
    cache.clear()
    services.get_note(note.id.hex)  # warm it with a real Note
    # A wrong-shape entry (not a Note) must be ignored, not returned.
    monkeypatch.setattr(services.cache, "get", lambda *a, **k: {"not": "a note"})
    with django_assert_num_queries(1):
        got = services.get_note(note.id.hex)
    assert got.id == note.id

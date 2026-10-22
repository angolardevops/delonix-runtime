"""The cache: the in-process backend, the cache-aside decorator, and that a
backend failure is a slow path and never a failed request."""

from __future__ import annotations

import pytest

from __MODULE__.cache.cached_notes import _KEY_PREFIX, CachedNotes
from __MODULE__.cache.memory import InMemoryCache
from __MODULE__.cache.redis import RedisCache
from __MODULE__.notes.domain import DEFAULT_PAGE_SIZE, Note, NoteNotFoundError, Page
from __MODULE__.notes.memory import InMemoryNoteStore
from __MODULE__.notes.service import NoteService

pytestmark = pytest.mark.anyio


class Clock:
    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now


async def test_inmemory_roundtrip_and_miss() -> None:
    cache = InMemoryCache()
    assert await cache.get("absent") is None
    await cache.set("k", b"v", ttl_seconds=10)
    assert await cache.get("k") == b"v"


async def test_inmemory_entry_expires_after_ttl() -> None:
    clock = Clock()
    cache = InMemoryCache(clock=clock)
    await cache.set("k", b"v", ttl_seconds=10)
    clock.now = 9.9
    assert await cache.get("k") == b"v"
    clock.now = 10.0
    assert await cache.get("k") is None


async def test_inmemory_evicts_the_oldest_when_full() -> None:
    cache = InMemoryCache(max_entries=2)
    await cache.set("a", b"1", ttl_seconds=100)
    await cache.set("b", b"2", ttl_seconds=100)
    await cache.set("c", b"3", ttl_seconds=100)  # evicts "a"
    assert await cache.get("a") is None
    assert await cache.get("b") == b"2"
    assert await cache.get("c") == b"3"


async def test_inmemory_rejects_non_positive_ttl() -> None:
    cache = InMemoryCache()
    await cache.set("k", b"v", ttl_seconds=0)
    assert await cache.get("k") is None


class CountingNotes:
    """A use-case object that counts how often the store is actually read."""

    def __init__(self) -> None:
        self._inner = NoteService(InMemoryNoteStore())
        self.gets = 0

    async def create(self, title: str, body: str = "") -> Note:
        return await self._inner.create(title, body)

    async def get(self, note_id: str) -> Note:
        self.gets += 1
        return await self._inner.get(note_id)

    async def list(self, offset: int = 0, limit: int = DEFAULT_PAGE_SIZE) -> Page:
        return await self._inner.list(offset, limit)


async def test_create_prewarms_so_the_first_get_is_a_hit() -> None:
    inner = CountingNotes()
    cache = InMemoryCache()
    notes = CachedNotes(inner, cache, ttl_seconds=100)
    note = await notes.create("hello")
    got = await notes.get(note.id)
    assert got == note
    assert inner.gets == 0  # served entirely from the cache


async def test_second_get_is_served_from_the_cache() -> None:
    inner = CountingNotes()
    notes = CachedNotes(inner, InMemoryCache(), ttl_seconds=100)
    created = await inner.create("x")  # created on the inner only: cache is cold
    assert await notes.get(created.id) == created
    assert await notes.get(created.id) == created
    assert inner.gets == 1  # the store was read once; the second get was a hit


async def test_unknown_note_still_raises_through_the_cache() -> None:
    notes = CachedNotes(CountingNotes(), InMemoryCache(), ttl_seconds=100)
    with pytest.raises(NoteNotFoundError):
        await notes.get("nope")


async def test_a_corrupt_cache_entry_is_ignored() -> None:
    inner = CountingNotes()
    cache = InMemoryCache()
    created = await inner.create("x")
    await cache.set(_KEY_PREFIX + created.id, b"not json", ttl_seconds=100)
    notes = CachedNotes(inner, cache, ttl_seconds=100)
    assert await notes.get(created.id) == created  # fell through to the store
    assert inner.gets == 1


class BrokenCache:
    """Models the port contract under a backend outage: never raises."""

    async def get(self, key: str) -> bytes | None:
        return None

    async def set(self, key: str, value: bytes, ttl_seconds: float) -> None:
        return None

    async def healthy(self) -> bool:
        return False

    async def aclose(self) -> None:
        return None


async def test_a_dead_cache_backend_still_serves_from_the_store() -> None:
    inner = CountingNotes()
    created = await inner.create("x")
    notes = CachedNotes(inner, BrokenCache(), ttl_seconds=100)
    assert await notes.get(created.id) == created
    assert await notes.get(created.id) == created
    assert inner.gets == 2  # every read falls through; nothing is ever cached


class RaisingRedis:
    """A redis client whose every call raises, to prove RedisCache swallows it."""

    async def get(self, key: str) -> bytes | None:
        raise _redis_error()

    async def set(self, *args: object, **kwargs: object) -> None:
        raise _redis_error()

    async def ping(self) -> bool:
        raise _redis_error()

    async def aclose(self) -> None:
        raise _redis_error()


def _redis_error() -> Exception:
    from redis.exceptions import ConnectionError as RedisConnectionError

    return RedisConnectionError("down")


async def test_redis_cache_swallows_backend_errors() -> None:
    cache = RedisCache(RaisingRedis())  # type: ignore[arg-type]
    assert await cache.get("k") is None
    await cache.set("k", b"v", ttl_seconds=10)  # must not raise
    assert await cache.healthy() is False
    await cache.aclose()  # must not raise

"""What the cache decorator needs from a backend, named by the decorator.

Adapters implement this structurally (no inheritance): `memory.InMemoryCache`
and `redis.RedisCache`. Every method is total — an adapter MUST NOT raise on a
backend failure, it returns `None`/does nothing and logs, so a cache outage is
a slow path, never a failed request (see `cached_notes.CachedNotes`)."""

from __future__ import annotations

from typing import Protocol


class Cache(Protocol):
    """A bytes key/value store with per-entry expiry. Failures are swallowed."""

    async def get(self, key: str) -> bytes | None:
        """The stored value, or `None` on a miss OR any backend error."""
        ...

    async def set(self, key: str, value: bytes, ttl_seconds: float) -> None:
        """Store `value` under `key` for `ttl_seconds`. A no-op on error."""
        ...

    async def healthy(self) -> bool:
        """Whether the backend answers (for the readiness probe)."""
        ...

    async def aclose(self) -> None:
        """Release connections. Called once on shutdown."""
        ...

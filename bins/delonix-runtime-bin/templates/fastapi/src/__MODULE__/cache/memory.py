"""In-process `ports.Cache`. The default when no `REDIS_URL` is set.

Entries live in a dict with a monotonic-clock expiry; a bounded size evicts
the oldest first so a long run cannot grow without limit. Lost on restart and
not shared between replicas — for local runs, tests, and as the graceful
fallback when Redis is not configured. Safe inside one event loop: no method
awaits between reading and writing."""

from __future__ import annotations

import time
from collections import OrderedDict
from collections.abc import Callable


class InMemoryCache:
    def __init__(
        self, *, max_entries: int = 4096, clock: Callable[[], float] = time.monotonic
    ) -> None:
        self._by_key: OrderedDict[str, tuple[float, bytes]] = OrderedDict()  # oldest first
        self._max = max_entries
        self._clock = clock

    async def get(self, key: str) -> bytes | None:
        entry = self._by_key.get(key)
        if entry is None:
            return None
        expires_at, value = entry
        if self._clock() >= expires_at:
            del self._by_key[key]
            return None
        return value

    async def set(self, key: str, value: bytes, ttl_seconds: float) -> None:
        if ttl_seconds <= 0:
            return
        if key in self._by_key:
            del self._by_key[key]
        elif len(self._by_key) >= self._max:
            self._by_key.popitem(last=False)  # full: evict the oldest
        self._by_key[key] = (self._clock() + ttl_seconds, value)

    async def healthy(self) -> bool:
        return True

    async def aclose(self) -> None:
        self._by_key.clear()

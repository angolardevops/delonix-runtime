"""`ports.Cache` backed by Redis (redis-py's asyncio client).

Used only when `REDIS_URL` is set. Every call is bounded by a short socket
timeout and every backend error is swallowed: a miss on read, a no-op on
write. A down Redis therefore turns the cache off (every lookup falls through
to the store) instead of failing requests. The URL — which carries the
password — is NEVER logged; only the error type is, once per health
transition, so an outage does not flood the log.
"""

from __future__ import annotations

import contextlib
import logging
from typing import cast

import redis.asyncio as redis
from redis.exceptions import RedisError

log = logging.getLogger(__name__)


class RedisCache:
    def __init__(self, client: redis.Redis) -> None:
        self._redis = client
        self._degraded = False

    @classmethod
    def from_url(cls, url: str, *, timeout: float = 2.0) -> RedisCache:
        """Build a client from a `redis://` / `rediss://` URL. No connection is
        opened here — the first command connects, bounded by `timeout`."""
        client: redis.Redis = redis.from_url(
            url,
            socket_timeout=timeout,
            socket_connect_timeout=timeout,
            # A down Redis must fail fast so the lookup falls through; the
            # client reconnects by itself on the next command.
            retry_on_timeout=False,
            health_check_interval=30,
        )
        return cls(client)

    async def get(self, key: str) -> bytes | None:
        try:
            value = cast("bytes | None", await self._redis.get(key))
        except RedisError as exc:
            self._degrade(exc)
            return None
        self._recover()
        return value

    async def set(self, key: str, value: bytes, ttl_seconds: float) -> None:
        if ttl_seconds <= 0:
            return
        try:
            await self._redis.set(key, value, ex=max(1, int(ttl_seconds)))
        except RedisError as exc:
            self._degrade(exc)
            return
        self._recover()

    async def healthy(self) -> bool:
        try:
            return bool(await self._redis.ping())
        except RedisError:
            return False

    async def aclose(self) -> None:
        with contextlib.suppress(RedisError):  # best effort on shutdown
            await self._redis.aclose()

    def _degrade(self, exc: RedisError) -> None:
        if not self._degraded:
            self._degraded = True
            # type only: the message can carry the host, never the password.
            log.warning(
                "cache backend unavailable, serving from the store",
                extra={"error": type(exc).__name__},
            )

    def _recover(self) -> None:
        if self._degraded:
            self._degraded = False
            log.info("cache backend recovered")

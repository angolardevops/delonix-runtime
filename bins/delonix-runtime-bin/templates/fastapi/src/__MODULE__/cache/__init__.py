"""Read-through cache for note lookups.

A small, honest example of a cache-aside: `GET /api/v1/notes/{id}` checks the
cache first and falls back to the store on a miss. Notes are immutable once
created, so a cached note is never stale — the only expiry is the TTL.

The capability is OPTIONAL and degrades gracefully: with no `REDIS_URL` the
composition root wires `memory.InMemoryCache`, which keeps entries in the
process (lost on restart, not shared between replicas) — enough for local runs
and tests, and the app starts and serves with Redis down. The `ports.Cache`
protocol is what the decorator depends on; `redis.RedisCache` is the adapter
used when `REDIS_URL` is set.
"""

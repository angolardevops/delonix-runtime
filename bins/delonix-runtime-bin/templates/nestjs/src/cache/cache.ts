// What the cache-aside layer needs from a backend, named by that layer. An
// abstract class, so Nest can inject it by type and a module can bind it to
// whichever adapter the configuration selects (InMemoryCache or RedisCache).
//
// Every method is TOTAL: an adapter MUST NOT throw on a backend failure. It
// returns null / does nothing / reports unhealthy and logs, so a cache outage
// is a slow path (every lookup falls through to the store), never a failed
// request. See CachedNotesService for the cache-aside policy built on it.

export abstract class Cache {
  /** The stored value, or null on a miss OR any backend error. */
  abstract get(key: string): Promise<Buffer | null>;
  /** Store `value` under `key` for `ttlMs` milliseconds. A no-op on error or a non-positive TTL. */
  abstract set(key: string, value: Buffer, ttlMs: number): Promise<void>;
  /** Whether the backend answers (for the readiness probe). Never throws. */
  abstract healthy(): Promise<boolean>;
  /** Release connections. Called once on shutdown. Never throws. */
  abstract close(): Promise<void>;
}

/**
 * What the cache-aside decorator needs from a backend, named by the decorator.
 *
 * Adapters implement this structurally: `memory.InMemoryCache` and
 * `redis.RedisCache`. Every method is TOTAL — an adapter MUST NOT throw on a
 * backend failure, it returns `null` / does nothing and logs, so a cache
 * outage is a slow path, never a failed request (see `cached-notes.ts`).
 */
export interface Cache {
  /** The stored value, or `null` on a miss OR any backend error. */
  get(key: string): Promise<Buffer | null>;
  /** Store `value` under `key` for `ttlSeconds`. A no-op on error. */
  set(key: string, value: Buffer, ttlSeconds: number): Promise<void>;
  /** Whether the backend answers (for the readiness probe). */
  healthy(): Promise<boolean>;
  /** Release connections. Called once on shutdown. */
  close(): Promise<void>;
}

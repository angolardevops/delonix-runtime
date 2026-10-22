// A small string key/value store with per-entry expiry, named by the
// cache-aside wrapper (cached-notes.ts). Two adapters implement it: an
// in-process map (the default, and the fallback when no REDIS_URL is set) and
// Redis. Every method is TOTAL — an adapter never throws on a backend failure:
// a read returns undefined, a write is a no-op. A cache outage is therefore a
// slow path (every lookup falls through to the store), never a failed request.
import "server-only";

export interface Cache {
  /** The stored value, or undefined on a miss OR any backend error. */
  get(key: string): Promise<string | undefined>;
  /** Store `value` under `key` for `ttlSeconds`. A no-op on error or ttl <= 0. */
  set(key: string, value: string, ttlSeconds: number): Promise<void>;
  /** Whether the backend answers (for the readiness probe). */
  healthy(): Promise<boolean>;
  /** Release connections. Called once on shutdown. Never throws. */
  close(): Promise<void>;
}

/**
 * In-process cache. The default when no REDIS_URL is set. Entries live in a
 * Map (insertion-ordered) with a monotonic expiry; a bounded size evicts the
 * oldest first so a long run cannot grow without limit. Lost on restart and
 * not shared between replicas — for local runs, tests, and as the graceful
 * fallback when Redis is not configured.
 */
export class MemoryCache implements Cache {
  private readonly entries = new Map<string, { expiresAt: number; value: string }>();

  constructor(
    private readonly maxEntries = 4096,
    /** Milliseconds since an arbitrary origin; injected so tests control time. */
    private readonly clock: () => number = () => Date.now(),
  ) {}

  async get(key: string): Promise<string | undefined> {
    const entry = this.entries.get(key);
    if (entry === undefined) return undefined;
    if (this.clock() >= entry.expiresAt) {
      this.entries.delete(key);
      return undefined;
    }
    return entry.value;
  }

  async set(key: string, value: string, ttlSeconds: number): Promise<void> {
    if (ttlSeconds <= 0) return;
    this.entries.delete(key); // a re-set moves the key to the newest slot
    if (this.entries.size >= this.maxEntries) {
      const oldest = this.entries.keys().next().value;
      if (oldest !== undefined) this.entries.delete(oldest);
    }
    this.entries.set(key, { expiresAt: this.clock() + ttlSeconds * 1000, value });
  }

  async healthy(): Promise<boolean> {
    return true;
  }

  async close(): Promise<void> {
    this.entries.clear();
  }
}

/** The subset of an ioredis client this adapter uses. A test passes a fake. */
export interface RedisClient {
  get(key: string): Promise<string | null>;
  set(key: string, value: string, mode: "EX", seconds: number): Promise<unknown>;
  ping(): Promise<unknown>;
  quit(): Promise<unknown>;
}

/**
 * A RedisClient that imports `ioredis` and opens the socket on the FIRST
 * command, not at construction. This keeps the runtime build synchronous and
 * means a project that never exercises the cache (tests, a local run with no
 * REDIS_URL) never loads the driver or touches the network.
 */
class LazyRedisClient implements RedisClient {
  private pending?: Promise<RedisClient>;

  constructor(private readonly url: string) {}

  private resolve(): Promise<RedisClient> {
    if (this.pending === undefined) {
      this.pending = import("ioredis").then(({ default: Redis }) => {
        const Ctor = Redis as unknown as new (url: string, opts: object) => RedisClient;
        // A down Redis must fail fast so the lookup falls through; ioredis
        // reconnects by itself on the next command.
        return new Ctor(this.url, {
          connectTimeout: 2000,
          commandTimeout: 2000,
          maxRetriesPerRequest: 1,
          enableOfflineQueue: false,
          lazyConnect: true,
        });
      });
    }
    return this.pending;
  }

  async get(key: string): Promise<string | null> {
    return (await this.resolve()).get(key);
  }

  async set(key: string, value: string, mode: "EX", seconds: number): Promise<unknown> {
    return (await this.resolve()).set(key, value, mode, seconds);
  }

  async ping(): Promise<unknown> {
    return (await this.resolve()).ping();
  }

  async quit(): Promise<unknown> {
    if (this.pending === undefined) return undefined; // never connected: nothing to close
    return (await this.pending).quit();
  }
}

/**
 * The cache the runtime uses: Redis when a URL is given (lazily connected),
 * the in-process map otherwise. One place so lib/server/runtime.ts reads a
 * single call.
 */
export function createCache(opts: { redisUrl?: string }): Cache {
  return opts.redisUrl ? RedisCache.fromUrl(opts.redisUrl) : new MemoryCache();
}

/**
 * `Cache` backed by Redis. Used only when REDIS_URL is set. Every backend
 * error is swallowed: a miss on read, a no-op on write, so a down Redis turns
 * the cache off instead of failing requests. The URL carries the password and
 * is NEVER logged.
 */
export class RedisCache implements Cache {
  private degraded = false;

  constructor(private readonly client: RedisClient) {}

  /**
   * Build an adapter for a `redis://` / `rediss://` URL. The construction is
   * synchronous and opens nothing: `ioredis` is imported and the socket opened
   * on the first command (see LazyRedisClient), so the runtime build stays sync
   * and the tests — which inject a fake client straight into the constructor —
   * never load the driver.
   */
  static fromUrl(url: string): RedisCache {
    return new RedisCache(new LazyRedisClient(url));
  }

  async get(key: string): Promise<string | undefined> {
    try {
      const value = await this.client.get(key);
      this.degraded = false;
      return value ?? undefined;
    } catch {
      this.degraded = true;
      return undefined;
    }
  }

  async set(key: string, value: string, ttlSeconds: number): Promise<void> {
    if (ttlSeconds <= 0) return;
    try {
      await this.client.set(key, value, "EX", Math.max(1, Math.trunc(ttlSeconds)));
      this.degraded = false;
    } catch {
      this.degraded = true;
    }
  }

  async healthy(): Promise<boolean> {
    try {
      await this.client.ping();
      return true;
    } catch {
      return false;
    }
  }

  async close(): Promise<void> {
    try {
      await this.client.quit(); // best effort on shutdown
    } catch {
      // ignore: the socket is torn down with the process anyway
    }
  }

  /** True once a backend call has failed and not yet recovered (for metrics). */
  get isDegraded(): boolean {
    return this.degraded;
  }
}

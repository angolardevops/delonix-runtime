/**
 * `Cache` backed by Redis (the `redis` package's client).
 *
 * Used only when `REDIS_URL` is set. Every backend error is swallowed: a miss
 * on read, a no-op on write. A down Redis therefore turns the cache off (every
 * lookup falls through to the store) instead of failing requests. The URL —
 * which carries the password — is NEVER logged; only the error name is, once
 * per health transition, so an outage does not flood the log.
 *
 * `RedisCache` depends only on the small `RedisBackend` below, so it is tested
 * with a fake and never needs a live broker; `fromUrl` builds the real backend
 * from the `redis` package, imported lazily so this module loads with no
 * connection and tests pay nothing for it.
 */
import type { Logger } from "pino";
import type { Cache } from "./ports.js";

/** The slice of a Redis client `RedisCache` uses. Total: methods may reject. */
export interface RedisBackend {
  get(key: string): Promise<Buffer | null>;
  set(key: string, value: Buffer, ttlSeconds: number): Promise<void>;
  ping(): Promise<boolean>;
  close(): Promise<void>;
}

export class RedisCache implements Cache {
  private degraded = false;

  constructor(
    private readonly backend: RedisBackend,
    private readonly log?: Logger,
  ) {}

  /**
   * Build a cache from a `redis://` / `rediss://` URL. No connection is opened
   * here — the first command connects, bounded by `timeoutMs`.
   */
  static async fromUrl(url: string, log?: Logger, timeoutMs = 2000): Promise<RedisCache> {
    const { createClient } = await import("redis");
    const client = createClient({
      url,
      socket: {
        connectTimeout: timeoutMs,
        reconnectStrategy: (retries) => Math.min(retries * 200, 2000),
      },
    });
    // A connection error must not crash the process; it degrades the cache.
    client.on("error", () => {});
    await client.connect().catch(() => {});
    const backend: RedisBackend = {
      async get(key) {
        const v = await client.get(client.commandOptions({ returnBuffers: true }), key);
        return (v as Buffer | null) ?? null;
      },
      async set(key, value, ttlSeconds) {
        await client.set(key, value, { EX: Math.max(1, Math.trunc(ttlSeconds)) });
      },
      async ping() {
        return (await client.ping()) === "PONG";
      },
      async close() {
        await client.quit();
      },
    };
    return new RedisCache(backend, log);
  }

  async get(key: string): Promise<Buffer | null> {
    try {
      const value = await this.backend.get(key);
      this.recover();
      return value;
    } catch (err) {
      this.degrade(err);
      return null;
    }
  }

  async set(key: string, value: Buffer, ttlSeconds: number): Promise<void> {
    if (ttlSeconds <= 0) return;
    try {
      await this.backend.set(key, value, ttlSeconds);
      this.recover();
    } catch (err) {
      this.degrade(err);
    }
  }

  async healthy(): Promise<boolean> {
    try {
      return await this.backend.ping();
    } catch {
      return false;
    }
  }

  async close(): Promise<void> {
    try {
      await this.backend.close(); // best effort on shutdown
    } catch {
      /* ignore */
    }
  }

  private degrade(err: unknown): void {
    if (!this.degraded) {
      this.degraded = true;
      // name only: the message can carry the host, never the password.
      this.log?.warn(
        { error: err instanceof Error ? err.name : "error" },
        "cache backend unavailable, serving from the store",
      );
    }
  }

  private recover(): void {
    if (this.degraded) {
      this.degraded = false;
      this.log?.info("cache backend recovered");
    }
  }
}

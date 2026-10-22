import type { AppLogger } from "../logging/app-logger";
import { Cache } from "./cache";

/**
 * The structural slice of a Redis client this cache uses (ioredis fits). Named
 * here so the adapter depends on behaviour, not on the driver: a test passes a
 * fake, and cache.module.ts builds the real `ioredis` client lazily.
 */
export interface RedisClient {
  getBuffer(key: string): Promise<Buffer | null>;
  set(key: string, value: Buffer, mode: "PX", ttlMs: number): Promise<unknown>;
  ping(): Promise<unknown>;
  quit(): Promise<unknown>;
}

/**
 * {@link Cache} backed by Redis. Used only when REDIS_URL is set.
 *
 * Every call is bounded by the client's own short socket timeout and every
 * backend error is swallowed: a miss on read, a no-op on write. A down Redis
 * therefore turns the cache off (every lookup falls through to the store)
 * instead of failing requests. The URL — which carries the password — is NEVER
 * logged; only that the backend became unavailable or recovered, once per
 * transition, so an outage does not flood the log.
 */
export class RedisCache extends Cache {
  private degraded = false;

  constructor(
    private readonly client: RedisClient,
    private readonly logger: AppLogger,
  ) {
    super();
  }

  async get(key: string): Promise<Buffer | null> {
    try {
      const value = await this.client.getBuffer(key);
      this.recover();
      return value;
    } catch (error) {
      this.degrade(error);
      return null;
    }
  }

  async set(key: string, value: Buffer, ttlMs: number): Promise<void> {
    if (ttlMs <= 0) {
      return;
    }
    try {
      await this.client.set(key, value, "PX", Math.max(1, Math.floor(ttlMs)));
      this.recover();
    } catch (error) {
      this.degrade(error);
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
      await this.client.quit();
    } catch {
      // Best effort on shutdown.
    }
  }

  private degrade(error: unknown): void {
    if (!this.degraded) {
      this.degraded = true;
      // The error NAME only: it can carry the host, never the password.
      this.logger.warn("cache backend unavailable, serving from the store", {
        error: error instanceof Error ? error.name : "unknown",
      });
    }
  }

  private recover(): void {
    if (this.degraded) {
      this.degraded = false;
      this.logger.log("cache backend recovered");
    }
  }
}

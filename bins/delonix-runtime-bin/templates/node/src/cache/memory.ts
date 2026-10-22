/**
 * In-process `Cache`. The default when no `REDIS_URL` is set.
 *
 * Entries live in a Map with a monotonic-clock expiry; a bounded size evicts
 * the oldest first (Map keeps insertion order) so a long run cannot grow
 * without limit. Lost on restart and not shared between replicas — for local
 * runs, tests, and as the graceful fallback when Redis is not configured.
 */
import { performance } from "node:perf_hooks";
import type { Cache } from "./ports.js";

export interface InMemoryCacheOptions {
  maxEntries?: number;
  /** Monotonic seconds. Injected in tests. */
  clock?: () => number;
}

export class InMemoryCache implements Cache {
  private readonly byKey = new Map<string, { expiresAt: number; value: Buffer }>(); // oldest first
  private readonly max: number;
  private readonly clock: () => number;

  constructor(opts: InMemoryCacheOptions = {}) {
    this.max = opts.maxEntries ?? 4096;
    this.clock = opts.clock ?? (() => performance.now() / 1000);
  }

  async get(key: string): Promise<Buffer | null> {
    const entry = this.byKey.get(key);
    if (entry === undefined) return null;
    if (this.clock() >= entry.expiresAt) {
      this.byKey.delete(key);
      return null;
    }
    return entry.value;
  }

  async set(key: string, value: Buffer, ttlSeconds: number): Promise<void> {
    if (ttlSeconds <= 0) return;
    if (this.byKey.has(key)) this.byKey.delete(key);
    else if (this.byKey.size >= this.max) {
      const oldest = this.byKey.keys().next().value; // full: evict the oldest
      if (oldest !== undefined) this.byKey.delete(oldest);
    }
    this.byKey.set(key, { expiresAt: this.clock() + ttlSeconds, value });
  }

  async healthy(): Promise<boolean> {
    return true;
  }

  async close(): Promise<void> {
    this.byKey.clear();
  }
}

import { Cache } from "./cache";

/**
 * In-process {@link Cache}. The default when no REDIS_URL is set.
 *
 * Entries live in a Map with a monotonic-clock expiry; a bounded size evicts
 * the oldest first so a long run cannot grow without limit. Lost on restart and
 * not shared between replicas — for local runs, tests, and as the graceful
 * fallback when Redis is not configured. Safe inside one event loop: no method
 * awaits between reading and writing.
 */
export class InMemoryCache extends Cache {
  // Insertion order is iteration order: the oldest key is first.
  private readonly byKey = new Map<string, { expiresAt: number; value: Buffer }>();

  constructor(
    private readonly maxEntries = 4096,
    /** Monotonic milliseconds; injectable so a test controls expiry. */
    private readonly clock: () => number = () => performance.now(),
  ) {
    super();
  }

  get(key: string): Promise<Buffer | null> {
    const entry = this.byKey.get(key);
    if (entry === undefined) {
      return Promise.resolve(null);
    }
    if (this.clock() >= entry.expiresAt) {
      this.byKey.delete(key);
      return Promise.resolve(null);
    }
    return Promise.resolve(entry.value);
  }

  set(key: string, value: Buffer, ttlMs: number): Promise<void> {
    if (ttlMs <= 0) {
      return Promise.resolve();
    }
    if (this.byKey.has(key)) {
      this.byKey.delete(key);
    } else if (this.byKey.size >= this.maxEntries) {
      const oldest = this.byKey.keys().next().value;
      if (oldest !== undefined) {
        this.byKey.delete(oldest);
      }
    }
    this.byKey.set(key, { expiresAt: this.clock() + ttlMs, value });
    return Promise.resolve();
  }

  healthy(): Promise<boolean> {
    return Promise.resolve(true);
  }

  close(): Promise<void> {
    this.byKey.clear();
    return Promise.resolve();
  }
}

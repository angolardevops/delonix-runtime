import { TOLERANCE_SECONDS } from "./signature.js";

/**
 * Remembers the ids of accepted deliveries for the tolerance window, so a
 * sender's retry of a message already handled is acknowledged without being
 * handled twice.
 *
 * It lives in memory: it does NOT survive a restart and is not shared between
 * replicas. That is enough against one sender retrying to one process; a
 * deployment that must never process a message twice needs a durable store (a
 * unique key in the database) instead.
 */
export class Dedup {
  private readonly seen = new Map<string, number>();

  constructor(private readonly max: number) {}

  /** Records the id and reports whether it was new. */
  firstSeen(id: string, nowSeconds: number): boolean {
    for (const [key, at] of this.seen) {
      if (nowSeconds - at > 2 * TOLERANCE_SECONDS) this.seen.delete(key);
    }
    if (this.seen.has(id)) return false;
    if (this.seen.size >= this.max) {
      // Full: evict the oldest (Map keeps insertion order) rather than refuse traffic.
      const oldest = this.seen.keys().next().value;
      if (oldest !== undefined) this.seen.delete(oldest);
    }
    this.seen.set(id, nowSeconds);
    return true;
  }

  /** Drops the id so a retry is processed again (processing failed after firstSeen). */
  forget(id: string): void {
    this.seen.delete(id);
  }
}

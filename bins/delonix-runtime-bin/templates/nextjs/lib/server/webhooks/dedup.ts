// Remembers the ids of accepted deliveries for twice the tolerance window, so
// a sender's retry of a message already handled is acknowledged without being
// handled twice. In memory: it does NOT survive a restart and is not shared
// between replicas — enough against the retries of one sender to one process;
// a deployment that must never process a message twice needs a unique key in
// a database instead (docs/adr/0002-webhooks.md).
import "server-only";
import { TOLERANCE_SECONDS } from "./signature";

export class Dedup {
  private readonly seen = new Map<string, number>();

  constructor(private readonly max = 4096) {}

  /** Records id and reports whether it was new. */
  firstSeen(id: string, nowMs: number): boolean {
    for (const [key, at] of this.seen) {
      if (nowMs - at > 2 * TOLERANCE_SECONDS * 1000) this.seen.delete(key);
    }
    if (this.seen.has(id)) return false;
    if (this.seen.size >= this.max) {
      // Full: evict the oldest (Map keeps insertion order) rather than refuse traffic.
      const oldest = this.seen.keys().next().value;
      if (oldest !== undefined) this.seen.delete(oldest);
    }
    this.seen.set(id, nowMs);
    return true;
  }

  /** Drops id so a retry is processed again; called when processing failed after firstSeen. */
  forget(id: string): void {
    this.seen.delete(id);
  }
}

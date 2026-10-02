// Whether the process should receive traffic, and the work it still owes.
//
// Readiness moves one way: starting → ready → draining. Liveness only says
// the process answers. After SIGTERM the process first stays ready-for-
// in-flight but reports 503 `draining` (DRAIN_DELAY lets a load balancer
// notice), then refuses new work, waits for the requests and page renders it
// is still serving, drains the outbound webhooks, and flushes telemetry — all
// within SHUTDOWN_TIMEOUT. See docs/adr/0003-shutdown.md for why this is done
// here and not by `next start`.
import "server-only";

export type ReadinessState = "starting" | "ready" | "draining";

/**
 * A dependency the service cannot serve without. The example has none: the
 * in-memory store cannot be down. Add one when a real database is configured.
 */
export type ReadinessCheck = () => Promise<void>;

export class Lifecycle {
  private state: ReadinessState = "starting";
  private closing = false;
  private inFlight = 0;
  private idleWaiters: Array<() => void> = [];

  constructor(private readonly checks: Record<string, ReadinessCheck> = {}) {}

  markReady(): void {
    if (this.state === "starting") this.state = "ready";
  }

  /** One-way: nothing brings a draining process back. */
  markDraining(): void {
    this.state = "draining";
  }

  /** After the drain delay: new work is refused with 503. */
  close(): void {
    this.markDraining();
    this.closing = true;
  }

  get readiness(): ReadinessState {
    return this.state;
  }

  get isClosing(): boolean {
    return this.closing;
  }

  get pending(): number {
    return this.inFlight;
  }

  /** The state and, when ready, the name of the first failing dependency. */
  async evaluate(): Promise<{ state: ReadinessState; failing?: string }> {
    if (this.state !== "ready") return { state: this.state };
    for (const [name, check] of Object.entries(this.checks)) {
      try {
        await check();
      } catch {
        return { state: this.state, failing: name };
      }
    }
    return { state: this.state };
  }

  /** Counts one unit of work; call the returned function once when it is done. */
  begin(): () => void {
    this.inFlight++;
    let done = false;
    return () => {
      if (done) return;
      done = true;
      this.inFlight--;
      if (this.inFlight === 0) {
        const waiters = this.idleWaiters;
        this.idleWaiters = [];
        waiters.forEach((w) => w());
      }
    };
  }

  /** Resolves true when no work is in flight, false if the deadline came first. */
  waitIdle(timeoutMs: number): Promise<boolean> {
    if (this.inFlight === 0) return Promise.resolve(true);
    return new Promise((resolve) => {
      const timer = setTimeout(() => resolve(false), Math.max(0, timeoutMs));
      this.idleWaiters.push(() => {
        clearTimeout(timer);
        resolve(true);
      });
    });
  }
}

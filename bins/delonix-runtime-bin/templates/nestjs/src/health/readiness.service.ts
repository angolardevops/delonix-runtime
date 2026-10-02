import { Injectable } from "@nestjs/common";

export type ReadinessState = "starting" | "ready" | "draining";

/** A dependency the service cannot serve without; resolves when healthy. */
export type ReadinessCheck = () => Promise<void>;

/**
 * Whether the process should receive traffic. Liveness only says the process
 * answers; readiness moves starting → ready → draining, and draining is
 * one-way. A load balancer polling readiness stops routing here before the
 * listener closes.
 *
 * The example registers no checks: the in-memory store cannot be down. Add
 * one when a real database or broker is configured — never a check for
 * something that is not used.
 */
@Injectable()
export class ReadinessService {
  private current: ReadinessState = "starting";
  private readonly checks = new Map<string, ReadinessCheck>();

  get state(): ReadinessState {
    return this.current;
  }

  markReady(): void {
    if (this.current === "starting") {
      this.current = "ready";
    }
  }

  markDraining(): void {
    this.current = "draining";
  }

  addCheck(name: string, check: ReadinessCheck): void {
    this.checks.set(name, check);
  }

  /** The state and, when ready, the first failing dependency (by name only). */
  async evaluate(): Promise<{ state: ReadinessState; failing?: string }> {
    if (this.current !== "ready") {
      return { state: this.current };
    }
    for (const [name, check] of this.checks) {
      try {
        await check();
      } catch {
        return { state: this.current, failing: name };
      }
    }
    return { state: this.current };
  }
}

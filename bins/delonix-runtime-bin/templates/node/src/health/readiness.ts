/**
 * Whether the process should receive traffic. Liveness only says it answers;
 * readiness moves starting → ready → draining, and draining is final: after a
 * shutdown signal the process finishes what is in flight and takes nothing new.
 */
export type ReadinessState = "starting" | "ready" | "draining";

/**
 * A dependency the service cannot serve without. The example has none (the
 * in-memory store cannot be down); add one when a real database or broker is
 * configured — never a check for something that is not used.
 */
export type ReadinessCheck = () => Promise<void>;

export class Readiness {
  private current: ReadinessState = "starting";

  constructor(private readonly checks: Record<string, ReadinessCheck> = {}) {}

  get state(): ReadinessState {
    return this.current;
  }

  setReady(): void {
    if (this.current === "starting") this.current = "ready";
  }

  setDraining(): void {
    this.current = "draining";
  }

  /** The state and, when ready, the name of the first failing dependency. */
  async evaluate(): Promise<{ state: ReadinessState; failing?: string }> {
    if (this.current !== "ready") return { state: this.current };
    for (const [name, check] of Object.entries(this.checks)) {
      try {
        await check();
      } catch {
        return { state: this.current, failing: name };
      }
    }
    return { state: this.current };
  }
}

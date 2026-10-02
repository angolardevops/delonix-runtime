// Ordered, bounded shutdown. `next start` and the standalone server.js
// normally handle SIGTERM themselves: they close the listener at once, wait
// for in-flight requests without a deadline, and exit 143 — no readiness
// drain, no flush of the telemetry exporters. With NEXT_MANUAL_SIG_HANDLE=true
// (set by `pnpm start` and by the image) Next.js registers no handler and
// this one runs instead:
//
//   readiness 503 `draining` → DRAIN_DELAY (still serving) → new API requests
//   refused with 503 → requests and page renders in flight finish → pending
//   webhooks drain → telemetry flushes → exit 0.
//
// Everything after the drain delay shares one SHUTDOWN_TIMEOUT; when it cuts
// a step short the exit code is 1. A second signal exits at once.
//
// The listener itself cannot be closed from here (Next.js owns the HTTP
// server and does not expose it), so a connection that arrives after the
// drain delay is answered 503 rather than refused. docs/adr/0003-shutdown.md.
import "server-only";
import type { Runtime } from "./runtime";

const TELEMETRY_FLUSH_MAX_MS = 3_000;

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/** The shutdown sequence, separate from the signal wiring so a test can run it. */
export async function shutdown(runtime: Runtime, exit: (code: number) => void): Promise<void> {
  const { config, lifecycle, log } = runtime;
  log.info("shutting down", { timeout_ms: config.shutdownTimeoutMs, drain_delay_ms: config.drainDelayMs });
  lifecycle.markDraining();
  if (config.drainDelayMs > 0) await sleep(config.drainDelayMs);
  lifecycle.close();

  const deadline = runtime.nowMs() + config.shutdownTimeoutMs;
  const remaining = () => Math.max(0, deadline - runtime.nowMs());
  let complete = true;
  if (!(await lifecycle.waitIdle(remaining()))) {
    log.error("requests still in flight at the deadline", { in_flight: lifecycle.pending });
    complete = false;
  }
  if (runtime.dispatcher && !(await runtime.dispatcher.close(remaining()))) {
    complete = false;
  }
  // Capped: a collector that is down must not hold the exit for the whole deadline.
  await runtime.telemetry.shutdown(Math.min(Math.max(remaining(), 500), TELEMETRY_FLUSH_MAX_MS));
  log.info(complete ? "stopped" : "stopped before the shutdown completed");
  exit(complete ? 0 : 1);
}

export function installSignalHandlers(runtime: Runtime): void {
  let started = false;
  const onSignal = (signal: NodeJS.Signals) => {
    if (started) {
      runtime.log.warn("second signal, exiting now", { signal });
      process.exit(1);
    }
    started = true;
    void shutdown(runtime, (code) => process.exit(code));
  };
  process.on("SIGTERM", onSignal);
  process.on("SIGINT", onSignal);
}

/**
 * Composition root: reads the configuration, builds every adapter, hands them
 * to the use cases and the HTTP transport, and owns the process lifecycle
 * (signals, readiness, ordered shutdown).
 */
import { buildApp } from "./app.js";
import { ConfigError, loadConfig } from "./config.js";
import { Readiness } from "./health/readiness.js";
import { shutdownTelemetry } from "./instrumentation.js";
import { createLogger } from "./logging.js";
import { MemoryNoteStore } from "./notes/memory-store.js";
import { NotesService } from "./notes/notes.js";
import { tracedNotes } from "./telemetry/traced-notes.js";
import { Dedup } from "./webhooks/dedup.js";
import { WebhookDispatcher } from "./webhooks/dispatcher.js";
import { parseSecret } from "./webhooks/signature.js";

async function main(): Promise<void> {
  const cfg = loadConfig(process.env, process.env.SERVICE_VERSION ?? "dev");
  const log = createLogger(cfg);

  const secret = (name: string, value: string): Buffer => {
    try {
      return parseSecret(value);
    } catch (err) {
      throw new ConfigError([`${name}: ${(err as Error).message}`]);
    }
  };
  const inboundKey = cfg.inboundWebhookSecret
    ? secret("WEBHOOK_INBOUND_SECRET", cfg.inboundWebhookSecret)
    : undefined;
  const dispatcher = cfg.outboundWebhookUrl
    ? new WebhookDispatcher(
        {
          url: cfg.outboundWebhookUrl,
          key: secret("WEBHOOK_TARGET_SECRET", cfg.outboundWebhookSecret ?? ""),
          timeoutMs: 5000,
          maxAttempts: 4,
          baseBackoffMs: 500,
          queueSize: 100,
        },
        log,
      )
    : undefined;

  const readiness = new Readiness();
  const app = await buildApp({
    logger: log,
    notes: tracedNotes(new NotesService(new MemoryNoteStore(), dispatcher)),
    readiness,
    maxBodyBytes: cfg.maxBodyBytes,
    inboundWebhook: inboundKey ? { key: inboundKey, dedup: new Dedup(4096) } : undefined,
  });

  await app.listen({ host: cfg.host, port: cfg.port });
  readiness.setReady();

  let stopping = false;
  const shutdown = async (signal: string): Promise<void> => {
    if (stopping) process.exit(1); // a second signal stops waiting
    stopping = true;
    log.info({ signal, timeout_ms: cfg.shutdownTimeoutMs }, "shutting down");
    // Ordered, and bounded as a whole: stop being ready, let the load balancer
    // notice, stop accepting and finish in-flight requests, drain the outbound
    // queue, flush telemetry.
    const deadline = Date.now() + cfg.shutdownTimeoutMs;
    const left = (): number => Math.max(0, deadline - Date.now());
    const hard = setTimeout(
      () => {
        log.error("shutdown deadline exceeded");
        process.exit(1);
      },
      cfg.shutdownTimeoutMs + cfg.drainDelayMs + 500,
    );
    readiness.setDraining();
    await new Promise((resolve) => setTimeout(resolve, cfg.drainDelayMs));
    await app.close();
    const drained = dispatcher ? await dispatcher.close(left()) : true;
    if (!drained) log.error("webhook queue not drained before the deadline");
    // A collector that is down loses the last batch; it does not turn a clean
    // stop into a failed one.
    if (!(await shutdownTelemetry(Math.min(left(), 5000)))) log.warn("telemetry flush failed");
    clearTimeout(hard);
    log.info("stopped");
    process.exit(drained ? 0 : 1);
  };
  process.on("SIGTERM", () => void shutdown("SIGTERM"));
  process.on("SIGINT", () => void shutdown("SIGINT"));
}

main().catch((err: unknown) => {
  // Before the logger exists (or when it could not be built): plain stderr.
  process.stderr.write(`fatal: ${err instanceof Error ? err.message : String(err)}\n`);
  process.exit(1);
});

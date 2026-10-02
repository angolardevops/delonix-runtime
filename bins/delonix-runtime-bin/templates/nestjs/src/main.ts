// The composition root: reads the configuration, builds the logger and the
// application, and owns the process lifecycle (listen, signals, readiness,
// ordered and bounded shutdown).
import "reflect-metadata";

import { createApp } from "./app.setup";
import { ConfigError, loadConfig } from "./config/app-config";
import { ReadinessService } from "./health/readiness.service";
import { AppLogger } from "./logging/app-logger";
import { telemetry } from "./telemetry/otel";

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/** Resolves "timeout" after `ms`, without keeping the process alive. */
function deadline(ms: number): Promise<"timeout"> {
  return new Promise((resolve) => setTimeout(() => resolve("timeout"), ms).unref());
}

async function run(): Promise<number> {
  let config;
  try {
    config = loadConfig(process.env);
  } catch (error) {
    if (error instanceof ConfigError) {
      process.stderr.write(`fatal: ${error.message}\n`);
      return 1;
    }
    throw error;
  }
  const logger = AppLogger.create({
    level: config.logLevel,
    service: config.serviceName,
    version: config.version,
    environment: config.env,
  });
  if (telemetry() === undefined) {
    logger.warn("telemetry not loaded: start with `node --require ./dist/instrumentation.js dist/main.js`");
  }

  const app = await createApp(config, logger);
  const readiness = app.get(ReadinessService);

  // Registered before listening, so a signal during startup is not lost.
  const stopSignal = new Promise<NodeJS.Signals>((resolve) => {
    process.once("SIGTERM", resolve);
    process.once("SIGINT", resolve);
  });

  await app.listen(config.port, config.host);
  readiness.markReady();
  logger.log("listening", { addr: `${config.host}:${config.port}` });

  const signal = await stopSignal;
  // A second signal now kills the process the default way.
  process.removeAllListeners("SIGTERM");
  process.removeAllListeners("SIGINT");

  // Ordered shutdown: stop being ready, let the load balancer notice, then
  // app.close() — Nest stops accepting, in-flight requests finish, and the
  // onApplicationShutdown hooks run (the webhook queue drains) — and last the
  // telemetry flush. Everything after the drain delay shares one deadline.
  // (Not app.enableShutdownHooks(): its handler closes without a deadline or a
  // drain step and then re-raises the signal, so the exit status would say
  // "killed". app.close() runs the same lifecycle hooks.)
  logger.log("shutting down", { signal, timeout_ms: config.shutdownTimeoutMs });
  readiness.markDraining();
  await sleep(config.drainDelayMs);
  const limit = deadline(config.shutdownTimeoutMs);
  const closed = await Promise.race([app.close().then(() => "closed" as const), limit]);
  if (closed === "timeout") {
    logger.error("shutdown deadline exceeded; exiting with work in flight");
    return 1;
  }
  // A collector that is down loses the last batch; it does not turn a clean
  // stop into a failed one.
  const flushed = await Promise.race([
    (telemetry()?.shutdown() ?? Promise.resolve()).then(
      () => "flushed" as const,
      () => "failed" as const,
    ),
    limit,
  ]);
  if (flushed !== "flushed") {
    logger.warn("telemetry flush did not complete", { outcome: flushed });
  }
  logger.log("stopped");
  return 0;
}

run().then(
  (code) => process.exit(code),
  (error: unknown) => {
    process.stderr.write(
      `fatal: ${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`,
    );
    process.exit(1);
  },
);

// Runs once per server process, from instrumentation.ts, before Next.js
// routes a request: validate the configuration, start telemetry, build the
// runtime, take over the signals, and only then report ready.
import "server-only";
import { after } from "next/server";
import { ConfigError, loadConfig } from "./config";
import { createLogger } from "./logger";
import { buildRuntime, setRuntime } from "./runtime";
import { installSignalHandlers } from "./shutdown";
import { startTelemetry } from "./telemetry";

export function boot(): void {
  try {
    const config = loadConfig();
    const log = createLogger({
      service: config.serviceName,
      version: config.version,
      environment: config.appEnv,
      level: config.logLevel,
    });
    const telemetry = startTelemetry({
      serviceName: config.serviceName,
      version: config.version,
      environment: config.appEnv,
      log,
    });
    const runtime = buildRuntime(config, { log, telemetry, afterResponse: (task) => after(task) });
    setRuntime(runtime);
    if (config.manualSignalHandling) {
      installSignalHandlers(runtime);
    } else {
      log.warn(
        "NEXT_MANUAL_SIG_HANDLE is not set: Next.js handles SIGTERM itself, without the readiness drain",
      );
    }
    runtime.lifecycle.markReady();
    log.info("ready", { port: config.port, environment: config.appEnv });
  } catch (err) {
    // Before the logger exists, or because of it: plain stderr, then stop.
    process.stderr.write(`fatal: ${err instanceof ConfigError ? err.message : String(err)}\n`);
    process.exit(1);
  }
}

import type { Server } from "node:http";

import type { NestExpressApplication } from "@nestjs/platform-express";

import { createApp } from "../../src/app.setup";
import { loadConfig } from "../../src/config/app-config";
import { ReadinessService } from "../../src/health/readiness.service";
import { AppLogger } from "../../src/logging/app-logger";

/** `whsec_` + base64 of 32 bytes; a test value, not a real secret. */
export const TEST_SECRET = "whsec_" + Buffer.alloc(32, 7).toString("base64");

export interface TestApp {
  app: NestExpressApplication;
  server: Server;
  /** Parsed log lines written so far. */
  logs(): Record<string, unknown>[];
  close(): Promise<void>;
}

/**
 * The application exactly as main.ts builds it (same middleware, body parser,
 * filters), initialised and marked ready, with the log lines captured.
 */
export async function startTestApp(env: Record<string, string> = {}): Promise<TestApp> {
  const config = loadConfig({ APP_ENV: "test", LOG_LEVEL: "debug", ...env });
  const lines: string[] = [];
  const logger = AppLogger.create({
    level: config.logLevel,
    service: config.serviceName,
    version: config.version,
    environment: config.env,
    destination: { write: (line: string) => void lines.push(line) },
  });
  const app = await createApp(config, logger);
  await app.init();
  app.get(ReadinessService).markReady();
  return {
    app,
    server: app.getHttpServer(),
    logs: () => lines.map((line) => JSON.parse(line) as Record<string, unknown>),
    close: () => app.close(),
  };
}

import { loadConfig } from "@/lib/server/config";
import { createLogger } from "@/lib/server/logger";
import { buildRuntime, setRuntime, type Runtime } from "@/lib/server/runtime";

export const TEST_SECRET = "whsec_" + Buffer.alloc(32, 7).toString("base64");

export interface TestRuntime {
  runtime: Runtime;
  /** Parsed JSON log lines, in order. */
  logs: Array<Record<string, unknown>>;
  /** Tasks scheduled "after the response"; run them with flush(). */
  flush: () => Promise<void>;
}

export interface TestOptions {
  env?: Record<string, string>;
  fetch?: typeof fetch;
  nowMs?: () => number;
  /** Run after-response tasks immediately instead of queueing them for flush(). */
  immediate?: boolean;
}

/** Builds the real runtime with test doubles at its edges and installs it for the Route Handlers. */
export function installRuntime(options: TestOptions = {}): TestRuntime {
  const config = loadConfig({ APP_ENV: "test", ...options.env });
  const logs: Array<Record<string, unknown>> = [];
  const log = createLogger({
    service: config.serviceName,
    version: config.version,
    environment: config.appEnv,
    level: "debug",
    write: (line) => logs.push(JSON.parse(line) as Record<string, unknown>),
  });
  const queue: Array<() => Promise<void>> = [];
  const runtime = buildRuntime(config, {
    log,
    telemetry: { shutdown: async () => true },
    afterResponse: (task) => {
      if (options.immediate) void task();
      else queue.push(task);
    },
    fetch: options.fetch,
    nowMs: options.nowMs,
  });
  runtime.lifecycle.markReady();
  setRuntime(runtime);
  return {
    runtime,
    logs,
    flush: async () => {
      while (queue.length > 0) await queue.shift()!();
    },
  };
}

export function request(
  method: string,
  path: string,
  body?: string,
  headers: Record<string, string> = {},
): Request {
  return new Request(`http://test.local${path}`, {
    method,
    body,
    headers: body === undefined ? headers : { "content-type": "application/json", ...headers },
  });
}

export async function errorOf(
  response: Response,
): Promise<{ code: string; message: string; fields?: Record<string, string>; request_id?: string }> {
  const body = (await response.json()) as { error: { code: string; message: string } };
  return body.error;
}

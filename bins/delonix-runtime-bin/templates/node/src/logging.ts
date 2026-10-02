/**
 * The process logger: JSON lines on stdout, the service identity on every line,
 * the trace and span ids of the active request when there is one, and secrets
 * redacted by key name before they reach the output.
 */
import { isSpanContextValid, trace } from "@opentelemetry/api";
import { destination, pino, stdTimeFunctions, type DestinationStream, type Logger } from "pino";
import type { Config } from "./config.js";

/** A value whose key contains one of these is replaced. */
const SENSITIVE = [
  "authorization",
  "cookie",
  "password",
  "secret",
  "token",
  "signature",
  "api_key",
  "apikey",
];

export const REDACTED = "[REDACTED]";

function redact(obj: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(obj)) {
    const k = key.toLowerCase();
    out[key] = SENSITIVE.some((s) => k.includes(s)) ? REDACTED : value;
  }
  return out;
}

export function createLogger(
  cfg: Pick<Config, "logLevel" | "serviceName" | "version" | "env">,
  stream?: DestinationStream,
): Logger {
  return pino(
    {
      level: cfg.logLevel,
      base: { service: cfg.serviceName, version: cfg.version, environment: cfg.env },
      timestamp: stdTimeFunctions.isoTime,
      formatters: {
        level: (label) => ({ level: label }),
        log: redact,
      },
      mixin() {
        const ctx = trace.getActiveSpan()?.spanContext();
        return ctx && isSpanContextValid(ctx) ? { trace_id: ctx.traceId, span_id: ctx.spanId } : {};
      },
    },
    stream ?? destination({ sync: true }),
  );
}

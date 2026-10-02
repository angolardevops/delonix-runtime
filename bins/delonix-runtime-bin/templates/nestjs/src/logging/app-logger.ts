import type { LoggerService } from "@nestjs/common";
import { trace } from "@opentelemetry/api";
import pino, { type DestinationStream, type Logger } from "pino";

/** Key fragments whose values never reach the output. */
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

export type Fields = Record<string, unknown>;

export interface LoggerOptions {
  level: "debug" | "info" | "warn" | "error";
  service: string;
  version: string;
  environment: string;
  /** Defaults to stdout, written synchronously (nothing lost on exit). */
  destination?: DestinationStream;
}

/**
 * The process logger: JSON lines on stdout, the service identity on every
 * line, the trace_id/span_id of the active span when there is one, and values
 * redacted by key name before they reach the output.
 *
 * It implements Nest's LoggerService, so the framework's own lines (route
 * mapping, bootstrap errors) come out in the same format. Pino is the sink:
 * it handles levels, serialisation and a stdout that is a pipe. Call it as
 * `logger.log("message", { field: value })`; a string instead of the object is
 * Nest's "context" and becomes the `context` field.
 */
export class AppLogger implements LoggerService {
  private constructor(private readonly sink: Logger) {}

  static create(options: LoggerOptions): AppLogger {
    const sink = pino(
      {
        level: options.level,
        messageKey: "msg",
        base: {
          service: options.service,
          version: options.version,
          environment: options.environment,
        },
        timestamp: pino.stdTimeFunctions.isoTime,
        formatters: { level: (label) => ({ level: label }) },
        // Runs for every line: correlates it with the active trace.
        mixin: () => {
          const span = trace.getActiveSpan();
          const ctx = span?.spanContext();
          return ctx !== undefined && trace.isSpanContextValid(ctx)
            ? { trace_id: ctx.traceId, span_id: ctx.spanId }
            : {};
        },
      },
      options.destination ?? pino.destination({ dest: 1, sync: true }),
    );
    return new AppLogger(sink);
  }

  log(message: unknown, ...params: unknown[]): void {
    this.write("info", message, params);
  }

  warn(message: unknown, ...params: unknown[]): void {
    this.write("warn", message, params);
  }

  debug(message: unknown, ...params: unknown[]): void {
    this.write("debug", message, params);
  }

  verbose(message: unknown, ...params: unknown[]): void {
    this.write("debug", message, params);
  }

  error(message: unknown, ...params: unknown[]): void {
    // Nest calls error(message, stack, context) or error(message, context).
    const strings = params.filter((p): p is string => typeof p === "string");
    const rest = params.filter((p) => typeof p !== "string");
    const extra: Fields = {};
    if (strings.length >= 2) {
      extra.stack = strings[0];
      extra.context = strings[1];
    } else if (strings.length === 1) {
      extra.context = strings[0];
    }
    this.write("error", message, [...rest, extra]);
  }

  fatal(message: unknown, ...params: unknown[]): void {
    this.write("fatal", message, params);
  }

  isLevelEnabled(level: "debug" | "info" | "warn" | "error"): boolean {
    return this.sink.isLevelEnabled(level);
  }

  private write(level: pino.Level, message: unknown, params: unknown[]): void {
    const fields: Fields = {};
    for (const param of params) {
      if (typeof param === "string") {
        fields.context = param;
      } else if (param !== null && typeof param === "object" && !Array.isArray(param)) {
        Object.assign(fields, param);
      }
    }
    let msg: string;
    if (message instanceof Error) {
      msg = message.message;
      fields.error = message.name;
    } else if (typeof message === "string") {
      msg = message;
    } else {
      msg = JSON.stringify(message);
    }
    this.sink[level](redact(fields) as Fields, msg);
  }
}

/** A copy of `value` where every key naming a secret has its value replaced. */
export function redact(value: unknown, depth = 0): unknown {
  if (depth > 8 || value === null || typeof value !== "object") {
    return value;
  }
  if (Array.isArray(value)) {
    return value.map((item) => redact(item, depth + 1));
  }
  const out: Fields = {};
  for (const [key, inner] of Object.entries(value as Fields)) {
    const lower = key.toLowerCase();
    out[key] = SENSITIVE.some((s) => lower.includes(s)) ? REDACTED : redact(inner, depth + 1);
  }
  return out;
}

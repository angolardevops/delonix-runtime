// JSON lines on stdout: the service identity on every line, the request id and
// the trace/span ids of the current request when there is one, and secrets
// redacted by key before they reach the output. Query strings, headers and
// bodies are never passed in by the callers in this project.
import "server-only";
import { trace } from "@opentelemetry/api";
import { currentRequest } from "./request-context";

export type Level = "debug" | "info" | "warn" | "error";
export type Fields = Record<string, unknown>;

export interface Logger {
  debug(msg: string, fields?: Fields): void;
  info(msg: string, fields?: Fields): void;
  warn(msg: string, fields?: Fields): void;
  error(msg: string, fields?: Fields): void;
}

const ORDER: Record<Level, number> = { debug: 10, info: 20, warn: 30, error: 40 };

/** A value whose key contains one of these is replaced by REDACTED. */
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

function redact(fields: Fields): Fields {
  const out: Fields = {};
  for (const [key, value] of Object.entries(fields)) {
    const lower = key.toLowerCase();
    out[key] = SENSITIVE.some((s) => lower.includes(s)) ? REDACTED : value;
  }
  return out;
}

export interface LoggerOptions {
  service: string;
  version: string;
  environment: string;
  level: Level;
  write?: (line: string) => void;
  now?: () => Date;
}

export function createLogger(opts: LoggerOptions): Logger {
  const write = opts.write ?? ((line: string) => process.stdout.write(line + "\n"));
  const now = opts.now ?? (() => new Date());
  const min = ORDER[opts.level];
  const emit = (level: Level, msg: string, fields: Fields = {}) => {
    if (ORDER[level] < min) return;
    const record: Fields = {
      time: now().toISOString(),
      level,
      msg,
      service: opts.service,
      version: opts.version,
      environment: opts.environment,
    };
    const request = currentRequest();
    if (request) record.request_id = request.requestId;
    const span = trace.getActiveSpan()?.spanContext();
    if (span && span.traceId !== "00000000000000000000000000000000") {
      record.trace_id = span.traceId;
      record.span_id = span.spanId;
    }
    write(JSON.stringify({ ...record, ...redact(fields) }));
  };
  return {
    debug: (m, f) => emit("debug", m, f),
    info: (m, f) => emit("info", m, f),
    warn: (m, f) => emit("warn", m, f),
    error: (m, f) => emit("error", m, f),
  };
}

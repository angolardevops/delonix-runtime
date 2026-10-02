/**
 * Configuration: read from the environment once, validated at startup. A bad
 * value stops the process before it listens, with every problem listed at once.
 */

import { SERVICE_NAME } from "./service.js";

export type Environment = "development" | "test" | "production";

export interface Config {
  serviceName: string;
  version: string;
  env: Environment;
  logLevel: "debug" | "info" | "warn" | "error";
  host: string;
  port: number;
  maxBodyBytes: number;
  shutdownTimeoutMs: number;
  /** Time serving with readiness at 503 before the listener closes. */
  drainDelayMs: number;
  /** Enables POST /api/v1/webhooks/inbound. Undefined: the route is not served. */
  inboundWebhookSecret?: string;
  /** Receives a signed `note.created` per new note. Undefined: nothing is sent. */
  outboundWebhookUrl?: string;
  outboundWebhookSecret?: string;
}

export class ConfigError extends Error {
  constructor(readonly problems: string[]) {
    super("configuration:\n" + problems.join("\n"));
  }
}

const ENVIRONMENTS = ["development", "test", "production"] as const;
const LEVELS = ["debug", "info", "warn", "error"] as const;

/** Parses "15s", "500ms", "2m" or a bare number of milliseconds. */
function duration(raw: string): number | undefined {
  const m = /^(\d+)(ms|s|m)?$/.exec(raw);
  if (!m) return undefined;
  const n = Number(m[1]);
  return m[2] === "m" ? n * 60_000 : m[2] === "s" ? n * 1000 : n;
}

export function loadConfig(env: NodeJS.ProcessEnv, version: string): Config {
  const problems: string[] = [];
  const get = (key: string): string | undefined => {
    const v = env[key]?.trim();
    return v === "" ? undefined : v;
  };
  const oneOf = <T extends string>(key: string, allowed: readonly T[], def: T): T => {
    const v = get(key) ?? def;
    if (!(allowed as readonly string[]).includes(v)) {
      problems.push(`${key}: "${v}" is not one of ${allowed.join(", ")}`);
      return def;
    }
    return v as T;
  };
  const dur = (key: string, def: number): number => {
    const raw = get(key);
    if (raw === undefined) return def;
    const d = duration(raw);
    if (d === undefined) {
      problems.push(`${key}: "${raw}" is not a duration (e.g. 15s, 500ms)`);
      return def;
    }
    return d;
  };
  const int = (key: string, def: number, min: number, max: number): number => {
    const raw = get(key);
    if (raw === undefined) return def;
    const n = Number(raw);
    if (!Number.isInteger(n) || n < min || n > max) {
      problems.push(`${key}: "${raw}" is not an integer between ${min} and ${max}`);
      return def;
    }
    return n;
  };

  const cfg: Config = {
    serviceName: get("SERVICE_NAME") ?? SERVICE_NAME,
    version,
    env: oneOf("APP_ENV", ENVIRONMENTS, "development"),
    logLevel: oneOf("LOG_LEVEL", LEVELS, "info"),
    host: get("HTTP_HOST") ?? "0.0.0.0",
    port: int("PORT", __PORT__, 1, 65535),
    maxBodyBytes: int("HTTP_MAX_BODY_BYTES", 1_048_576, 1, 1_073_741_824),
    shutdownTimeoutMs: dur("SHUTDOWN_TIMEOUT", 15_000),
    drainDelayMs: dur("DRAIN_DELAY", 0),
    inboundWebhookSecret: get("WEBHOOK_INBOUND_SECRET"),
    outboundWebhookUrl: get("WEBHOOK_TARGET_URL"),
    outboundWebhookSecret: get("WEBHOOK_TARGET_SECRET"),
  };

  if (cfg.outboundWebhookUrl !== undefined) {
    let url: URL | undefined;
    try {
      url = new URL(cfg.outboundWebhookUrl);
    } catch {
      url = undefined;
    }
    if (!url || (url.protocol !== "http:" && url.protocol !== "https:")) {
      problems.push(`WEBHOOK_TARGET_URL: "${cfg.outboundWebhookUrl}" is not an http(s) URL`);
    } else if (cfg.env === "production" && url.protocol !== "https:") {
      problems.push("WEBHOOK_TARGET_URL: production sends signed events over https only");
    }
    if (cfg.outboundWebhookSecret === undefined) {
      problems.push("WEBHOOK_TARGET_SECRET: required when WEBHOOK_TARGET_URL is set");
    }
  }

  if (problems.length > 0) throw new ConfigError(problems);
  return cfg;
}

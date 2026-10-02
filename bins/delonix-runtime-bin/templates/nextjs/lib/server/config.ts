// Configuration from the environment, validated once at startup
// (instrumentation.ts → lib/server/boot.ts). A bad value stops the process
// before it serves, with every problem listed at once. Secrets are read here
// and only here, on the server: none of them has a NEXT_PUBLIC_ prefix, so
// Next.js never inlines them into browser code.
import "server-only";
import { DEFAULT_PORT, PROJECT_NAME } from "@/lib/project";

export type AppEnv = "development" | "test" | "production";
export type LogLevel = "debug" | "info" | "warn" | "error";

export interface Config {
  serviceName: string;
  version: string;
  appEnv: AppEnv;
  logLevel: LogLevel;
  port: number;
  maxBodyBytes: number;
  shutdownTimeoutMs: number;
  /** Keeps serving (readiness 503) before refusing work, so a load balancer stops routing here. */
  drainDelayMs: number;
  /** Enables POST /api/v1/webhooks/inbound. Undefined: the endpoint answers 404. */
  webhookInboundSecret?: string;
  /** Receives a signed `note.created` per new note. Undefined: nothing is sent. */
  webhookTargetUrl?: string;
  webhookTargetSecret?: string;
  /** Set by the start command; without it Next.js handles SIGTERM itself (no drain). */
  manualSignalHandling: boolean;
}

export class ConfigError extends Error {
  constructor(readonly problems: string[]) {
    super("configuration:\n  " + problems.join("\n  "));
    this.name = "ConfigError";
  }
}

type Env = Record<string, string | undefined>;

/** Parses "15s", "500ms", "1m" or "0" into milliseconds. */
export function parseDuration(raw: string): number | undefined {
  const m = /^(\d+(?:\.\d+)?)(ms|s|m)?$/.exec(raw.trim());
  if (!m) return undefined;
  const n = Number(m[1]);
  const unit = m[2] ?? (n === 0 ? "s" : undefined);
  if (unit === undefined) return undefined;
  return Math.round(n * { ms: 1, s: 1000, m: 60_000 }[unit as "ms" | "s" | "m"]);
}

export function loadConfig(env: Env = process.env): Config {
  const problems: string[] = [];
  const get = (key: string, fallback = ""): string => {
    const v = env[key]?.trim();
    return v ? v : fallback;
  };
  const duration = (key: string, fallback: string): number => {
    const raw = get(key, fallback);
    const ms = parseDuration(raw);
    if (ms === undefined) {
      problems.push(`${key}: "${raw}" is not a non-negative duration (e.g. 15s, 500ms)`);
      return parseDuration(fallback) ?? 0;
    }
    return ms;
  };

  const appEnv = get("APP_ENV", "development");
  if (!["development", "test", "production"].includes(appEnv)) {
    problems.push(`APP_ENV: "${appEnv}" is not one of development, test, production`);
  }
  const logLevel = get("LOG_LEVEL", "info").toLowerCase();
  if (!["debug", "info", "warn", "error"].includes(logLevel)) {
    problems.push(`LOG_LEVEL: "${logLevel}" is not one of debug, info, warn, error`);
  }
  const portRaw = get("PORT", DEFAULT_PORT);
  const port = Number(portRaw);
  if (!/^\d+$/.test(portRaw) || port < 1 || port > 65535) {
    problems.push(`PORT: "${portRaw}" is not a port number (1-65535)`);
  }
  const maxBodyRaw = get("HTTP_MAX_BODY_BYTES", "1048576");
  const maxBodyBytes = Number(maxBodyRaw);
  if (!/^\d+$/.test(maxBodyRaw) || maxBodyBytes < 1) {
    problems.push(`HTTP_MAX_BODY_BYTES: "${maxBodyRaw}" is not a positive integer`);
  }

  const webhookInboundSecret = get("WEBHOOK_INBOUND_SECRET") || undefined;
  const webhookTargetUrl = get("WEBHOOK_TARGET_URL") || undefined;
  const webhookTargetSecret = get("WEBHOOK_TARGET_SECRET") || undefined;
  if (webhookTargetUrl) {
    let url: URL | undefined;
    try {
      url = new URL(webhookTargetUrl);
    } catch {
      url = undefined;
    }
    if (!url || (url.protocol !== "http:" && url.protocol !== "https:")) {
      problems.push(`WEBHOOK_TARGET_URL: "${webhookTargetUrl}" is not an http(s) URL`);
    } else if (appEnv === "production" && url.protocol !== "https:") {
      problems.push("WEBHOOK_TARGET_URL: production sends signed events over https only");
    }
    if (!webhookTargetSecret) {
      problems.push("WEBHOOK_TARGET_SECRET: required when WEBHOOK_TARGET_URL is set");
    }
  }

  const manualSignalHandling = get("NEXT_MANUAL_SIG_HANDLE") === "true";
  if (appEnv === "production" && !manualSignalHandling) {
    problems.push(
      "NEXT_MANUAL_SIG_HANDLE: must be true in production — without it Next.js exits on SIGTERM " +
        "without the readiness drain and the bounded shutdown (use `pnpm start` or the image)",
    );
  }

  const config: Config = {
    serviceName: get("SERVICE_NAME", PROJECT_NAME),
    version: get("APP_VERSION", "dev"),
    appEnv: appEnv as AppEnv,
    logLevel: logLevel as LogLevel,
    port,
    maxBodyBytes,
    shutdownTimeoutMs: duration("SHUTDOWN_TIMEOUT", "15s"),
    drainDelayMs: duration("DRAIN_DELAY", "0s"),
    webhookInboundSecret,
    webhookTargetUrl,
    webhookTargetSecret,
    manualSignalHandling,
  };
  if (problems.length > 0) {
    throw new ConfigError(problems);
  }
  return config;
}

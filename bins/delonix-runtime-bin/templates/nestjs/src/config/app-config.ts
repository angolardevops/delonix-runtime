// The service configuration, read from the environment and validated once, at
// startup. A bad value stops the process before it listens, with every
// problem listed at once. Nothing else in the code reads process.env
// (except src/telemetry/otel.ts, which runs before this, and reads only the
// OTEL_* variables and the service identity).

export type Environment = "development" | "test" | "production";
export type LogLevel = "debug" | "info" | "warn" | "error";

/** Defaults filled in when the project was generated. */
export const DEFAULT_SERVICE_NAME = "__NAME__";
export const DEFAULT_PORT = __PORT__;

const ENVIRONMENTS: readonly Environment[] = ["development", "test", "production"];
const LOG_LEVELS: readonly LogLevel[] = ["debug", "info", "warn", "error"];

/**
 * The whole runtime configuration. A class, so Nest can inject it by type
 * (`constructor(private readonly config: AppConfig)`).
 */
export class AppConfig {
  constructor(
    readonly serviceName: string,
    readonly version: string,
    readonly env: Environment,
    readonly logLevel: LogLevel,
    readonly host: string,
    readonly port: number,
    /** Request body limit, bytes. */
    readonly maxBodyBytes: number,
    /** Server timeouts, milliseconds. */
    readonly requestTimeoutMs: number,
    readonly headersTimeoutMs: number,
    readonly keepAliveTimeoutMs: number,
    /** Bound of the whole shutdown, milliseconds. */
    readonly shutdownTimeoutMs: number,
    /** Time serving with readiness at 503 before the listener closes. */
    readonly drainDelayMs: number,
    /** `whsec_…`; enables POST /api/v1/webhooks/inbound. Empty: endpoint off. */
    readonly webhookInboundSecret: string,
    /** Receiver of `note.created`. Empty: nothing is sent. */
    readonly webhookTargetUrl: string,
    readonly webhookTargetSecret: string,
  ) {}

  get isProduction(): boolean {
    return this.env === "production";
  }
}

export class ConfigError extends Error {
  constructor(readonly problems: string[]) {
    super("configuration:\n" + problems.map((p) => `  ${p}`).join("\n"));
    this.name = "ConfigError";
  }
}

const DURATION = /^(\d+)(ms|s|m)$/;

/** `500ms`, `15s`, `1m` → milliseconds. */
export function parseDuration(raw: string): number | undefined {
  const match = DURATION.exec(raw);
  if (match === null) {
    return undefined;
  }
  const value = Number(match[1]);
  const unit = match[2];
  return unit === "ms" ? value : unit === "s" ? value * 1000 : value * 60_000;
}

/** Reads `env` (process.env outside tests). Throws ConfigError listing every problem. */
export function loadConfig(env: NodeJS.ProcessEnv): AppConfig {
  const problems: string[] = [];
  const get = (key: string, fallback: string): string => {
    const value = (env[key] ?? "").trim();
    return value === "" ? fallback : value;
  };
  const duration = (key: string, fallback: string): number => {
    const raw = get(key, fallback);
    const ms = parseDuration(raw);
    if (ms === undefined) {
      problems.push(`${key}: "${raw}" is not a duration (e.g. 500ms, 15s, 1m)`);
      return parseDuration(fallback) ?? 0;
    }
    return ms;
  };
  const positiveInt = (key: string, fallback: number, max: number): number => {
    const raw = get(key, String(fallback));
    const value = /^\d+$/.test(raw) ? Number(raw) : NaN;
    if (!Number.isSafeInteger(value) || value < 1 || value > max) {
      problems.push(`${key}: "${raw}" is not an integer between 1 and ${max}`);
      return fallback;
    }
    return value;
  };

  const envName = get("APP_ENV", "development");
  if (!ENVIRONMENTS.includes(envName as Environment)) {
    problems.push(`APP_ENV: "${envName}" is not one of ${ENVIRONMENTS.join(", ")}`);
  }
  const logLevel = get("LOG_LEVEL", "info").toLowerCase();
  if (!LOG_LEVELS.includes(logLevel as LogLevel)) {
    problems.push(`LOG_LEVEL: "${logLevel}" is not one of ${LOG_LEVELS.join(", ")}`);
  }

  const inboundSecret = get("WEBHOOK_INBOUND_SECRET", "");
  if (inboundSecret !== "") {
    const problem = secretProblem(inboundSecret);
    if (problem !== undefined) {
      problems.push(`WEBHOOK_INBOUND_SECRET: ${problem}`);
    }
  }
  const targetUrl = get("WEBHOOK_TARGET_URL", "");
  const targetSecret = get("WEBHOOK_TARGET_SECRET", "");
  if (targetUrl !== "") {
    let url: URL | undefined;
    try {
      url = new URL(targetUrl);
    } catch {
      url = undefined;
    }
    if (url === undefined || (url.protocol !== "http:" && url.protocol !== "https:")) {
      problems.push(`WEBHOOK_TARGET_URL: "${targetUrl}" is not an http(s) URL`);
    } else if (envName === "production" && url.protocol !== "https:") {
      problems.push("WEBHOOK_TARGET_URL: production sends signed events over https only");
    }
    if (targetSecret === "") {
      problems.push("WEBHOOK_TARGET_SECRET: required when WEBHOOK_TARGET_URL is set");
    } else {
      const problem = secretProblem(targetSecret);
      if (problem !== undefined) {
        problems.push(`WEBHOOK_TARGET_SECRET: ${problem}`);
      }
    }
  }

  const config = new AppConfig(
    get("SERVICE_NAME", DEFAULT_SERVICE_NAME),
    get("APP_VERSION", "dev"),
    envName as Environment,
    logLevel as LogLevel,
    get("HTTP_HOST", "0.0.0.0"),
    positiveInt("PORT", DEFAULT_PORT, 65535),
    positiveInt("HTTP_MAX_BODY_BYTES", 1_048_576, 1 << 30),
    duration("HTTP_REQUEST_TIMEOUT", "15s"),
    duration("HTTP_HEADERS_TIMEOUT", "5s"),
    duration("HTTP_KEEP_ALIVE_TIMEOUT", "60s"),
    duration("SHUTDOWN_TIMEOUT", "15s"),
    duration("DRAIN_DELAY", "0s"),
    inboundSecret,
    targetUrl,
    targetSecret,
  );
  if (problems.length > 0) {
    throw new ConfigError(problems);
  }
  return config;
}

/** Standard Webhooks secret: `whsec_` + base64 of at least 24 bytes. */
export function secretProblem(secret: string): string | undefined {
  if (!secret.startsWith("whsec_")) {
    return 'must look like "whsec_<base64>"';
  }
  const raw = secret.slice("whsec_".length);
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(raw) || raw.length % 4 !== 0) {
    return "is not valid base64 after whsec_";
  }
  if (Buffer.from(raw, "base64").length < 24) {
    return "must decode to at least 24 bytes";
  }
  return undefined;
}

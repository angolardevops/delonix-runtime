import { describe, expect, it } from "vitest";
import { ConfigError, loadConfig, parseDuration } from "@/lib/server/config";
import { DEFAULT_PORT, PROJECT_NAME } from "@/lib/project";
import { buildRuntime } from "@/lib/server/runtime";
import { createLogger } from "@/lib/server/logger";
import { TEST_SECRET } from "./helpers";

const problems = (env: Record<string, string>): string[] => {
  try {
    loadConfig(env);
    return [];
  } catch (err) {
    if (err instanceof ConfigError) return err.problems;
    throw err;
  }
};

describe("configuration", () => {
  it("has working defaults", () => {
    expect(loadConfig({})).toMatchObject({
      serviceName: PROJECT_NAME,
      appEnv: "development",
      logLevel: "info",
      port: Number(DEFAULT_PORT),
      maxBodyBytes: 1_048_576,
      shutdownTimeoutMs: 15_000,
      drainDelayMs: 0,
      manualSignalHandling: false,
    });
  });

  it("parses durations", () => {
    expect(parseDuration("15s")).toBe(15_000);
    expect(parseDuration("500ms")).toBe(500);
    expect(parseDuration("1.5m")).toBe(90_000);
    expect(parseDuration("0")).toBe(0);
    expect(parseDuration("15")).toBeUndefined();
    expect(parseDuration("-1s")).toBeUndefined();
  });

  it("lists every problem at once", () => {
    const found = problems({
      APP_ENV: "staging",
      LOG_LEVEL: "loud",
      PORT: "99999",
      HTTP_MAX_BODY_BYTES: "0",
      SHUTDOWN_TIMEOUT: "soon",
      WEBHOOK_TARGET_URL: "ftp://x",
    });
    expect(found.map((p) => p.split(":")[0])).toEqual([
      "APP_ENV",
      "LOG_LEVEL",
      "PORT",
      "HTTP_MAX_BODY_BYTES",
      "WEBHOOK_TARGET_URL",
      "WEBHOOK_TARGET_SECRET",
      "SHUTDOWN_TIMEOUT",
    ]);
  });

  it("production refuses the development shortcuts", () => {
    const found = problems({
      APP_ENV: "production",
      WEBHOOK_TARGET_URL: "http://receiver.test/hook",
      WEBHOOK_TARGET_SECRET: TEST_SECRET,
    });
    expect(found).toHaveLength(2);
    expect(found[0]).toContain("https only");
    expect(found[1]).toContain("NEXT_MANUAL_SIG_HANDLE");
    expect(
      problems({
        APP_ENV: "production",
        NEXT_MANUAL_SIG_HANDLE: "true",
        WEBHOOK_TARGET_URL: "https://receiver.test/hook",
        WEBHOOK_TARGET_SECRET: TEST_SECRET,
      }),
    ).toEqual([]);
  });

  it("refuses a webhook secret that is not whsec_<base64> when the runtime is built", () => {
    const config = loadConfig({ WEBHOOK_INBOUND_SECRET: "plain-text" });
    const log = createLogger({
      service: "s",
      version: "v",
      environment: "test",
      level: "error",
      write: () => {},
    });
    expect(() =>
      buildRuntime(config, { log, telemetry: { shutdown: async () => true }, afterResponse: () => {} }),
    ).toThrow(/WEBHOOK_INBOUND_SECRET: webhook secret must look like/);
  });
});

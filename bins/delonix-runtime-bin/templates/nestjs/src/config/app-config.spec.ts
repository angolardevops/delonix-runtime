import { ConfigError, DEFAULT_PORT, DEFAULT_SERVICE_NAME, loadConfig, parseDuration } from "./app-config";

const SECRET = "whsec_" + Buffer.alloc(32, 1).toString("base64");

describe("loadConfig", () => {
  it("has working defaults", () => {
    const config = loadConfig({});
    expect(config).toMatchObject({
      serviceName: DEFAULT_SERVICE_NAME,
      env: "development",
      logLevel: "info",
      host: "0.0.0.0",
      port: DEFAULT_PORT,
      maxBodyBytes: 1_048_576,
      shutdownTimeoutMs: 15_000,
      drainDelayMs: 0,
      webhookInboundSecret: "",
      webhookTargetUrl: "",
    });
    expect(config.isProduction).toBe(false);
  });

  it("reads and trims values", () => {
    const config = loadConfig({
      PORT: " 9000 ",
      APP_ENV: "production",
      SHUTDOWN_TIMEOUT: "500ms",
      DRAIN_DELAY: "1m",
    });
    expect(config).toMatchObject({
      port: 9000,
      env: "production",
      shutdownTimeoutMs: 500,
      drainDelayMs: 60_000,
    });
    expect(config.isProduction).toBe(true);
  });

  it("lists every problem at once", () => {
    const error = (() => {
      try {
        loadConfig({
          APP_ENV: "staging",
          LOG_LEVEL: "loud",
          PORT: "70000",
          SHUTDOWN_TIMEOUT: "soon",
          HTTP_MAX_BODY_BYTES: "-1",
          WEBHOOK_INBOUND_SECRET: "plain",
        });
      } catch (e) {
        return e;
      }
      return undefined;
    })();
    expect(error).toBeInstanceOf(ConfigError);
    const problems = (error as ConfigError).problems.map((p) => p.split(":")[0]);
    expect(problems).toEqual([
      "APP_ENV",
      "LOG_LEVEL",
      "WEBHOOK_INBOUND_SECRET",
      "PORT",
      "HTTP_MAX_BODY_BYTES",
      "SHUTDOWN_TIMEOUT",
    ]);
    // The message never repeats a secret's value.
    expect((error as ConfigError).message).not.toContain("plain");
  });

  it("requires the secret with the outbound URL, and a real whsec_ secret", () => {
    expect(() => loadConfig({ WEBHOOK_TARGET_URL: "http://localhost:9/hook" })).toThrow(
      /WEBHOOK_TARGET_SECRET: required/,
    );
    expect(() => loadConfig({ WEBHOOK_TARGET_URL: "ftp://x", WEBHOOK_TARGET_SECRET: SECRET })).toThrow(
      /not an http/,
    );
    expect(() =>
      loadConfig({ WEBHOOK_TARGET_URL: "http://localhost:9/hook", WEBHOOK_TARGET_SECRET: "whsec_c2hvcnQ=" }),
    ).toThrow(/at least 24 bytes/);
    expect(
      loadConfig({ WEBHOOK_TARGET_URL: "http://localhost:9/hook", WEBHOOK_TARGET_SECRET: SECRET }),
    ).toMatchObject({
      webhookTargetUrl: "http://localhost:9/hook",
    });
  });

  it("production refuses to send signed events over plain http", () => {
    const env = { APP_ENV: "production", WEBHOOK_TARGET_SECRET: SECRET };
    expect(() => loadConfig({ ...env, WEBHOOK_TARGET_URL: "http://hooks.example/x" })).toThrow(/https only/);
    expect(() => loadConfig({ ...env, WEBHOOK_TARGET_URL: "https://hooks.example/x" })).not.toThrow();
  });
});

describe("parseDuration", () => {
  it.each([
    ["0s", 0],
    ["250ms", 250],
    ["15s", 15_000],
    ["2m", 120_000],
  ])("%s → %d ms", (raw, ms) => {
    expect(parseDuration(raw)).toBe(ms);
  });

  it.each(["", "15", "-1s", "1h", "1.5s", "s"])("refuses %p", (raw) => {
    expect(parseDuration(raw)).toBeUndefined();
  });
});

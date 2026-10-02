import assert from "node:assert/strict";
import { test } from "node:test";
import { ConfigError, loadConfig } from "../src/config.js";

test("the defaults are valid", () => {
  const cfg = loadConfig({}, "dev");
  assert.equal(cfg.port, __PORT__);
  assert.equal(cfg.env, "development");
  assert.equal(cfg.shutdownTimeoutMs, 15_000);
});

test("every bad value is reported at once", () => {
  assert.throws(
    () =>
      loadConfig(
        { PORT: "http", APP_ENV: "staging", LOG_LEVEL: "loud", SHUTDOWN_TIMEOUT: "soon" },
        "dev",
      ),
    (err: unknown) =>
      err instanceof ConfigError &&
      ["PORT", "APP_ENV", "LOG_LEVEL", "SHUTDOWN_TIMEOUT"].every((k) =>
        err.problems.some((p) => p.startsWith(k)),
      ),
  );
});

test("an outbound webhook needs a secret, and https in production", () => {
  assert.throws(() => loadConfig({ WEBHOOK_TARGET_URL: "http://hooks.example" }, "dev"), /SECRET/);
  assert.throws(
    () =>
      loadConfig(
        {
          APP_ENV: "production",
          WEBHOOK_TARGET_URL: "http://hooks.example",
          WEBHOOK_TARGET_SECRET: "s",
        },
        "dev",
      ),
    /https/,
  );
});

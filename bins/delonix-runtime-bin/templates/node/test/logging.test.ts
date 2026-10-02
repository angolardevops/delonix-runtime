import assert from "node:assert/strict";
import { test } from "node:test";
import { createLogger, REDACTED } from "../src/logging.js";
import { logSink } from "./helpers.js";

test("secrets are redacted by key and the service identity is on every line", () => {
  const sink = logSink();
  const log = createLogger(
    { logLevel: "info", serviceName: "svc", version: "1.0.0", env: "test" },
    sink.stream,
  );
  log.info({ Authorization: "Bearer abc", webhook_secret: "s3cr3t", user: "ana" }, "hello");
  const [line] = sink.lines();
  assert.equal(line?.Authorization, REDACTED);
  assert.equal(line?.webhook_secret, REDACTED);
  assert.equal(line?.user, "ana");
  assert.equal(line?.service, "svc");
  assert.equal(line?.environment, "test");
  const raw = JSON.stringify(sink.lines());
  assert.ok(!raw.includes("s3cr3t") && !raw.includes("abc"), raw);
});

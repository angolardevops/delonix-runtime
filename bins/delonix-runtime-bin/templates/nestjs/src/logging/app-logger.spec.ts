import { context, trace, TraceFlags } from "@opentelemetry/api";
import { AsyncLocalStorageContextManager } from "@opentelemetry/context-async-hooks";

import { AppLogger, redact, REDACTED } from "./app-logger";

function capture(level: "debug" | "info" | "warn" | "error" = "debug") {
  const lines: string[] = [];
  const logger = AppLogger.create({
    level,
    service: "svc",
    version: "1.2.3",
    environment: "test",
    destination: { write: (line: string) => void lines.push(line) },
  });
  return { logger, records: () => lines.map((l) => JSON.parse(l) as Record<string, unknown>) };
}

describe("AppLogger", () => {
  it("writes one JSON line with the service identity", () => {
    const { logger, records } = capture();
    logger.log("hello", { note_id: "n1" });
    expect(records()).toEqual([
      {
        level: "info",
        time: expect.stringMatching(/^\d{4}-\d\d-\d\dT/),
        service: "svc",
        version: "1.2.3",
        environment: "test",
        note_id: "n1",
        msg: "hello",
      },
    ]);
  });

  it("redacts by key name, at any depth", () => {
    const { logger, records } = capture();
    logger.warn("careless", {
      Authorization: "Bearer abc",
      webhook_signature: "v1,xyz",
      nested: { db_password: "hunter2", list: [{ api_key: "k" }], fine: "visible" },
    });
    const line = JSON.stringify(records()[0]);
    expect(line).not.toMatch(/Bearer abc|v1,xyz|hunter2|"k"/);
    expect(records()[0]).toMatchObject({
      Authorization: REDACTED,
      webhook_signature: REDACTED,
      nested: { db_password: REDACTED, list: [{ api_key: REDACTED }], fine: "visible" },
    });
  });

  it("adds trace_id and span_id inside an active span, and nothing outside", () => {
    const manager = new AsyncLocalStorageContextManager().enable();
    context.setGlobalContextManager(manager);
    try {
      const { logger, records } = capture();
      const spanContext = {
        traceId: "0af7651916cd43dd8448eb211c80319c",
        spanId: "b7ad6b7169203331",
        traceFlags: TraceFlags.SAMPLED,
      };
      context.with(trace.setSpanContext(context.active(), spanContext), () => logger.log("inside"));
      logger.log("outside");
      expect(records()[0]).toMatchObject({ trace_id: spanContext.traceId, span_id: spanContext.spanId });
      expect(records()[1]).not.toHaveProperty("trace_id");
    } finally {
      context.disable();
    }
  });

  it("honours the level", () => {
    const { logger, records } = capture("warn");
    logger.debug("no");
    logger.log("no");
    logger.warn("yes");
    logger.error("yes");
    expect(records().map((r) => r.level)).toEqual(["warn", "error"]);
  });

  it("speaks Nest's LoggerService: context strings and error stacks", () => {
    const { logger, records } = capture();
    logger.log("Mapped {/x, GET} route", "RouterExplorer");
    logger.error("boom", "Error: boom\n    at x", "ExceptionsHandler");
    expect(records()[0]).toMatchObject({ level: "info", context: "RouterExplorer" });
    expect(records()[1]).toMatchObject({
      level: "error",
      context: "ExceptionsHandler",
      stack: "Error: boom\n    at x",
    });
  });
});

describe("redact", () => {
  it("leaves the input untouched", () => {
    const input = { token: "t", ok: 1 };
    expect(redact(input)).toEqual({ token: REDACTED, ok: 1 });
    expect(input.token).toBe("t");
  });
});

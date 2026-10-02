import { describe, expect, it } from "vitest";
import { createLogger, REDACTED } from "@/lib/server/logger";
import { runWithRequest } from "@/lib/server/request-context";

function capture(level: "debug" | "info" = "info") {
  const lines: Array<Record<string, unknown>> = [];
  const log = createLogger({
    service: "svc",
    version: "1.2.3",
    environment: "test",
    level,
    write: (line) => lines.push(JSON.parse(line) as Record<string, unknown>),
    now: () => new Date("2026-01-01T00:00:00Z"),
  });
  return { log, lines };
}

describe("logger", () => {
  it("writes one JSON object per line with the service identity", () => {
    const { log, lines } = capture();
    log.info("hello", { note_id: "n1" });
    expect(lines).toEqual([
      {
        time: "2026-01-01T00:00:00.000Z",
        level: "info",
        msg: "hello",
        service: "svc",
        version: "1.2.3",
        environment: "test",
        note_id: "n1",
      },
    ]);
  });

  it("redacts values by key name", () => {
    const { log, lines } = capture();
    log.info("careless", {
      Authorization: "Bearer abc",
      webhook_signature: "v1,xyz",
      db_password: "hunter2",
      api_key: "k",
      cookie: "c",
      session_token: "t",
      client_secret: "s",
      title: "kept",
    });
    const line = lines[0]!;
    for (const key of [
      "Authorization",
      "webhook_signature",
      "db_password",
      "api_key",
      "cookie",
      "session_token",
      "client_secret",
    ]) {
      expect(line[key], key).toBe(REDACTED);
    }
    expect(line.title).toBe("kept");
    expect(JSON.stringify(line)).not.toContain("hunter2");
  });

  it("drops lines below the level and adds the request id inside a request", () => {
    const { log, lines } = capture();
    log.debug("noise");
    runWithRequest({ requestId: "req-1" }, () => log.warn("inside"));
    log.error("outside");
    expect(lines.map((l) => [l.msg, l.request_id])).toEqual([
      ["inside", "req-1"],
      ["outside", undefined],
    ]);
  });
});

import { type ChildProcess, execFileSync, spawn } from "node:child_process";
import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { join } from "node:path";

import { parseSecret, verify } from "../src/webhooks/signature";
import { TEST_SECRET } from "./support/app";

/**
 * The one-trace gate. One request that creates a note must leave ONE trace
 * across the incoming request, the use case, the outbound webhook call and
 * the log lines — that is what makes a failure findable from any of the three.
 *
 * It runs the service the way production does — a separate process started
 * with `node --require instrumentation.js main.js` — because that is the
 * wiring under test: the HTTP instrumentation only works when it is loaded
 * before Nest. The process exports to a fake OTLP/HTTP collector in this
 * test, and delivers its webhook to a fake receiver.
 */
const ROOT = join(__dirname, "..");
const OUT = join(ROOT, ".tmp", "trace-e2e");

interface ExportedSpan {
  traceId: string;
  spanId: string;
  parentSpanId?: string;
  name: string;
  attributes: Record<string, unknown>;
}

const readBody = async (req: IncomingMessage): Promise<Buffer> => {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(chunk as Buffer);
  }
  return Buffer.concat(chunks);
};

const listen = (server: Server): Promise<number> =>
  new Promise((resolve) =>
    server.listen(0, "127.0.0.1", () => resolve((server.address() as AddressInfo).port)),
  );

const until = async (what: string, done: () => boolean, timeoutMs = 30_000): Promise<void> => {
  const started = Date.now();
  while (!done()) {
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
};

describe("one trace across request, use case, outbound call and logs", () => {
  const spans: ExportedSpan[] = [];
  const deliveries: { traceparent?: string; signatureOk: boolean }[] = [];
  const logs: Record<string, unknown>[] = [];
  let collector: Server;
  let receiver: Server;
  let service: ChildProcess;
  let base: string;
  let exitCode: Promise<number | null>;

  beforeAll(async () => {
    // The service's own sources, compiled like `pnpm build` does.
    execFileSync(
      process.execPath,
      [
        require.resolve("typescript/bin/tsc"),
        "-p",
        "tsconfig.build.json",
        "--outDir",
        OUT,
        "--sourceMap",
        "false",
      ],
      { cwd: ROOT, stdio: "inherit" },
    );

    // A fake OTLP/HTTP collector: the exporter posts JSON to /v1/traces.
    collector = createServer((req, res) => {
      void readBody(req).then((body) => {
        if (req.url === "/v1/traces") {
          const payload = JSON.parse(body.toString()) as {
            resourceSpans?: { scopeSpans?: { spans?: Record<string, unknown>[] }[] }[];
          };
          for (const resource of payload.resourceSpans ?? []) {
            for (const scope of resource.scopeSpans ?? []) {
              for (const span of scope.spans ?? []) {
                const attributes: Record<string, unknown> = {};
                for (const { key, value } of (span.attributes ?? []) as {
                  key: string;
                  value: Record<string, unknown>;
                }[]) {
                  attributes[key] = Object.values(value)[0];
                }
                spans.push({ ...(span as unknown as ExportedSpan), attributes });
              }
            }
          }
        }
        res.writeHead(200, { "content-type": "application/json" }).end("{}");
      });
    });
    const collectorPort = await listen(collector);

    // A fake webhook receiver: verifies the signature over the raw bytes.
    const key = parseSecret(TEST_SECRET);
    receiver = createServer((req, res) => {
      void readBody(req).then((body) => {
        const header = (name: string): string | undefined => req.headers[name] as string | undefined;
        deliveries.push({
          traceparent: header("traceparent"),
          signatureOk:
            verify(
              key,
              header("webhook-id"),
              header("webhook-timestamp"),
              header("webhook-signature"),
              body,
              Math.floor(Date.now() / 1000),
            ) === "ok",
        });
        res.writeHead(204).end();
      });
    });
    const receiverPort = await listen(receiver);

    // A free port for the service.
    const probe = createServer();
    const port = await listen(probe);
    await new Promise((resolve) => probe.close(resolve));
    base = `http://127.0.0.1:${port}`;

    service = spawn(process.execPath, ["--require", join(OUT, "instrumentation.js"), join(OUT, "main.js")], {
      cwd: ROOT,
      env: {
        PATH: process.env.PATH,
        APP_ENV: "test",
        PORT: String(port),
        HTTP_HOST: "127.0.0.1",
        SHUTDOWN_TIMEOUT: "10s",
        WEBHOOK_TARGET_URL: `http://127.0.0.1:${receiverPort}/hook`,
        WEBHOOK_TARGET_SECRET: TEST_SECRET,
        OTEL_EXPORTER_OTLP_ENDPOINT: `http://127.0.0.1:${collectorPort}`,
        OTEL_BSP_SCHEDULE_DELAY: "100",
      },
      stdio: ["ignore", "pipe", "inherit"],
    });
    exitCode = new Promise((resolve) => service.once("exit", (code) => resolve(code)));
    let pending = "";
    service.stdout?.on("data", (chunk: Buffer) => {
      const lines = (pending + chunk.toString()).split("\n");
      pending = lines.pop() ?? "";
      for (const line of lines) {
        try {
          logs.push(JSON.parse(line) as Record<string, unknown>);
        } catch {
          logs.push({ msg: line });
        }
      }
    });
    await until("the service to listen", () => logs.some((l) => l.msg === "listening"), 60_000);
  }, 240_000);

  afterAll(async () => {
    service?.kill("SIGKILL");
    await Promise.all([collector, receiver].map((s) => new Promise((resolve) => s?.close(resolve))));
  });

  it("request, use case, webhook delivery, outbound call and log lines share one trace id", async () => {
    await fetch(`${base}/api/v1/health/live`);
    const created = await fetch(`${base}/api/v1/notes`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ title: "traced" }),
    });
    expect(created.status).toBe(201);

    await until("the webhook delivery", () => deliveries.length > 0);
    expect(deliveries[0].signatureOk).toBe(true);
    const span = (name: string): ExportedSpan | undefined => spans.find((s) => s.name === name);
    await until("the spans to be exported", () =>
      ["POST /api/v1/notes", "notes.create", "webhook.deliver", "POST"].every((n) => span(n) !== undefined),
    );

    const server = span("POST /api/v1/notes");
    const traceId = server?.traceId;
    expect(traceId).toMatch(/^[0-9a-f]{32}$/);
    expect(server?.attributes["http.route"]).toBe("/api/v1/notes");
    // The use case and the delivery are children of the request…
    expect(span("notes.create")).toMatchObject({ traceId, parentSpanId: server?.spanId });
    expect(span("webhook.deliver")).toMatchObject({ traceId });
    // …and the outbound HTTP call is a child of the delivery, carrying the id to the receiver.
    expect(span("POST")).toMatchObject({ traceId, parentSpanId: span("webhook.deliver")?.spanId });
    expect(deliveries[0].traceparent).toContain(`-${traceId}-`);

    await until("the log lines", () => logs.some((l) => l.msg === "webhook delivered"));
    const inTrace = logs.filter((l) => l.trace_id === traceId).map((l) => l.msg);
    expect(inTrace).toEqual(expect.arrayContaining(["request", "webhook delivered"]));

    // Health probes are not traced.
    const probed = (s: ExportedSpan): boolean => {
      const path = s.attributes["url.path"];
      return typeof path === "string" && path.startsWith("/api/v1/health");
    };
    expect(spans.some(probed)).toBe(false);
  }, 60_000);

  it("stops on SIGTERM with exit code 0, after logging the ordered shutdown", async () => {
    service.kill("SIGTERM");
    expect(await exitCode).toBe(0);
    const messages = logs.map((l) => l.msg);
    expect(messages.indexOf("shutting down")).toBeGreaterThan(-1);
    expect(messages.indexOf("stopped")).toBeGreaterThan(messages.indexOf("shutting down"));
  }, 30_000);
});

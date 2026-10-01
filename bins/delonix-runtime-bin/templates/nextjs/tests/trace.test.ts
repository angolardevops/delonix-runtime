import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { context, SpanKind, trace } from "@opentelemetry/api";
import { InMemorySpanExporter, SimpleSpanProcessor } from "@opentelemetry/sdk-trace-base";
import { NodeTracerProvider } from "@opentelemetry/sdk-trace-node";
import { afterAll, beforeAll, expect, it } from "vitest";
import * as notes from "@/app/api/v1/notes/route";
import { parseSecret, verify } from "@/lib/server/webhooks/signature";
import { installRuntime, request, TEST_SECRET } from "./helpers";

const exporter = new InMemorySpanExporter();
const provider = new NodeTracerProvider({ spanProcessors: [new SimpleSpanProcessor(exporter)] });

beforeAll(() => provider.register());
afterAll(() => provider.shutdown());

// One request that creates a note must leave ONE trace across the incoming
// request, the use case, the outbound webhook call and the log lines — that
// is what makes a failure findable from any of the three.
//
// In production Next.js opens the request span before it calls the Route
// Handler. Here the handler is called directly, so the test opens the span
// Next.js would have opened; everything below it is the project's own code.
it("one trace across the request, the use case, the outbound call and the log lines", async () => {
  const received: Array<{ traceparent?: string; signatureOk: boolean }> = [];
  const key = parseSecret(TEST_SECRET);
  const receiver = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => {
      const header = (name: string) => (req.headers[name] as string | undefined) ?? null;
      const result = verify(
        key,
        {
          id: header("webhook-id"),
          timestamp: header("webhook-timestamp"),
          signature: header("webhook-signature"),
        },
        Buffer.concat(chunks),
        Math.floor(Date.now() / 1000),
      );
      received.push({
        traceparent: req.headers.traceparent as string | undefined,
        signatureOk: result === "ok",
      });
      res.writeHead(204).end();
    });
  });
  await new Promise<void>((resolve) => receiver.listen(0, "127.0.0.1", resolve));
  const port = (receiver.address() as AddressInfo).port;

  try {
    const t = installRuntime({
      env: { WEBHOOK_TARGET_URL: `http://127.0.0.1:${port}/hook`, WEBHOOK_TARGET_SECRET: TEST_SECRET },
    });
    const server = trace.getTracer("test").startSpan("POST /api/v1/notes", { kind: SpanKind.SERVER });
    const traceId = server.spanContext().traceId;
    const response = await context.with(trace.setSpan(context.active(), server), () =>
      notes.POST(request("POST", "/api/v1/notes", '{"title":"traced"}'), {}),
    );
    server.end();
    expect(response.status).toBe(201);

    // The delivery runs after the response, outside the request's context.
    await t.flush();
    expect(await t.runtime.dispatcher!.close(5_000)).toBe(true);

    const traces = new Map(exporter.getFinishedSpans().map((s) => [s.name, s.spanContext().traceId]));
    for (const name of ["POST /api/v1/notes", "notes.create", "webhook.deliver", "POST"]) {
      expect(traces.get(name), `span "${name}" (spans: ${[...traces.keys()].join(", ")})`).toBe(traceId);
    }
    expect(received).toHaveLength(1);
    expect(received[0]!.signatureOk).toBe(true);
    // traceparent: 00-<trace id>-<span id>-<flags>
    expect(received[0]!.traceparent).toContain(traceId);

    const traced = t.logs.filter((l) => l.trace_id === traceId).map((l) => l.msg);
    expect(traced).toContain("request");
    expect(traced).toContain("webhook delivered");
  } finally {
    receiver.close();
  }
});

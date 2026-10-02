import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { test } from "node:test";
import { TEST_KEY } from "./helpers.js";

interface OtlpSpan {
  name: string;
  traceId: string;
}

/**
 * One request that creates a note must leave ONE trace across the incoming
 * request, the use case, the outbound webhook call and the log lines — that is
 * what makes a failure findable from any of the three.
 *
 * It runs the REAL process (instrumentation loaded with --import, as in
 * production) against a local server that plays both the OTLP collector and
 * the webhook receiver, and it stops the process with SIGTERM: the spans only
 * reach the collector if shutdown flushes them, and the exit code must be 0.
 */
test("one trace across request, use case, outbound call and logs", async () => {
  const spans: OtlpSpan[] = [];
  let traceparent: string | undefined;
  const peer = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      if (req.url === "/v1/traces") {
        const body = JSON.parse(Buffer.concat(chunks).toString("utf8")) as {
          resourceSpans?: { scopeSpans?: { spans?: OtlpSpan[] }[] }[];
        };
        for (const rs of body.resourceSpans ?? [])
          for (const ss of rs.scopeSpans ?? []) spans.push(...(ss.spans ?? []));
      } else if (req.url === "/hook") {
        traceparent = req.headers.traceparent as string | undefined;
      }
      res.statusCode = req.url === "/hook" ? 204 : 200;
      res.setHeader("content-type", "application/json");
      res.end(req.url === "/hook" ? undefined : "{}");
    });
  });
  await new Promise<void>((resolve) => peer.listen(0, "127.0.0.1", resolve));
  const peerUrl = `http://127.0.0.1:${(peer.address() as AddressInfo).port}`;

  const port = 20000 + Math.floor(Math.random() * 20000);
  const child = spawn(
    process.execPath,
    ["--import", "tsx", "--import", "./src/instrumentation.ts", "src/index.ts"],
    {
      env: {
        ...process.env,
        PORT: String(port),
        HTTP_HOST: "127.0.0.1",
        APP_ENV: "test",
        OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: `${peerUrl}/v1/traces`,
        WEBHOOK_TARGET_URL: `${peerUrl}/hook`,
        WEBHOOK_TARGET_SECRET: "whsec_" + TEST_KEY.toString("base64"),
      },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (c: Buffer) => (stdout += String(c)));
  child.stderr.on("data", (c: Buffer) => (stderr += String(c)));
  const exited = new Promise<number | null>((resolve) => child.on("exit", resolve));

  try {
    const base = `http://127.0.0.1:${port}`;
    let ready = false;
    for (let i = 0; i < 150 && !ready; i++) {
      ready = await fetch(`${base}/api/v1/health/ready`).then(
        (r) => r.ok,
        () => false,
      );
      if (!ready) await new Promise((r) => setTimeout(r, 100));
    }
    assert.ok(ready, `the service never became ready\n${stderr}`);

    const res = await fetch(`${base}/api/v1/notes`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ title: "traced" }),
    });
    assert.equal(res.status, 201);
    for (let i = 0; i < 100 && !traceparent; i++) await new Promise((r) => setTimeout(r, 50));
    assert.ok(traceparent, "the outbound webhook never arrived");
  } finally {
    child.kill("SIGTERM");
  }
  assert.equal(await exited, 0, `SIGTERM must end in a clean exit\n${stderr}`);
  peer.close();

  const server = spans.find((s) => s.name === "POST /api/v1/notes");
  assert.ok(server, `no server span; got ${spans.map((s) => s.name).join(", ")}`);
  for (const name of ["notes.create", "webhook.deliver"]) {
    const span = spans.find((s) => s.name === name);
    assert.equal(span?.traceId, server.traceId, `span ${name} is not in the request's trace`);
  }
  // traceparent: 00-<trace id>-<span id>-<flags>
  assert.ok(traceparent?.includes(server.traceId), `outbound traceparent ${traceparent}`);
  const logged = stdout
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l) as { msg?: string; trace_id?: string })
    .filter((l) => l.trace_id === server.traceId)
    .map((l) => l.msg);
  assert.ok(logged.includes("request"), `no access log line in the trace: ${logged.join(", ")}`);
  assert.ok(logged.includes("webhook delivered"), `no delivery log line in the trace`);
});

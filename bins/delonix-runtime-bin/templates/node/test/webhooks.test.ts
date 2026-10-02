import assert from "node:assert/strict";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { test } from "node:test";
import { createLogger } from "../src/logging.js";
import { WebhookDispatcher } from "../src/webhooks/dispatcher.js";
import {
  HEADER_ID,
  HEADER_SIGNATURE,
  HEADER_TIMESTAMP,
  parseSecret,
  sign,
} from "../src/webhooks/signature.js";
import { logSink, TEST_KEY, testApp } from "./helpers.js";

const NOW = 1_800_000_000;
const signed = (id: string, body: string, ts = NOW, key = TEST_KEY) => ({
  [HEADER_ID]: id,
  [HEADER_TIMESTAMP]: String(ts),
  [HEADER_SIGNATURE]: sign(key, id, ts, Buffer.from(body)),
  "content-type": "application/json",
});

test("an inbound webhook is verified over the raw bytes and de-duplicated", async () => {
  const { app } = await testApp({ now: () => NOW });
  const url = "/api/v1/webhooks/inbound";
  // Spacing a JSON re-encoder would change: the signature must cover it as sent.
  const body = '{"type":"note.create",  "data":{"title":"from a webhook"}}';
  const post = (payload: string, headers: Record<string, string>) =>
    app.inject({ method: "POST", url, payload, headers });

  assert.equal((await post(body, { "content-type": "application/json" })).statusCode, 400);
  assert.equal(
    (await post(body, signed("m1", body, NOW, Buffer.alloc(32, 9)))).statusCode,
    401,
    "wrong key",
  );
  assert.equal(
    (await post(body, signed("m1", body, NOW - 600))).statusCode,
    401,
    "stale timestamp (replay)",
  );
  assert.equal(
    (await post(body.replace("from", "FROM"), signed("m1", body))).statusCode,
    401,
    "tampered body",
  );
  for (let i = 0; i < 2; i++)
    assert.equal((await post(body, signed("m1", body))).statusCode, 204, `delivery ${i}`);
  const page = await app.inject({ url: "/api/v1/notes" });
  assert.equal(page.json().total, 1, "a duplicate delivery was processed twice");

  // A delivery that failed is not remembered: its retry is processed.
  const bad = '{"type":"note.create","data":{"title":""}}';
  assert.equal((await post(bad, signed("m2", bad))).statusCode, 422);
  const good = '{"type":"note.create","data":{"title":"second try"}}';
  assert.equal((await post(good, signed("m2", good))).statusCode, 204);
  assert.equal((await app.inject({ url: "/api/v1/notes" })).json().total, 2);
  await app.close();
});

test("a webhook secret must be whsec_<base64> of at least 24 bytes", () => {
  assert.equal(parseSecret("whsec_" + TEST_KEY.toString("base64")).length, 32);
  for (const bad of ["", "plain", "whsec_!!", "whsec_" + Buffer.from("short").toString("base64")])
    assert.throws(() => parseSecret(bad), `accepted ${JSON.stringify(bad)}`);
});

/** A receiver that answers with the given statuses in order, then 204. */
async function receiver(statuses: number[]) {
  const calls: { id: string | undefined }[] = [];
  const server = createServer((req, res) => {
    calls.push({ id: req.headers[HEADER_ID] as string | undefined });
    req.resume();
    res.statusCode = statuses[calls.length - 1] ?? 204;
    res.end();
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return { calls, url: `http://127.0.0.1:${port}/hook`, close: () => server.close() };
}

function dispatcher(url: string, maxAttempts: number) {
  const log = createLogger(
    { logLevel: "info", serviceName: "svc", version: "t", env: "test" },
    logSink().stream,
  );
  return new WebhookDispatcher(
    {
      url,
      key: TEST_KEY,
      timeoutMs: 1000,
      maxAttempts,
      baseBackoffMs: 1,
      queueSize: 8,
      sleep: async () => {},
    },
    log,
  );
}

const note = (id: string) => ({ id, title: "t", body: "", created_at: "2026-01-01T00:00:00.000Z" });

test("server errors are retried with the same idempotency id, then delivered", async () => {
  const rx = await receiver([503, 503]);
  const d = dispatcher(rx.url, 4);
  d.noteCreated(note("n1"));
  assert.equal(await d.close(5000), true);
  assert.equal(rx.calls.length, 3);
  assert.ok(rx.calls.every((c) => c.id === "n1"));
  rx.close();
});

test("a client error is final", async () => {
  const rx = await receiver([400, 400, 400]);
  const d = dispatcher(rx.url, 4);
  d.noteCreated(note("n1"));
  await d.close(5000);
  assert.equal(rx.calls.length, 1, "a 400 was retried");
  rx.close();
});

test("close is bounded, and later events are dropped without throwing", async () => {
  const server = createServer(() => {
    /* never answers */
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  const d = dispatcher(`http://127.0.0.1:${port}/hook`, 1);
  d.noteCreated(note("slow"));
  const started = Date.now();
  assert.equal(await d.close(50), false, "close reported a drained queue with a delivery stuck");
  assert.ok(Date.now() - started < 900, "close did not respect its deadline");
  d.noteCreated(note("late"));
  server.closeAllConnections();
  server.close();
});

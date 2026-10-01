import request from "supertest";

import { parseSecret, sign } from "../src/webhooks/signature";
import { startTestApp, TEST_SECRET, type TestApp } from "./support/app";

const key = parseSecret(TEST_SECRET);
const nowSeconds = (): number => Math.floor(Date.now() / 1000);

/** Sends `body` exactly as given (a string: supertest does not re-serialise it). */
function deliver(
  t: TestApp,
  id: string,
  body: string,
  options: { timestamp?: number; signature?: string } = {},
) {
  const timestamp = options.timestamp ?? nowSeconds();
  return request(t.server)
    .post("/api/v1/webhooks/inbound")
    .set("Content-Type", "application/json")
    .set("webhook-id", id)
    .set("webhook-timestamp", String(timestamp))
    .set("webhook-signature", options.signature ?? sign(key, id, timestamp, Buffer.from(body)))
    .send(body);
}

describe("inbound webhook", () => {
  let t: TestApp;

  beforeAll(async () => {
    t = await startTestApp({ WEBHOOK_INBOUND_SECRET: TEST_SECRET });
  });

  afterAll(async () => {
    await t.close();
  });

  it("accepts a signed ping", async () => {
    await deliver(t, "msg_ping", '{"type":"ping"}').expect(204);
  });

  it("verifies the raw bytes, not a re-serialised body", async () => {
    // Same JSON value, different bytes: odd spacing, a unicode escape, key order.
    const body = '{ "data" : {"body":"","title":"caf\\u00e9"},\n\t"type":"note.create" }';
    await deliver(t, "msg_raw", body).expect(204);
    const list = await request(t.server).get("/api/v1/notes").expect(200);
    expect(list.body.items.map((n: { title: string }) => n.title)).toContain("café");

    // A signature over the re-serialised JSON of the same value does not verify.
    const reserialised = JSON.stringify(JSON.parse(body));
    expect(reserialised).not.toBe(body);
    const ts = nowSeconds();
    const res = await deliver(t, "msg_raw2", body, {
      timestamp: ts,
      signature: sign(key, "msg_raw2", ts, Buffer.from(reserialised)),
    }).expect(401);
    expect(res.body.error.code).toBe("invalid_signature");
  });

  it("runs note.create once per webhook-id", async () => {
    const before = (await request(t.server).get("/api/v1/notes")).body.total as number;
    const body = '{"type":"note.create","data":{"title":"from webhook"}}';
    await deliver(t, "msg_dup", body).expect(204);
    await deliver(t, "msg_dup", body).expect(204);
    const after = (await request(t.server).get("/api/v1/notes")).body.total as number;
    expect(after).toBe(before + 1);
  });

  it("processes a retry when the first attempt was refused by the use case", async () => {
    const res = await deliver(t, "msg_retry", '{"type":"note.create","data":{"title":""}}').expect(422);
    expect(res.body.error).toMatchObject({ code: "validation_failed", fields: { title: "is required" } });
    const before = (await request(t.server).get("/api/v1/notes")).body.total as number;
    await deliver(t, "msg_retry", '{"type":"note.create","data":{"title":"fixed"}}').expect(204);
    expect((await request(t.server).get("/api/v1/notes")).body.total).toBe(before + 1);
  });

  it("refuses a wrong signature, a stale timestamp and a future timestamp with 401", async () => {
    const body = '{"type":"ping"}';
    const wrong = await deliver(t, "msg_bad", body, { signature: "v1,AAAA" }).expect(401);
    expect(wrong.body.error.code).toBe("invalid_signature");
    await deliver(t, "msg_old", body, { timestamp: nowSeconds() - 301 }).expect(401);
    await deliver(t, "msg_future", body, { timestamp: nowSeconds() + 301 }).expect(401);
    expect(t.logs().some((l) => l.msg === "webhook rejected" && l.reason === "bad_timestamp")).toBe(true);
  });

  it("accepts any of several space-separated signatures (key rotation)", async () => {
    const body = '{"type":"ping"}';
    const ts = nowSeconds();
    const good = sign(key, "msg_rot", ts, Buffer.from(body));
    await deliver(t, "msg_rot", body, { timestamp: ts, signature: `v1,AAAA ${good}` }).expect(204);
  });

  it("answers 400 when the headers are missing or the signed body is not JSON", async () => {
    const missing = await request(t.server)
      .post("/api/v1/webhooks/inbound")
      .set("Content-Type", "application/json")
      .send('{"type":"ping"}')
      .expect(400);
    expect(missing.body.error.code).toBe("missing_signature");
  });

  it("answers 422 for an unknown event and 413 over 256 KiB", async () => {
    const unknown = await deliver(t, "msg_unknown", '{"type":"other"}').expect(422);
    expect(unknown.body.error.code).toBe("unknown_event");
    const big = await startTestApp({ WEBHOOK_INBOUND_SECRET: TEST_SECRET, HTTP_MAX_BODY_BYTES: "1048576" });
    try {
      const body = JSON.stringify({ type: "ping", pad: "x".repeat(300 * 1024) });
      const res = await deliver(big, "msg_big", body).expect(413);
      expect(res.body.error.code).toBe("body_too_large");
    } finally {
      await big.close();
    }
  });

  it("never logs the signature", () => {
    expect(JSON.stringify(t.logs())).not.toContain("v1,");
  });
});

describe("inbound webhook without a secret", () => {
  it("is not served", async () => {
    const t = await startTestApp();
    try {
      const res = await request(t.server).post("/api/v1/webhooks/inbound").send({ type: "ping" }).expect(404);
      expect(res.body.error.code).toBe("not_found");
    } finally {
      await t.close();
    }
  });
});

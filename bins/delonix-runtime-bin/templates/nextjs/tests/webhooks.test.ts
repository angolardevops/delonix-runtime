import { describe, expect, it } from "vitest";
import * as inbound from "@/app/api/v1/webhooks/inbound/route";
import * as notes from "@/app/api/v1/notes/route";
import { createLogger } from "@/lib/server/logger";
import { Dedup } from "@/lib/server/webhooks/dedup";
import { WebhookDispatcher } from "@/lib/server/webhooks/dispatcher";
import { parseSecret, sign, verify } from "@/lib/server/webhooks/signature";
import { errorOf, installRuntime, request, TEST_SECRET } from "./helpers";

const key = parseSecret(TEST_SECRET);
const NOW = 1_800_000_000;

describe("signature", () => {
  it("matches the Standard Webhooks reference vector", () => {
    // https://www.standardwebhooks.com — the example from the specification.
    const k = parseSecret("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw");
    const sig = sign(k, "msg_p5jXN8AQM9LWM0D4loKWxJek", 1614265330, '{"test": 2432232314}');
    expect(sig).toBe("v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=");
  });

  it("refuses secrets that are not whsec_<base64> of at least 24 bytes", () => {
    expect(() => parseSecret("secret")).toThrow(/whsec_/);
    expect(() => parseSecret("whsec_not base64!")).toThrow(/base64/);
    expect(() => parseSecret("whsec_" + Buffer.alloc(8).toString("base64"))).toThrow(/24 bytes/);
  });

  it("verifies over the raw bytes, inside the 5-minute window, with any of several signatures", () => {
    const body = new TextEncoder().encode('{"type":"ping"}');
    const good = sign(key, "m1", NOW, body);
    const headers = (signature: string, timestamp = String(NOW)) => ({ id: "m1", timestamp, signature });
    expect(verify(key, headers(good), body, NOW)).toBe("ok");
    expect(verify(key, headers(`v1,AAAA ${good}`), body, NOW)).toBe("ok");
    expect(verify(key, headers(good), body, NOW + 299)).toBe("ok");
    expect(verify(key, headers(good), body, NOW + 301)).toBe("bad_timestamp");
    expect(verify(key, headers(good), body, NOW - 301)).toBe("bad_timestamp");
    expect(verify(key, headers(good, "yesterday"), body, NOW)).toBe("bad_timestamp");
    expect(verify(key, headers(good), new TextEncoder().encode('{"type": "ping"}'), NOW)).toBe(
      "bad_signature",
    );
    expect(
      verify(parseSecret("whsec_" + Buffer.alloc(32, 9).toString("base64")), headers(good), body, NOW),
    ).toBe("bad_signature");
    expect(verify(key, { id: "m1", timestamp: String(NOW), signature: null }, body, NOW)).toBe(
      "missing_headers",
    );
  });
});

describe("dedup", () => {
  it("remembers an id for the window, forgets on demand, and evicts the oldest when full", () => {
    const dedup = new Dedup(2);
    expect(dedup.firstSeen("a", 0)).toBe(true);
    expect(dedup.firstSeen("a", 1)).toBe(false);
    dedup.forget("a");
    expect(dedup.firstSeen("a", 2)).toBe(true);
    expect(dedup.firstSeen("b", 3)).toBe(true);
    expect(dedup.firstSeen("c", 4)).toBe(true); // evicts "a"
    expect(dedup.firstSeen("a", 5)).toBe(true);
    expect(dedup.firstSeen("c", 11 * 60 * 1000)).toBe(true); // expired after twice the tolerance
  });
});

describe("inbound webhook", () => {
  const signed = (id: string, body: string, ts = NOW) =>
    request("POST", "/api/v1/webhooks/inbound", body, {
      "webhook-id": id,
      "webhook-timestamp": String(ts),
      "webhook-signature": sign(key, id, ts, body),
    });
  const setup = () =>
    installRuntime({ env: { WEBHOOK_INBOUND_SECRET: TEST_SECRET }, nowMs: () => NOW * 1000 });

  it("is 404 when no secret is configured", async () => {
    installRuntime();
    const res = await inbound.POST(signed("m1", '{"type":"ping"}'), {});
    expect(res.status).toBe(404);
  });

  it("accepts a signed ping and a signed note.create", async () => {
    const t = setup();
    expect((await inbound.POST(signed("m1", '{"type":"ping"}'), {})).status).toBe(204);
    const res = await inbound.POST(
      signed("m2", '{"type":"note.create","data":{"title":"from webhook"}}'),
      {},
    );
    expect(res.status).toBe(204);
    expect((await t.runtime.notes.list(0, 20)).items.map((n) => n.title)).toEqual(["from webhook"]);
  });

  it("acknowledges a repeated id without processing it again", async () => {
    const t = setup();
    const body = '{"type":"note.create","data":{"title":"once"}}';
    expect((await inbound.POST(signed("m1", body), {})).status).toBe(204);
    expect((await inbound.POST(signed("m1", body), {})).status).toBe(204);
    expect((await t.runtime.notes.list(0, 20)).total).toBe(1);
  });

  it("processes a retry when the first attempt failed", async () => {
    const t = setup();
    const bad = await inbound.POST(signed("m1", '{"type":"note.create","data":{"title":""}}'), {});
    expect(bad.status).toBe(422);
    const good = await inbound.POST(signed("m1", '{"type":"note.create","data":{"title":"fixed"}}'), {});
    expect(good.status).toBe(204);
    expect((await t.runtime.notes.list(0, 20)).total).toBe(1);
  });

  it("answers 401 for a wrong signature or a timestamp outside the window, 400 without headers", async () => {
    setup();
    const tampered = signed("m1", '{"type":"ping"}');
    const forged = new Request(tampered.url, {
      method: "POST",
      headers: tampered.headers,
      body: '{"type":"pong"}',
    });
    const res = await inbound.POST(forged, {});
    expect(res.status).toBe(401);
    expect((await errorOf(res)).code).toBe("invalid_signature");
    expect((await inbound.POST(signed("m2", '{"type":"ping"}', NOW - 600), {})).status).toBe(401);
    const bare = await inbound.POST(request("POST", "/api/v1/webhooks/inbound", '{"type":"ping"}'), {});
    expect(bare.status).toBe(400);
    expect((await errorOf(bare)).code).toBe("missing_signature");
  });

  it("answers 422 for an unknown event and 413 over 256 KiB", async () => {
    setup();
    const unknown = await inbound.POST(signed("m1", '{"type":"other"}'), {});
    expect(unknown.status).toBe(422);
    expect((await errorOf(unknown)).code).toBe("unknown_event");
    const huge = await inbound.POST(
      signed("m2", JSON.stringify({ type: "ping", pad: "x".repeat(300_000) })),
      {},
    );
    expect(huge.status).toBe(413);
  });

  it("never logs the signature", async () => {
    const t = setup();
    const req = signed("m1", '{"type":"ping"}');
    const signature = req.headers.get("webhook-signature")!;
    await inbound.POST(req, {});
    expect(JSON.stringify(t.logs)).not.toContain(signature);
  });
});

describe("outbound dispatcher", () => {
  interface Call {
    headers: Headers;
    body: string;
  }
  function dispatcher(
    statuses: Array<number | "network">,
    overrides: Partial<ConstructorParameters<typeof WebhookDispatcher>[0]> = {},
  ) {
    const calls: Call[] = [];
    const sleeps: number[] = [];
    const lines: Array<Record<string, unknown>> = [];
    const d = new WebhookDispatcher({
      url: "https://receiver.test/hook",
      key,
      log: createLogger({
        service: "s",
        version: "v",
        environment: "test",
        level: "debug",
        write: (l) => lines.push(JSON.parse(l)),
      }),
      schedule: (task) => void task(),
      nowMs: () => NOW * 1000,
      sleep: async (ms) => void sleeps.push(ms),
      random: () => 0.5,
      fetch: async (_url, init) => {
        calls.push({ headers: new Headers(init?.headers), body: String(init?.body) });
        const next = statuses[Math.min(calls.length - 1, statuses.length - 1)];
        if (next === "network") throw new TypeError("fetch failed");
        return new Response(null, { status: next });
      },
      ...overrides,
    });
    return { d, calls, sleeps, lines };
  }
  const note = { id: "n1", title: "t", body: "", created_at: "2026-01-01T00:00:00.000Z" };

  it("sends a signed note.created with the note id as webhook-id", async () => {
    const { d, calls } = dispatcher([204]);
    d.noteCreated(note);
    expect(await d.close(1000)).toBe(true);
    expect(calls).toHaveLength(1);
    const call = calls[0]!;
    expect(JSON.parse(call.body)).toEqual({ type: "note.created", data: note });
    expect(call.headers.get("webhook-id")).toBe("n1");
    expect(
      verify(
        key,
        {
          id: call.headers.get("webhook-id"),
          timestamp: call.headers.get("webhook-timestamp"),
          signature: call.headers.get("webhook-signature"),
        },
        new TextEncoder().encode(call.body),
        NOW,
      ),
    ).toBe("ok");
  });

  it("retries network errors, 429 and 5xx with full-jitter backoff, then succeeds", async () => {
    const { d, calls, sleeps, lines } = dispatcher(["network", 503, 429, 200]);
    d.noteCreated(note);
    await d.close(1000);
    expect(calls).toHaveLength(4);
    // random() = 0.5 → half of 500, 1000, 2000 ms.
    expect(sleeps).toEqual([250, 500, 1000]);
    expect(lines.find((l) => l.msg === "webhook delivered")).toMatchObject({ note_id: "n1", attempts: 4 });
  });

  it("gives up after the last attempt", async () => {
    const { d, calls, lines } = dispatcher([500]);
    d.noteCreated(note);
    await d.close(1000);
    expect(calls).toHaveLength(4);
    expect(lines.find((l) => l.msg === "webhook delivery failed")).toMatchObject({
      attempts: 4,
      error: "receiver answered 500",
    });
  });

  it("does not retry a 4xx: the receiver refused this message", async () => {
    const { d, calls } = dispatcher([400]);
    d.noteCreated(note);
    await d.close(1000);
    expect(calls).toHaveLength(1);
  });

  it("drops instead of queueing without bound, and after close", async () => {
    const tasks: Array<() => Promise<void>> = [];
    const { d, calls, lines } = dispatcher([204], {
      maxPending: 1,
      schedule: (task) => void tasks.push(task),
    });
    d.noteCreated(note);
    d.noteCreated({ ...note, id: "n2" });
    expect(lines.find((l) => l.msg === "webhook dispatcher full, event dropped")).toMatchObject({
      note_id: "n2",
    });
    await tasks[0]!();
    expect(await d.close(1000)).toBe(true);
    d.noteCreated({ ...note, id: "n3" });
    expect(lines.some((l) => l.msg === "webhook dispatcher closed, event dropped")).toBe(true);
    expect(calls).toHaveLength(1);
  });

  it("close() reports false when deliveries outlive the deadline", async () => {
    const { d } = dispatcher([204], { schedule: () => undefined });
    d.noteCreated(note);
    expect(await d.close(20)).toBe(false);
  });

  it("is scheduled for after the response by the API", async () => {
    const calls: string[] = [];
    const t = installRuntime({
      env: { WEBHOOK_TARGET_URL: "https://receiver.test/hook", WEBHOOK_TARGET_SECRET: TEST_SECRET },
      fetch: async (_url, init) => {
        calls.push(String(init?.body));
        return new Response(null, { status: 204 });
      },
    });
    const res = await notes.POST(request("POST", "/api/v1/notes", '{"title":"evt"}'), {});
    expect(res.status).toBe(201);
    expect(calls).toHaveLength(0); // the response does not wait for the receiver
    await t.flush();
    expect(await t.runtime.dispatcher!.close(1000)).toBe(true);
    expect(JSON.parse(calls[0]!)).toMatchObject({ type: "note.created", data: { title: "evt" } });
  });
});

import { AppLogger } from "../logging/app-logger";
import type { Note } from "../notes/note";
import { HEADER_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, verify } from "./signature";
import { type DispatcherOptions, WebhookDispatcher } from "./webhook-dispatcher";

const key = Buffer.alloc(32, 3);
const NOW = 1_800_000_000;
const note = (id: string): Note => ({ id, title: "t", body: "", created_at: "2026-01-01T00:00:00.000Z" });

interface Call {
  url: string;
  headers: Record<string, string>;
  body: Buffer;
}

/** A dispatcher whose network, clock, sleep and randomness are the test's. */
function setup(responses: (number | Error | "hang")[], overrides: Partial<DispatcherOptions> = {}) {
  const calls: Call[] = [];
  const sleeps: number[] = [];
  const lines: string[] = [];
  const logger = AppLogger.create({
    level: "debug",
    service: "svc",
    version: "test",
    environment: "test",
    destination: { write: (line: string) => void lines.push(line) },
  });
  const fakeFetch = ((url: string, init: RequestInit) => {
    calls.push({
      url,
      headers: init.headers as Record<string, string>,
      body: Buffer.from(init.body as Uint8Array),
    });
    const next = responses.shift() ?? 204;
    if (next === "hang") {
      // Resolves only when the per-attempt timeout aborts it.
      return new Promise<Response>((_, reject) => {
        init.signal?.addEventListener("abort", () => reject(new Error("timed out")));
      });
    }
    return next instanceof Error
      ? Promise.reject(next)
      : Promise.resolve(new Response(null, { status: next }));
  }) as typeof fetch;
  const dispatcher = new WebhookDispatcher(
    {
      url: "http://receiver.test/hook",
      key,
      timeoutMs: 1000,
      maxAttempts: 4,
      baseBackoffMs: 100,
      queueSize: 3,
      fetch: fakeFetch,
      sleep: (ms) => {
        sleeps.push(ms);
        return Promise.resolve();
      },
      random: () => 0.5,
      nowSeconds: () => NOW,
      ...overrides,
    },
    logger,
  );
  return {
    dispatcher,
    calls,
    sleeps,
    messages: () => lines.map((l) => (JSON.parse(l) as { msg: string }).msg),
  };
}

describe("WebhookDispatcher", () => {
  it("delivers a signed note.created with the note id as webhook-id", async () => {
    const { dispatcher, calls, messages } = setup([204]);
    dispatcher.noteCreated(note("n1"));
    await dispatcher.onApplicationShutdown();
    expect(calls).toHaveLength(1);
    const { headers, body, url } = calls[0];
    expect(url).toBe("http://receiver.test/hook");
    expect(JSON.parse(body.toString())).toEqual({ type: "note.created", data: note("n1") });
    expect(headers[HEADER_ID]).toBe("n1");
    expect(
      verify(key, headers[HEADER_ID], headers[HEADER_TIMESTAMP], headers[HEADER_SIGNATURE], body, NOW),
    ).toBe("ok");
    expect(messages()).toContain("webhook delivered");
  });

  it("retries network errors, 429 and 5xx with full-jitter backoff, same id each time", async () => {
    const { dispatcher, calls, sleeps } = setup([new Error("ECONNREFUSED"), 429, 503, 200]);
    dispatcher.noteCreated(note("n1"));
    await dispatcher.onApplicationShutdown();
    expect(calls.map((c) => c.headers[HEADER_ID])).toEqual(["n1", "n1", "n1", "n1"]);
    // random()=0.5 → half of base * 2^(attempt-1): 100, 200, 400.
    expect(sleeps).toEqual([50, 100, 200]);
  });

  it("keeps the backoff inside [0, base * 2^(attempt-1))", async () => {
    for (const [random, expected] of [
      [0, [0, 0, 0]],
      [0.999, [99, 199, 399]],
    ] as const) {
      const { dispatcher, sleeps } = setup([500, 500, 500, 500], { random: () => random });
      dispatcher.noteCreated(note("n1"));
      await dispatcher.onApplicationShutdown();
      expect(sleeps).toEqual(expected);
    }
  });

  it("gives up after maxAttempts and logs the failure", async () => {
    const { dispatcher, calls, messages } = setup([500, 500, 500, 500, 204]);
    dispatcher.noteCreated(note("n1"));
    await dispatcher.onApplicationShutdown();
    expect(calls).toHaveLength(4);
    expect(messages()).toContain("webhook delivery failed");
  });

  it("does not retry a 4xx: the receiver refused this message", async () => {
    const { dispatcher, calls, sleeps } = setup([400, 204]);
    dispatcher.noteCreated(note("n1"));
    await dispatcher.onApplicationShutdown();
    expect(calls).toHaveLength(1);
    expect(sleeps).toEqual([]);
  });

  it("bounds each attempt with a timeout and treats it as retryable", async () => {
    const { dispatcher, calls } = setup(["hang", 204], { timeoutMs: 20 });
    dispatcher.noteCreated(note("n1"));
    await dispatcher.onApplicationShutdown();
    expect(calls).toHaveLength(2);
  });

  it("drops instead of blocking when the queue is full", async () => {
    const { dispatcher, calls, messages } = setup([]);
    for (const id of ["n1", "n2", "n3", "n4", "n5"]) {
      dispatcher.noteCreated(note(id));
    }
    await dispatcher.onApplicationShutdown();
    // n1 is taken by the worker at once; the queue (3) holds n2..n4; n5 is dropped.
    expect(calls.map((c) => c.headers[HEADER_ID])).toEqual(["n1", "n2", "n3", "n4"]);
    expect(messages()).toContain("webhook queue full, event dropped");
  });

  it("drains what is queued at shutdown and refuses events afterwards", async () => {
    const { dispatcher, calls, messages } = setup([204, 204]);
    dispatcher.noteCreated(note("n1"));
    dispatcher.noteCreated(note("n2"));
    await dispatcher.onApplicationShutdown();
    expect(calls).toHaveLength(2);
    expect(dispatcher.pending).toBe(0);
    dispatcher.noteCreated(note("n3"));
    expect(calls).toHaveLength(2);
    expect(messages()).toContain("webhook dispatcher closed, event dropped");
  });
});

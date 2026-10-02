import { beforeEach, describe, expect, it } from "vitest";
import * as catchAll from "@/app/api/[...path]/route";
import * as live from "@/app/api/v1/health/live/route";
import * as ready from "@/app/api/v1/health/ready/route";
import * as note from "@/app/api/v1/notes/[id]/route";
import * as notes from "@/app/api/v1/notes/route";
import { Lifecycle } from "@/lib/server/lifecycle";
import { errorOf, installRuntime, request, type TestRuntime } from "./helpers";

// The Route Handlers are called directly with a Request, the way Next.js
// calls them; no server is started.
const noContext = {};
const idContext = (id: string) => ({ params: Promise.resolve({ id }) });

let t: TestRuntime;
beforeEach(() => {
  t = installRuntime();
});

async function create(body: unknown): Promise<Response> {
  return notes.POST(request("POST", "/api/v1/notes", JSON.stringify(body)), noContext);
}

describe("notes API", () => {
  it("creates, reads and lists a note", async () => {
    const created = await create({ title: " hello ", body: "world" });
    expect(created.status).toBe(201);
    const body = (await created.json()) as { id: string; title: string; body: string; created_at: string };
    expect(body).toMatchObject({ title: "hello", body: "world" });
    expect(created.headers.get("location")).toBe(`/api/v1/notes/${body.id}`);
    expect(created.headers.get("x-request-id")).toMatch(/^[0-9a-f]{16}$/);

    const got = await note.GET(request("GET", `/api/v1/notes/${body.id}`), idContext(body.id));
    expect(got.status).toBe(200);
    expect(await got.json()).toEqual(body);

    const listed = await notes.GET(request("GET", "/api/v1/notes"), noContext);
    expect(await listed.json()).toEqual({ items: [body], total: 1, offset: 0, limit: 20 });
  });

  it("paginates newest first", async () => {
    for (const title of ["a", "b", "c"]) await create({ title });
    const page = await notes.GET(request("GET", "/api/v1/notes?offset=1&limit=1"), noContext);
    const body = (await page.json()) as { items: Array<{ title: string }>; total: number };
    expect(body.items.map((n) => n.title)).toEqual(["b"]);
    expect(body.total).toBe(3);
  });

  it("answers 422 validation_failed with the fields", async () => {
    const res = await create({ title: "" });
    expect(res.status).toBe(422);
    const error = await errorOf(res);
    expect(error).toMatchObject({ code: "validation_failed", fields: { title: "is required" } });
    expect(error.request_id).toBe(res.headers.get("x-request-id"));
  });

  it("answers 422 for query parameters that are not integers or out of range", async () => {
    for (const query of ["limit=abc", "offset=1.5", "limit=0", "limit=101", "offset=-1"]) {
      const res = await notes.GET(request("GET", `/api/v1/notes?${query}`), noContext);
      expect(res.status, query).toBe(422);
      expect((await errorOf(res)).code).toBe("validation_failed");
    }
  });

  it("answers 400 malformed_json for bad JSON, a non-object, unknown fields and wrong types", async () => {
    for (const body of [
      "{bad",
      "[]",
      '"text"',
      '{"title":"a","extra":1}',
      '{"title":5}',
      '{"title":"a","body":[]}',
    ]) {
      const res = await notes.POST(request("POST", "/api/v1/notes", body), noContext);
      expect(res.status, body).toBe(400);
      expect((await errorOf(res)).code).toBe("malformed_json");
    }
  });

  it("answers 413 body_too_large, by declared length and by actual length", async () => {
    t = installRuntime({ env: { HTTP_MAX_BODY_BYTES: "64" } });
    const big = JSON.stringify({ title: "a", body: "x".repeat(100) });
    const declared = await notes.POST(
      request("POST", "/api/v1/notes", big, { "content-length": "9999" }),
      noContext,
    );
    expect(declared.status).toBe(413);
    expect((await errorOf(declared)).code).toBe("body_too_large");
    // A streamed body carries no Content-Length: the limit is enforced while reading.
    const stream = new Request("http://test.local/api/v1/notes", {
      method: "POST",
      body: new Blob([big]).stream(),
      duplex: "half",
    } as RequestInit);
    expect((await notes.POST(stream, noContext)).status).toBe(413);
  });

  it("answers 404 not_found for a missing note and for an unknown API path", async () => {
    const missing = await note.GET(request("GET", "/api/v1/notes/nope"), idContext("nope"));
    expect(missing.status).toBe(404);
    expect((await errorOf(missing)).code).toBe("not_found");
    const unknown = await catchAll.GET(request("GET", "/api/v1/nope"), noContext);
    expect(unknown.status).toBe(404);
    expect((await errorOf(unknown)).code).toBe("not_found");
  });

  it("answers 500 internal without the cause when the use case throws", async () => {
    t.runtime.notes = {
      create: async () => {
        throw new Error("database password is hunter2");
      },
      get: t.runtime.notes.get,
      list: t.runtime.notes.list,
    };
    const res = await create({ title: "a" });
    expect(res.status).toBe(500);
    const text = await res.text();
    expect(JSON.parse(text).error).toMatchObject({ code: "internal", message: "internal error" });
    expect(text).not.toContain("hunter2");
    expect(t.logs.some((l) => l.msg === "request failed" && l.level === "error")).toBe(true);
  });

  it("keeps a plain client request id and replaces anything else", async () => {
    const kept = await live.GET(
      request("GET", "/api/v1/health/live", undefined, { "x-request-id": "abc-123" }),
      noContext,
    );
    expect(kept.headers.get("x-request-id")).toBe("abc-123");
    const replaced = await live.GET(
      request("GET", "/api/v1/health/live", undefined, { "x-request-id": 'x {"injected":true}' }),
      noContext,
    );
    expect(replaced.headers.get("x-request-id")).toMatch(/^[0-9a-f]{16}$/);
  });

  it("logs one line per request with the request id and the path, never the query string", async () => {
    await notes.GET(request("GET", "/api/v1/notes?limit=5&token=s3cret"), noContext);
    const line = t.logs.find((l) => l.msg === "request");
    expect(line).toMatchObject({ method: "GET", path: "/api/v1/notes", status: 200, level: "info" });
    expect(line?.request_id).toMatch(/^[0-9a-f]{16}$/);
    expect(JSON.stringify(t.logs)).not.toContain("s3cret");
  });
});

describe("health", () => {
  it("liveness is 200", async () => {
    const res = await live.GET(request("GET", "/api/v1/health/live"), noContext);
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ status: "alive" });
    expect(t.logs.find((l) => l.msg === "request")?.level).toBe("debug");
  });

  it("readiness follows the lifecycle: starting → ready → draining", async () => {
    const check = async () => {
      const res = await ready.GET(request("GET", "/api/v1/health/ready"), noContext);
      return [res.status, ((await res.json()) as { status: string }).status];
    };
    t.runtime.lifecycle = new Lifecycle();
    expect(await check()).toEqual([503, "starting"]);
    t.runtime.lifecycle.markReady();
    expect(await check()).toEqual([200, "ready"]);
    t.runtime.lifecycle.markDraining();
    expect(await check()).toEqual([503, "draining"]);
    t.runtime.lifecycle.markReady();
    expect(await check()).toEqual([503, "draining"]);
  });

  it("while draining the API still serves; once closed it refuses work but answers the probes", async () => {
    t.runtime.lifecycle.markDraining();
    expect((await create({ title: "during drain" })).status).toBe(201);
    t.runtime.lifecycle.close();
    const refused = await create({ title: "too late" });
    expect(refused.status).toBe(503);
    expect((await errorOf(refused)).code).toBe("shutting_down");
    expect((await live.GET(request("GET", "/api/v1/health/live"), noContext)).status).toBe(200);
  });

  it("a request counts as in flight until its response has been sent", async () => {
    await create({ title: "a" });
    expect(t.runtime.lifecycle.pending).toBe(1);
    await t.flush();
    expect(t.runtime.lifecycle.pending).toBe(0);
  });
});

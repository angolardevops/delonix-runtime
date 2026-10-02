import request from "supertest";

import { DEFAULT_SERVICE_NAME } from "../src/config/app-config";
import { NoteRepository } from "../src/notes/note.repository";
import { startTestApp, type TestApp } from "./support/app";

describe("notes over HTTP", () => {
  let t: TestApp;

  beforeAll(async () => {
    t = await startTestApp({ HTTP_MAX_BODY_BYTES: "2048" });
  });

  afterAll(async () => {
    await t.close();
  });

  it("creates, reads and lists a note", async () => {
    const created = await request(t.server)
      .post("/api/v1/notes")
      .send({ title: "  hello  ", body: "world" })
      .expect(201);
    expect(created.body).toMatchObject({ title: "hello", body: "world" });
    expect(created.body.id).toEqual(expect.any(String));
    expect(Number.isNaN(Date.parse(created.body.created_at as string))).toBe(false);
    expect(created.headers.location).toBe(`/api/v1/notes/${created.body.id}`);
    expect(created.headers["x-request-id"]).toMatch(/^[0-9a-f]{16}$/);

    const fetched = await request(t.server).get(`/api/v1/notes/${created.body.id}`).expect(200);
    expect(fetched.body).toEqual(created.body);

    const page = await request(t.server).get("/api/v1/notes").expect(200);
    expect(page.body).toMatchObject({ total: 1, offset: 0, limit: 20 });
    expect(page.body.items).toEqual([created.body]);
  });

  it("pages newest first", async () => {
    const fresh = await startTestApp();
    try {
      for (const title of ["a", "b", "c"]) {
        await request(fresh.server).post("/api/v1/notes").send({ title }).expect(201);
      }
      const page = await request(fresh.server).get("/api/v1/notes?offset=1&limit=1").expect(200);
      expect(page.body).toMatchObject({ total: 3, offset: 1, limit: 1 });
      expect(page.body.items.map((n: { title: string }) => n.title)).toEqual(["b"]);
    } finally {
      await fresh.close();
    }
  });

  it("answers 422 with the failing fields", async () => {
    const res = await request(t.server).post("/api/v1/notes").send({ title: "   " }).expect(422);
    expect(res.body).toEqual({
      error: {
        code: "validation_failed",
        message: "invalid input",
        fields: { title: "is required" },
        request_id: res.headers["x-request-id"],
      },
    });
  });

  it("refuses a wrong type and an unknown property", async () => {
    const wrongType = await request(t.server).post("/api/v1/notes").send({ title: 5 }).expect(422);
    expect(wrongType.body.error.fields).toEqual({ title: "must be a string" });
    const unknown = await request(t.server)
      .post("/api/v1/notes")
      .send({ title: "x", admin: true })
      .expect(422);
    expect(unknown.body.error.fields).toEqual({ admin: "is not allowed" });
  });

  it("refuses a title or body over the limit, counting characters", async () => {
    await request(t.server)
      .post("/api/v1/notes")
      .send({ title: "é".repeat(200) })
      .expect(201);
    const res = await request(t.server)
      .post("/api/v1/notes")
      .send({ title: "x".repeat(201) })
      .expect(422);
    expect(res.body.error.fields).toEqual({ title: "must be at most 200 characters" });
  });

  it("answers 400 malformed_json for a body that is not the expected JSON", async () => {
    for (const body of ['{"title":', "[1]", '"text"']) {
      const res = await request(t.server)
        .post("/api/v1/notes")
        .set("Content-Type", "application/json")
        .send(body)
        .expect(400);
      expect(res.body.error.code).toBe("malformed_json");
      expect(res.body.error.request_id).toBe(res.headers["x-request-id"]);
    }
  });

  it("answers 413 body_too_large over HTTP_MAX_BODY_BYTES", async () => {
    const res = await request(t.server)
      .post("/api/v1/notes")
      .send({ title: "x", body: "y".repeat(4096) })
      .expect(413);
    expect(res.body.error.code).toBe("body_too_large");
  });

  it("answers 404 not_found for a missing note and for an unknown route", async () => {
    const note = await request(t.server).get("/api/v1/notes/does-not-exist").expect(404);
    expect(note.body.error).toMatchObject({ code: "not_found", message: "note not found" });
    const route = await request(t.server).get("/nope").expect(404);
    expect(route.body.error).toMatchObject({ code: "not_found", message: "no such route" });
  });

  it("validates offset and limit", async () => {
    const notInt = await request(t.server).get("/api/v1/notes?limit=abc").expect(422);
    expect(notInt.body.error.fields).toEqual({ limit: "must be an integer" });
    const range = await request(t.server).get("/api/v1/notes?limit=101&offset=-1").expect(422);
    expect(range.body.error.fields).toEqual({
      limit: "must be between 1 and 100",
      offset: "must not be negative",
    });
  });

  it("keeps a plain client request id and replaces anything else", async () => {
    const kept = await request(t.server).get("/api/v1/health/live").set("X-Request-Id", "abc-123");
    expect(kept.headers["x-request-id"]).toBe("abc-123");
    const replaced = await request(t.server).get("/api/v1/health/live").set("X-Request-Id", 'bad id"{}');
    expect(replaced.headers["x-request-id"]).toMatch(/^[0-9a-f]{16}$/);
  });

  it("answers 500 internal without a stack when the use case throws", async () => {
    const broken = await startTestApp();
    try {
      jest
        .spyOn(broken.app.get(NoteRepository), "get")
        .mockRejectedValue(new Error("db password=hunter2 unreachable"));
      const res = await request(broken.server).get("/api/v1/notes/x").expect(500);
      expect(res.body).toEqual({
        error: { code: "internal", message: "internal error", request_id: res.headers["x-request-id"] },
      });
      expect(JSON.stringify(res.body)).not.toContain("hunter2");
      expect(broken.logs().some((l) => l.msg === "request failed" && l.level === "error")).toBe(true);
    } finally {
      await broken.close();
    }
  });

  it("logs one line per request with the path only, and probes at debug", async () => {
    await request(t.server).get("/api/v1/notes?limit=7").expect(200);
    await request(t.server).get("/api/v1/health/live").expect(200);
    const lines = t.logs().filter((l) => l.msg === "request");
    const list = lines.findLast((l) => l.path === "/api/v1/notes");
    expect(list).toMatchObject({
      level: "info",
      method: "GET",
      status: 200,
      service: DEFAULT_SERVICE_NAME,
      environment: "test",
    });
    // The path, never the query string.
    expect(JSON.stringify(list)).not.toMatch(/limit|\?/);
    expect(lines.findLast((l) => l.path === "/api/v1/health/live")).toMatchObject({ level: "debug" });
  });
});

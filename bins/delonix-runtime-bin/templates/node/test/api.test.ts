import assert from "node:assert/strict";
import { test } from "node:test";
import { testApp } from "./helpers.js";

test("a note goes from POST to GET to the listing", async () => {
  const { app } = await testApp();
  const created = await app.inject({
    method: "POST",
    url: "/api/v1/notes",
    payload: { title: "first", body: "hello" },
  });
  assert.equal(created.statusCode, 201);
  const note = created.json();
  assert.equal(created.headers.location, `/api/v1/notes/${note.id}`);
  const got = await app.inject({ url: `/api/v1/notes/${note.id}` });
  assert.equal(got.json().title, "first");
  const page = await app.inject({ url: "/api/v1/notes?limit=1" });
  assert.equal(page.json().total, 1);
  await app.close();
});

test("every error has the one shape, the right status and a request id", async () => {
  const { app } = await testApp();
  const cases: [string, string, string | undefined, number, string][] = [
    ["POST", "/api/v1/notes", '{"title":""}', 422, "validation_failed"],
    ["POST", "/api/v1/notes", '{"title":"x","extra":1}', 422, "validation_failed"],
    ["POST", "/api/v1/notes", "{not json", 400, "malformed_json"],
    ["POST", "/api/v1/notes", `{"title":"${"x".repeat(2000)}"}`, 413, "body_too_large"],
    ["GET", "/api/v1/notes?limit=500", undefined, 422, "validation_failed"],
    ["GET", "/api/v1/notes?offset=abc", undefined, 422, "validation_failed"],
    ["GET", "/api/v1/notes/missing", undefined, 404, "not_found"],
    ["GET", "/nowhere", undefined, 404, "not_found"],
  ];
  for (const [method, url, payload, status, code] of cases) {
    const res = await app.inject({
      method: method as "GET" | "POST",
      url,
      payload,
      headers: payload ? { "content-type": "application/json" } : {},
    });
    const body = res.json();
    assert.equal(res.statusCode, status, `${method} ${url}: ${res.body}`);
    assert.equal(body.error.code, code, `${method} ${url}`);
    assert.ok(body.error.request_id, `${method} ${url}: no request id`);
  }
  await app.close();
});

test("readiness follows the lifecycle and liveness does not", async () => {
  const { app, readiness } = await testApp();
  assert.equal((await app.inject({ url: "/api/v1/health/ready" })).statusCode, 200);
  readiness.setDraining();
  readiness.setReady(); // draining is final
  const draining = await app.inject({ url: "/api/v1/health/ready" });
  assert.equal(draining.statusCode, 503);
  assert.equal(draining.json().status, "draining");
  assert.equal((await app.inject({ url: "/api/v1/health/live" })).statusCode, 200);
  await app.close();
});

test("a plain client request id is kept, anything else is replaced", async () => {
  const { app } = await testApp();
  const kept = await app.inject({
    url: "/api/v1/health/live",
    headers: { "x-request-id": "abc-123" },
  });
  assert.equal(kept.headers["x-request-id"], "abc-123");
  const replaced = await app.inject({
    url: "/api/v1/health/live",
    headers: { "x-request-id": 'x" injected="1' },
  });
  assert.match(String(replaced.headers["x-request-id"]), /^[0-9a-f]{16}$/);
  await app.close();
});

test("the access log has the path without the query string", async () => {
  const { app, logs } = await testApp();
  await app.inject({ url: "/api/v1/notes?limit=1&token=s3cr3t" });
  const line = logs().find((l) => l.msg === "request");
  assert.equal(line?.path, "/api/v1/notes");
  assert.ok(!JSON.stringify(logs()).includes("s3cr3t"), "the query string reached the log");
  await app.close();
});

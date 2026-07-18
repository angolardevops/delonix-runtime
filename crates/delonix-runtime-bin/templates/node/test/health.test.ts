import assert from "node:assert/strict";
import { test } from "node:test";
import { buildApp } from "../src/app.js";

test("GET /api/v1/health/live", async () => {
  const app = buildApp();
  const res = await app.inject({ method: "GET", url: "/api/v1/health/live" });
  assert.equal(res.statusCode, 200);
  assert.deepEqual(res.json(), { status: "alive" });
  await app.close();
});

test("GET /api/v1/health/ready", async () => {
  const app = buildApp();
  const res = await app.inject({ method: "GET", url: "/api/v1/health/ready" });
  assert.equal(res.statusCode, 200);
  assert.equal(res.json().status, "ready");
  await app.close();
});

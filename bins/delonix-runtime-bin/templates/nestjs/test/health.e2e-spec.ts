import request from "supertest";

import { ReadinessService } from "../src/health/readiness.service";
import { startTestApp, type TestApp } from "./support/app";

describe("health", () => {
  let t: TestApp;

  beforeAll(async () => {
    t = await startTestApp();
  });

  afterAll(async () => {
    await t.close();
  });

  it("is alive", async () => {
    const res = await request(t.server).get("/api/v1/health/live").expect(200);
    expect(res.body).toEqual({ status: "alive" });
  });

  it("follows the lifecycle: ready, failing dependency, draining", async () => {
    const readiness = t.app.get(ReadinessService);
    expect((await request(t.server).get("/api/v1/health/ready").expect(200)).body).toEqual({
      status: "ready",
    });

    let healthy = false;
    readiness.addCheck("database", () =>
      healthy ? Promise.resolve() : Promise.reject(new Error("refused")),
    );
    const failing = await request(t.server).get("/api/v1/health/ready").expect(503);
    // The dependency is named; the reason stays in the logs.
    expect(failing.body).toEqual({ status: "unavailable", dependency: "database" });
    healthy = true;
    await request(t.server).get("/api/v1/health/ready").expect(200);

    readiness.markDraining();
    expect((await request(t.server).get("/api/v1/health/ready").expect(503)).body).toEqual({
      status: "draining",
    });
    // Draining is one-way, and liveness is unaffected.
    readiness.markReady();
    await request(t.server).get("/api/v1/health/ready").expect(503);
    await request(t.server).get("/api/v1/health/live").expect(200);
  });
});

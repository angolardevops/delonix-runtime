import { ReadinessService } from "./readiness.service";

describe("ReadinessService", () => {
  it("starts as starting, becomes ready, and draining is one-way", async () => {
    const readiness = new ReadinessService();
    expect(await readiness.evaluate()).toEqual({ state: "starting" });
    readiness.markReady();
    expect(await readiness.evaluate()).toEqual({ state: "ready" });
    readiness.markDraining();
    readiness.markReady();
    expect(readiness.state).toBe("draining");
  });

  it("names the first failing dependency only while ready", async () => {
    const readiness = new ReadinessService();
    readiness.addCheck("cache", () => Promise.reject(new Error("timeout")));
    expect(await readiness.evaluate()).toEqual({ state: "starting" });
    readiness.markReady();
    expect(await readiness.evaluate()).toEqual({ state: "ready", failing: "cache" });
  });
});

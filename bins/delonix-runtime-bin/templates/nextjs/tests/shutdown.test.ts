import { describe, expect, it } from "vitest";
import * as notes from "@/app/api/v1/notes/route";
import { shutdown } from "@/lib/server/shutdown";
import { installRuntime, request, TEST_SECRET } from "./helpers";

describe("shutdown", () => {
  it("drains readiness first, waits for the request in flight, then exits 0", async () => {
    const t = installRuntime({ env: { DRAIN_DELAY: "30ms" } });
    await notes.POST(request("POST", "/api/v1/notes", '{"title":"in flight"}'), {});
    expect(t.runtime.lifecycle.pending).toBe(1);

    const exits: number[] = [];
    const stopping = shutdown(t.runtime, (code) => exits.push(code));
    expect(t.runtime.lifecycle.readiness).toBe("draining");
    // Still serving during the drain delay.
    expect((await notes.GET(request("GET", "/api/v1/notes"), {})).status).toBe(200);
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(t.runtime.lifecycle.isClosing).toBe(true);
    expect((await notes.GET(request("GET", "/api/v1/notes"), {})).status).toBe(503);
    expect(exits).toEqual([]); // the first request has not finished sending

    await t.flush();
    await stopping;
    expect(exits).toEqual([0]);
    expect(t.logs.map((l) => l.msg)).toContain("stopped");
  });

  it("drains pending webhooks before exiting", async () => {
    const delivered: string[] = [];
    const t = installRuntime({
      env: { WEBHOOK_TARGET_URL: "https://receiver.test/hook", WEBHOOK_TARGET_SECRET: TEST_SECRET },
      fetch: async (_url, init) => {
        await new Promise((resolve) => setTimeout(resolve, 30));
        delivered.push(String(init?.body));
        return new Response(null, { status: 204 });
      },
      immediate: true,
    });
    await notes.POST(request("POST", "/api/v1/notes", '{"title":"evt"}'), {});
    const exits: number[] = [];
    await shutdown(t.runtime, (code) => exits.push(code));
    expect(delivered).toHaveLength(1);
    expect(exits).toEqual([0]);
  });

  it("exits 1 when the deadline cuts the shutdown short", async () => {
    const t = installRuntime({ env: { SHUTDOWN_TIMEOUT: "40ms" } });
    await notes.POST(request("POST", "/api/v1/notes", '{"title":"never finishes sending"}'), {});
    const exits: number[] = [];
    await shutdown(t.runtime, (code) => exits.push(code));
    expect(exits).toEqual([1]);
    expect(t.logs.some((l) => l.msg === "requests still in flight at the deadline")).toBe(true);
  });
});

import { describe, expect, it } from "vitest";
import { loadConfig } from "@/lib/server/config";
import { createLogger } from "@/lib/server/logger";
import { buildRuntime, type Runtime } from "@/lib/server/runtime";
import { shutdown } from "@/lib/server/shutdown";
import { MemoryCache, RedisCache } from "@/lib/server/cache/cache";

// Proves the cache and the RabbitMQ publisher are reached FROM the runtime —
// not merely defined in their modules — and that both are released in the
// ordered shutdown. No backend is touched: Redis connects lazily (never here)
// and the publisher's queue stays empty, so nothing is published.
function build(env: Record<string, string> = {}): Runtime {
  const config = loadConfig({ APP_ENV: "test", ...env });
  const log = createLogger({
    service: "s",
    version: "v",
    environment: "test",
    level: "error",
    write: () => {},
  });
  return buildRuntime(config, {
    log,
    telemetry: { shutdown: async () => true },
    afterResponse: () => {},
  });
}

describe("runtime wiring", () => {
  it("defaults to the in-process cache and wires no broker publisher", () => {
    const rt = build();
    expect(rt.cache).toBeInstanceOf(MemoryCache);
    expect(rt.messaging).toBeUndefined();
  });

  it("reaches the cache from the notes read path (create prewarms the runtime's cache)", async () => {
    const rt = build();
    const created = await rt.notes.create({ title: "x" });
    expect(await rt.cache.get("note:v1:" + created.id)).toBeDefined();
    expect(await rt.notes.get(created.id)).toEqual(created);
  });

  it("uses Redis and a RabbitMQ publisher when configured, and closes both on shutdown", async () => {
    const rt = build({ REDIS_URL: "redis://redis:6379/0", RABBITMQ_URL: "amqp://app:pw@rabbitmq:5672/" });
    expect(rt.cache).toBeInstanceOf(RedisCache);
    expect(rt.messaging).toBeDefined();
    const exits: number[] = [];
    await shutdown(rt, (code) => exits.push(code));
    expect(exits).toEqual([0]); // drained cleanly: empty queue, lazy Redis never opened
    expect(rt.messaging!.counts.published).toBe(0);
  });
});

import { AppConfig, loadConfig } from "../config/app-config";
import { AppLogger } from "../logging/app-logger";
import { InMemoryNoteRepository } from "../notes/in-memory-note.repository";
import { type Note, NoteNotFoundError } from "../notes/note";
import { NoopNotePublisher } from "../notes/note.publisher";
import type { Cache } from "./cache";
import { CachedNotesService } from "./cached-notes.service";
import { InMemoryCache } from "./in-memory-cache";
import { RedisCache, type RedisClient } from "./redis-cache";

const silentLogger = (): AppLogger =>
  AppLogger.create({
    level: "error",
    service: "test",
    version: "test",
    environment: "test",
    destination: { write: () => undefined },
  });

// A clock the test advances, in milliseconds.
class Clock {
  now = 0;
  tick = (): number => this.now;
}

describe("InMemoryCache", () => {
  it("round-trips a value and misses an absent key", async () => {
    const cache = new InMemoryCache();
    expect(await cache.get("absent")).toBeNull();
    await cache.set("k", Buffer.from("v"), 10_000);
    expect((await cache.get("k"))?.toString()).toBe("v");
  });

  it("expires an entry after its TTL", async () => {
    const clock = new Clock();
    const cache = new InMemoryCache(4096, clock.tick);
    await cache.set("k", Buffer.from("v"), 10_000);
    clock.now = 9_999;
    expect(await cache.get("k")).not.toBeNull();
    clock.now = 10_000;
    expect(await cache.get("k")).toBeNull();
  });

  it("evicts the oldest when full", async () => {
    const cache = new InMemoryCache(2);
    await cache.set("a", Buffer.from("1"), 100_000);
    await cache.set("b", Buffer.from("2"), 100_000);
    await cache.set("c", Buffer.from("3"), 100_000); // evicts "a"
    expect(await cache.get("a")).toBeNull();
    expect((await cache.get("b"))?.toString()).toBe("2");
    expect((await cache.get("c"))?.toString()).toBe("3");
  });

  it("stores nothing for a non-positive TTL", async () => {
    const cache = new InMemoryCache();
    await cache.set("k", Buffer.from("v"), 0);
    expect(await cache.get("k")).toBeNull();
  });
});

describe("RedisCache", () => {
  const raising: RedisClient = {
    getBuffer: () => Promise.reject(new Error("down")),
    set: () => Promise.reject(new Error("down")),
    ping: () => Promise.reject(new Error("down")),
    quit: () => Promise.reject(new Error("down")),
  };

  it("swallows every backend error", async () => {
    const cache = new RedisCache(raising, silentLogger());
    expect(await cache.get("k")).toBeNull();
    await expect(cache.set("k", Buffer.from("v"), 10_000)).resolves.toBeUndefined();
    expect(await cache.healthy()).toBe(false);
    await expect(cache.close()).resolves.toBeUndefined();
  });

  it("reads through a working client and reports healthy", async () => {
    const store = new Map<string, Buffer>();
    const client: RedisClient = {
      getBuffer: (key) => Promise.resolve(store.get(key) ?? null),
      set: (key, value) => {
        store.set(key, value);
        return Promise.resolve("OK");
      },
      ping: () => Promise.resolve("PONG"),
      quit: () => Promise.resolve("OK"),
    };
    const cache = new RedisCache(client, silentLogger());
    await cache.set("k", Buffer.from("v"), 10_000);
    expect((await cache.get("k"))?.toString()).toBe("v");
    expect(await cache.healthy()).toBe(true);
  });
});

describe("CachedNotesService", () => {
  const config: AppConfig = loadConfig({});

  function build(cache: Cache): { service: CachedNotesService; repository: InMemoryNoteRepository } {
    const repository = new InMemoryNoteRepository();
    const service = new CachedNotesService(repository, new NoopNotePublisher(), cache, config, silentLogger());
    return { service, repository };
  }

  it("prewarms on create so the first get is a cache hit", async () => {
    const { service, repository } = build(new InMemoryCache());
    const note = await service.create({ title: "hello", body: "world" });
    const spy = jest.spyOn(repository, "get");
    expect(await service.get(note.id)).toEqual(note);
    expect(spy).not.toHaveBeenCalled();
  });

  it("reads the store once, then serves the second get from the cache", async () => {
    const { service, repository } = build(new InMemoryCache());
    // Written straight to the store, so the cache is cold.
    const stored: Note = { id: "abc", title: "x", body: "", created_at: "2026-01-01T00:00:00.000Z" };
    await repository.save(stored);
    const spy = jest.spyOn(repository, "get");
    expect(await service.get("abc")).toEqual(stored);
    expect(await service.get("abc")).toEqual(stored);
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("raises NoteNotFoundError through the cache and never caches a miss", async () => {
    const { service, repository } = build(new InMemoryCache());
    const spy = jest.spyOn(repository, "get");
    await expect(service.get("nope")).rejects.toBeInstanceOf(NoteNotFoundError);
    await expect(service.get("nope")).rejects.toBeInstanceOf(NoteNotFoundError);
    expect(spy).toHaveBeenCalledTimes(2);
  });

  it("ignores an undecodable cache entry and falls through to the store", async () => {
    const cache = new InMemoryCache();
    const { service, repository } = build(cache);
    const stored: Note = { id: "abc", title: "x", body: "", created_at: "2026-01-01T00:00:00.000Z" };
    await repository.save(stored);
    await cache.set("note:v1:abc", Buffer.from("not json"), 100_000);
    const spy = jest.spyOn(repository, "get");
    expect(await service.get("abc")).toEqual(stored);
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("serves from the store when the cache backend is dead", async () => {
    const dead: Cache = {
      get: () => Promise.resolve(null),
      set: () => Promise.resolve(),
      healthy: () => Promise.resolve(false),
      close: () => Promise.resolve(),
    };
    const { service, repository } = build(dead);
    const stored: Note = { id: "abc", title: "x", body: "", created_at: "2026-01-01T00:00:00.000Z" };
    await repository.save(stored);
    const spy = jest.spyOn(repository, "get");
    expect(await service.get("abc")).toEqual(stored);
    expect(await service.get("abc")).toEqual(stored);
    expect(spy).toHaveBeenCalledTimes(2); // every read falls through; nothing is cached
  });
});

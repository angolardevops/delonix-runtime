import { describe, expect, it } from "vitest";
import { MemoryCache, RedisCache, type Cache, type RedisClient } from "@/lib/server/cache/cache";
import { CachedNotes } from "@/lib/server/cache/cached-notes";
import {
  isNoteNotFound,
  NotesService,
  type CreateNoteInput,
  type Note,
  type NotePage,
  type NotesApi,
} from "@/lib/notes/notes";
import { MemoryNoteStore } from "@/lib/server/memory-note-store";

// A clock the tests drive, in milliseconds.
function clock(): { now: number; read: () => number } {
  const c = { now: 0, read: () => c.now };
  return c;
}

// ---- MemoryCache ----

describe("MemoryCache", () => {
  it("round-trips a value and misses on an absent key", async () => {
    const cache = new MemoryCache();
    expect(await cache.get("absent")).toBeUndefined();
    await cache.set("k", "v", 10);
    expect(await cache.get("k")).toBe("v");
  });

  it("expires an entry after its ttl", async () => {
    const c = clock();
    const cache = new MemoryCache(4096, c.read);
    await cache.set("k", "v", 10);
    c.now = 9_900;
    expect(await cache.get("k")).toBe("v");
    c.now = 10_000;
    expect(await cache.get("k")).toBeUndefined();
  });

  it("evicts the oldest when full", async () => {
    const cache = new MemoryCache(2);
    await cache.set("a", "1", 100);
    await cache.set("b", "2", 100);
    await cache.set("c", "3", 100); // evicts "a"
    expect(await cache.get("a")).toBeUndefined();
    expect(await cache.get("b")).toBe("2");
    expect(await cache.get("c")).toBe("3");
  });

  it("rejects a non-positive ttl", async () => {
    const cache = new MemoryCache();
    await cache.set("k", "v", 0);
    expect(await cache.get("k")).toBeUndefined();
  });
});

// ---- CachedNotes ----

/** A NotesApi that counts how often the store is actually read. */
class CountingNotes implements NotesApi {
  private readonly inner = new NotesService(new MemoryNoteStore());
  gets = 0;

  create(input: CreateNoteInput): Promise<Note> {
    return this.inner.create(input);
  }

  get(id: string): Promise<Note> {
    this.gets++;
    return this.inner.get(id);
  }

  list(offset: number, limit: number): Promise<NotePage> {
    return this.inner.list(offset, limit);
  }
}

/** Models the Cache contract under a backend outage: never throws, never stores. */
class BrokenCache implements Cache {
  async get(): Promise<string | undefined> {
    return undefined;
  }
  async set(): Promise<void> {}
  async healthy(): Promise<boolean> {
    return false;
  }
  async close(): Promise<void> {}
}

describe("CachedNotes", () => {
  it("prewarms on create so the first get is a hit", async () => {
    const inner = new CountingNotes();
    const notes = new CachedNotes(inner, new MemoryCache(), 100);
    const note = await notes.create({ title: "hello" });
    expect(await notes.get(note.id)).toEqual(note);
    expect(inner.gets).toBe(0); // served entirely from the cache
  });

  it("serves the second get from the cache", async () => {
    const inner = new CountingNotes();
    const notes = new CachedNotes(inner, new MemoryCache(), 100);
    const created = await inner.create({ title: "x" }); // created on the inner: cache is cold
    expect(await notes.get(created.id)).toEqual(created);
    expect(await notes.get(created.id)).toEqual(created);
    expect(inner.gets).toBe(1); // read once; the second get was a hit
  });

  it("still throws not_found for an unknown id, through the cache", async () => {
    const notes = new CachedNotes(new CountingNotes(), new MemoryCache(), 100);
    const err = await notes.get("nope").catch((e: unknown) => e);
    expect(isNoteNotFound(err)).toBe(true);
  });

  it("ignores a corrupt cache entry and reads the store", async () => {
    const inner = new CountingNotes();
    const cache = new MemoryCache();
    const created = await inner.create({ title: "x" });
    await cache.set("note:v1:" + created.id, "not json", 100);
    const notes = new CachedNotes(inner, cache, 100);
    expect(await notes.get(created.id)).toEqual(created); // fell through to the store
    expect(inner.gets).toBe(1);
  });

  it("serves from the store when the cache backend is dead", async () => {
    const inner = new CountingNotes();
    const created = await inner.create({ title: "x" });
    const notes = new CachedNotes(inner, new BrokenCache(), 100);
    expect(await notes.get(created.id)).toEqual(created);
    expect(await notes.get(created.id)).toEqual(created);
    expect(inner.gets).toBe(2); // every read falls through; nothing is ever cached
  });
});

// ---- RedisCache ----

/** A redis client whose every call rejects, to prove RedisCache swallows it. */
const raisingRedis: RedisClient = {
  get: () => Promise.reject(new Error("down")),
  set: () => Promise.reject(new Error("down")),
  ping: () => Promise.reject(new Error("down")),
  quit: () => Promise.reject(new Error("down")),
};

describe("RedisCache", () => {
  it("swallows every backend error", async () => {
    const cache = new RedisCache(raisingRedis);
    expect(await cache.get("k")).toBeUndefined();
    await cache.set("k", "v", 10); // must not throw
    expect(await cache.healthy()).toBe(false);
    await cache.close(); // must not throw
    expect(cache.isDegraded).toBe(true);
  });

  it("round-trips through a working client and clears degraded", async () => {
    const store = new Map<string, string>();
    const client: RedisClient = {
      get: (k) => Promise.resolve(store.get(k) ?? null),
      set: (k, v) => {
        store.set(k, v);
        return Promise.resolve("OK");
      },
      ping: () => Promise.resolve("PONG"),
      quit: () => Promise.resolve("OK"),
    };
    const cache = new RedisCache(client);
    await cache.set("k", "v", 10);
    expect(await cache.get("k")).toBe("v");
    expect(await cache.healthy()).toBe(true);
    expect(cache.isDegraded).toBe(false);
  });
});

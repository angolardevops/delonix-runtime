/**
 * The cache: the in-process backend, the cache-aside decorator, and that a
 * backend failure is a slow path and never a failed request.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { CachedNotes, KEY_PREFIX } from "../src/cache/cached-notes.js";
import { InMemoryCache } from "../src/cache/memory.js";
import type { Cache } from "../src/cache/ports.js";
import { RedisCache, type RedisBackend } from "../src/cache/redis.js";
import { MemoryNoteStore } from "../src/notes/memory-store.js";
import {
  DEFAULT_PAGE_SIZE,
  NotFoundError,
  NotesService,
  type Note,
  type NotePage,
  type NotesUseCases,
} from "../src/notes/notes.js";

class Clock {
  now = 0;
  tick = (): number => this.now;
}

test("in-memory cache round-trips and misses", async () => {
  const cache = new InMemoryCache();
  assert.equal(await cache.get("absent"), null);
  await cache.set("k", Buffer.from("v"), 10);
  assert.deepEqual(await cache.get("k"), Buffer.from("v"));
});

test("in-memory entry expires after its ttl", async () => {
  const clock = new Clock();
  const cache = new InMemoryCache({ clock: clock.tick });
  await cache.set("k", Buffer.from("v"), 10);
  clock.now = 9.9;
  assert.deepEqual(await cache.get("k"), Buffer.from("v"));
  clock.now = 10;
  assert.equal(await cache.get("k"), null);
});

test("in-memory evicts the oldest when full", async () => {
  const cache = new InMemoryCache({ maxEntries: 2 });
  await cache.set("a", Buffer.from("1"), 100);
  await cache.set("b", Buffer.from("2"), 100);
  await cache.set("c", Buffer.from("3"), 100); // evicts "a"
  assert.equal(await cache.get("a"), null);
  assert.deepEqual(await cache.get("b"), Buffer.from("2"));
  assert.deepEqual(await cache.get("c"), Buffer.from("3"));
});

test("in-memory rejects a non-positive ttl", async () => {
  const cache = new InMemoryCache();
  await cache.set("k", Buffer.from("v"), 0);
  assert.equal(await cache.get("k"), null);
});

/** A use-case object that counts how often the store is actually read. */
class CountingNotes implements NotesUseCases {
  private readonly inner = new NotesService(new MemoryNoteStore());
  gets = 0;

  create(input: { title: string; body?: string }): Promise<Note> {
    return this.inner.create(input);
  }
  get(id: string): Promise<Note> {
    this.gets += 1;
    return this.inner.get(id);
  }
  list(offset = 0, limit = DEFAULT_PAGE_SIZE): Promise<NotePage> {
    return this.inner.list(offset, limit);
  }
}

test("create prewarms so the first get is a hit", async () => {
  const inner = new CountingNotes();
  const notes = new CachedNotes(inner, new InMemoryCache(), 100);
  const note = await notes.create({ title: "hello" });
  assert.deepEqual(await notes.get(note.id), note);
  assert.equal(inner.gets, 0); // served entirely from the cache
});

test("the second get is served from the cache", async () => {
  const inner = new CountingNotes();
  const notes = new CachedNotes(inner, new InMemoryCache(), 100);
  const created = await inner.create({ title: "x" }); // on the inner only: cache cold
  assert.deepEqual(await notes.get(created.id), created);
  assert.deepEqual(await notes.get(created.id), created);
  assert.equal(inner.gets, 1); // store read once; the second get was a hit
});

test("an unknown note still raises through the cache", async () => {
  const notes = new CachedNotes(new CountingNotes(), new InMemoryCache(), 100);
  await assert.rejects(notes.get("nope"), NotFoundError);
});

test("a corrupt cache entry is ignored", async () => {
  const inner = new CountingNotes();
  const cache = new InMemoryCache();
  const created = await inner.create({ title: "x" });
  await cache.set(KEY_PREFIX + created.id, Buffer.from("not json"), 100);
  const notes = new CachedNotes(inner, cache, 100);
  assert.deepEqual(await notes.get(created.id), created); // fell through to the store
  assert.equal(inner.gets, 1);
});

/** Models the port contract under a backend outage: never throws. */
class BrokenCache implements Cache {
  async get(): Promise<Buffer | null> {
    return null;
  }
  async set(): Promise<void> {}
  async healthy(): Promise<boolean> {
    return false;
  }
  async close(): Promise<void> {}
}

test("a dead cache backend still serves from the store", async () => {
  const inner = new CountingNotes();
  const created = await inner.create({ title: "x" });
  const notes = new CachedNotes(inner, new BrokenCache(), 100);
  assert.deepEqual(await notes.get(created.id), created);
  assert.deepEqual(await notes.get(created.id), created);
  assert.equal(inner.gets, 2); // every read falls through; nothing is cached
});

/** A backend whose every call rejects, to prove RedisCache swallows it. */
class RaisingBackend implements RedisBackend {
  async get(): Promise<Buffer | null> {
    throw new Error("down");
  }
  async set(): Promise<void> {
    throw new Error("down");
  }
  async ping(): Promise<boolean> {
    throw new Error("down");
  }
  async close(): Promise<void> {
    throw new Error("down");
  }
}

test("RedisCache swallows backend errors", async () => {
  const cache = new RedisCache(new RaisingBackend());
  assert.equal(await cache.get("k"), null);
  await cache.set("k", Buffer.from("v"), 10); // must not throw
  assert.equal(await cache.healthy(), false);
  await cache.close(); // must not throw
});

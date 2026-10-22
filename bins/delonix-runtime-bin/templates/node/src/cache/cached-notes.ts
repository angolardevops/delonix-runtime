/**
 * Cache-aside over the notes use cases: `get(id)` reads through the cache.
 *
 * Wraps the use cases from the OUTSIDE, exactly like `telemetry/traced-notes`,
 * so the notes package never imports the cache (test/architecture.test.ts).
 * A note is immutable once created, so a cached note is never stale — only the
 * TTL expires it. Only `get` is cached: `list` changes whenever a note is
 * created, and `create` returns a note the caller already holds (it is written
 * to the cache so the first read is a hit). The cache is a slow path, never a
 * failure path — `Cache` swallows backend errors, and a decode error here
 * falls through to the store too.
 */
import type { Logger } from "pino";
import type { Note, NotePage, NotesUseCases } from "../notes/notes.js";
import type { Cache } from "./ports.js";

export const KEY_PREFIX = "note:v1:";

function encode(note: Note): Buffer {
  return Buffer.from(JSON.stringify(note));
}

function decode(raw: Buffer): Note {
  const data = JSON.parse(raw.toString("utf8")) as Record<string, unknown>;
  if (
    typeof data.id !== "string" ||
    typeof data.title !== "string" ||
    typeof data.body !== "string" ||
    typeof data.created_at !== "string"
  ) {
    throw new Error("undecodable cache entry");
  }
  return { id: data.id, title: data.title, body: data.body, created_at: data.created_at };
}

export class CachedNotes implements NotesUseCases {
  constructor(
    private readonly inner: NotesUseCases,
    private readonly cache: Cache,
    private readonly ttlSeconds: number,
    private readonly log?: Logger,
  ) {}

  async create(input: { title: string; body?: string }): Promise<Note> {
    const note = await this.inner.create(input);
    await this.cache.set(KEY_PREFIX + note.id, encode(note), this.ttlSeconds);
    return note;
  }

  async get(id: string): Promise<Note> {
    const key = KEY_PREFIX + id;
    const raw = await this.cache.get(key);
    if (raw !== null) {
      try {
        return decode(raw);
      } catch {
        // A corrupt or old-shape entry: ignore it and read the store.
        this.log?.warn({ note_id: id }, "discarding an undecodable cache entry");
      }
    }
    const note = await this.inner.get(id); // throws NotFoundError on a miss
    await this.cache.set(key, encode(note), this.ttlSeconds);
    return note;
  }

  async list(offset: number, limit: number): Promise<NotePage> {
    return this.inner.list(offset, limit);
  }
}

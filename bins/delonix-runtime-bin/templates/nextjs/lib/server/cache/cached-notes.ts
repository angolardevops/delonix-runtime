// Cache-aside over the notes use cases: get(id) reads through the cache.
//
// Wraps a NotesApi from the OUTSIDE, exactly like TracedNotes, so lib/notes
// never imports the cache. A note is immutable once created, so a cached note
// is never stale — only the TTL expires it. Only get is cached: list changes
// whenever a note is created, and create returns a note the caller already
// holds (it is written to the cache so the first read is a hit). The cache is
// a slow path, never a failure path — Cache swallows backend errors, and a
// decode error here falls through to the store too.
import "server-only";
import type {
  CreateNoteInput,
  Note,
  NotePage,
  NotesApi,
} from "@/lib/notes/notes";
import type { Cache } from "./cache";

const KEY_PREFIX = "note:v1:";

function encode(note: Note): string {
  return JSON.stringify(note);
}

function decode(raw: string): Note {
  const data = JSON.parse(raw) as Partial<Note>;
  if (
    typeof data.id !== "string" ||
    typeof data.title !== "string" ||
    typeof data.body !== "string" ||
    typeof data.created_at !== "string"
  ) {
    throw new Error("cache entry is not a note");
  }
  return { id: data.id, title: data.title, body: data.body, created_at: data.created_at };
}

export class CachedNotes implements NotesApi {
  constructor(
    private readonly inner: NotesApi,
    private readonly cache: Cache,
    private readonly ttlSeconds: number,
  ) {}

  async create(input: CreateNoteInput): Promise<Note> {
    const note = await this.inner.create(input);
    await this.cache.set(KEY_PREFIX + note.id, encode(note), this.ttlSeconds);
    return note;
  }

  async get(id: string): Promise<Note> {
    const key = KEY_PREFIX + id;
    const raw = await this.cache.get(key);
    if (raw !== undefined) {
      try {
        return decode(raw);
      } catch {
        // A corrupt or old-shape entry: ignore it and read the store.
      }
    }
    const note = await this.inner.get(id); // throws NoteNotFoundError on a miss
    await this.cache.set(key, encode(note), this.ttlSeconds);
    return note;
  }

  async list(offset: number, limit: number): Promise<NotePage> {
    return this.inner.list(offset, limit);
  }
}

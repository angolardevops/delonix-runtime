import { Injectable } from "@nestjs/common";

import { AppConfig } from "../config/app-config";
import { AppLogger } from "../logging/app-logger";
import type { CreateNoteInput, Note } from "../notes/note";
import { NotePublisher } from "../notes/note.publisher";
import { NoteRepository } from "../notes/note.repository";
import { TracedNotesService } from "../telemetry/traced-notes.service";
import { Cache } from "./cache";

const KEY_PREFIX = "note:v1:";

/**
 * Cache-aside over the notes use cases, wrapping them from the OUTSIDE exactly
 * like TracedNotesService does — the notes package never imports the cache.
 *
 * A note is immutable once created, so a cached note is never stale: only the
 * TTL expires it. Only `get` is cached (`list` changes whenever a note is
 * created); `create` writes the new note to the cache so its first read is a
 * hit. The cache is a slow path, never a failure path: {@link Cache} swallows
 * backend errors, and an undecodable entry here falls through to the store too.
 *
 * It extends TracedNotesService, so a store read is still a traced use case;
 * the cache layer sits above the span, and a cache HIT serves without one.
 */
@Injectable()
export class CachedNotesService extends TracedNotesService {
  private readonly ttlMs: number;

  constructor(
    repository: NoteRepository,
    publisher: NotePublisher,
    private readonly cache: Cache,
    config: AppConfig,
    private readonly logger: AppLogger,
  ) {
    super(repository, publisher);
    this.ttlMs = config.cacheTtlMs;
  }

  override async create(input: CreateNoteInput): Promise<Note> {
    const note = await super.create(input);
    await this.cache.set(KEY_PREFIX + note.id, encode(note), this.ttlMs);
    return note;
  }

  override async get(id: string): Promise<Note> {
    const key = KEY_PREFIX + id;
    const raw = await this.cache.get(key);
    if (raw !== null) {
      const cached = decode(raw);
      if (cached !== undefined) {
        return cached;
      }
      // A corrupt or old-shape entry: ignore it and read the store.
      this.logger.warn("discarding an undecodable cache entry", { note_id: id });
    }
    const note = await super.get(id); // raises NoteNotFoundError on a miss
    await this.cache.set(key, encode(note), this.ttlMs);
    return note;
  }
}

function encode(note: Note): Buffer {
  return Buffer.from(JSON.stringify(note));
}

function decode(raw: Buffer): Note | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw.toString());
  } catch {
    return undefined;
  }
  if (parsed === null || typeof parsed !== "object") {
    return undefined;
  }
  const value = parsed as Record<string, unknown>;
  if (
    typeof value.id !== "string" ||
    typeof value.title !== "string" ||
    typeof value.body !== "string" ||
    typeof value.created_at !== "string"
  ) {
    return undefined;
  }
  return { id: value.id, title: value.title, body: value.body, created_at: value.created_at };
}

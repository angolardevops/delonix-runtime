// The example capability: create, read and list short notes. This module
// holds the rules and the use cases, and names what it needs from the outside
// world as two small interfaces (NoteStore, NotePublisher). It imports nothing
// about HTTP, Next.js, telemetry or storage technology — tests/architecture.test.ts
// fails if it ever does. `server-only` makes a Client Component that imports
// it fail the build instead of shipping the rules to the browser.
import "server-only";

export const MAX_TITLE_LENGTH = 200;
export const MAX_BODY_LENGTH = 10_000;
export const MAX_PAGE_SIZE = 100;
export const DEFAULT_PAGE_SIZE = 20;

export interface Note {
  id: string;
  title: string;
  body: string;
  created_at: string;
}

export interface NotePage {
  items: Note[];
  total: number;
  offset: number;
  limit: number;
}

// The two errors are recognised by their `kind`, not with `instanceof`:
// Next.js bundles instrumentation.ts and each route separately, so this module
// can be loaded more than once in a process, and `instanceof` is false between
// copies of the same class.

/** Every field that failed, so a client fixes a request in one round trip. */
export class ValidationError extends Error {
  readonly kind = "validation_failed";

  constructor(readonly fields: Record<string, string>) {
    super(
      "invalid note: " +
        Object.entries(fields)
          .map(([field, message]) => `${field}: ${message}`)
          .join("; "),
    );
    this.name = "ValidationError";
  }
}

export class NoteNotFoundError extends Error {
  readonly kind = "not_found";

  constructor(readonly id: string) {
    super("note not found");
    this.name = "NoteNotFoundError";
  }
}

const kindOf = (err: unknown): unknown =>
  typeof err === "object" && err !== null && "kind" in err ? err.kind : undefined;

export function isValidationError(err: unknown): err is ValidationError {
  return kindOf(err) === "validation_failed";
}

export function isNoteNotFound(err: unknown): err is NoteNotFoundError {
  return kindOf(err) === "not_found";
}

/** Persistence port. The in-memory adapter is lib/server/memory-note-store.ts. */
export interface NoteStore {
  save(note: Note): Promise<void>;
  get(id: string): Promise<Note | undefined>;
  /** One page, newest first, and the total count. */
  list(offset: number, limit: number): Promise<{ items: Note[]; total: number }>;
}

/**
 * Tells the outside world that a note exists. Must not block the caller on
 * the network: the outbound webhook adapter schedules the delivery for after
 * the response.
 */
export interface NotePublisher {
  noteCreated(note: Note): void;
}

export interface CreateNoteInput {
  title: string;
  body?: string;
}

/** What the transport and the pages need from the use cases. */
export interface NotesApi {
  create(input: CreateNoteInput): Promise<Note>;
  get(id: string): Promise<Note>;
  list(offset: number, limit: number): Promise<NotePage>;
}

export interface NotesServiceOptions {
  now?: () => Date;
  newId?: () => string;
}

/** Counts code points, not UTF-16 units, so "é" and "😀" are one character each. */
const length = (s: string): number => [...s].length;

export class NotesService implements NotesApi {
  private readonly now: () => Date;
  private readonly newId: () => string;

  constructor(
    private readonly store: NoteStore,
    private readonly publisher?: NotePublisher,
    options: NotesServiceOptions = {},
  ) {
    this.now = options.now ?? (() => new Date());
    this.newId = options.newId ?? (() => globalThis.crypto.randomUUID().replaceAll("-", ""));
  }

  async create(input: CreateNoteInput): Promise<Note> {
    const title = input.title.trim();
    const body = input.body ?? "";
    const fields: Record<string, string> = {};
    if (title === "") {
      fields.title = "is required";
    } else if (length(title) > MAX_TITLE_LENGTH) {
      fields.title = `must be at most ${MAX_TITLE_LENGTH} characters`;
    }
    if (length(body) > MAX_BODY_LENGTH) {
      fields.body = `must be at most ${MAX_BODY_LENGTH} characters`;
    }
    if (Object.keys(fields).length > 0) {
      throw new ValidationError(fields);
    }
    const note: Note = { id: this.newId(), title, body, created_at: this.now().toISOString() };
    await this.store.save(note);
    this.publisher?.noteCreated(note);
    return note;
  }

  async get(id: string): Promise<Note> {
    const note = await this.store.get(id);
    if (!note) {
      throw new NoteNotFoundError(id);
    }
    return note;
  }

  async list(offset: number, limit: number): Promise<NotePage> {
    if (!Number.isInteger(limit) || limit < 1 || limit > MAX_PAGE_SIZE) {
      throw new ValidationError({ limit: `must be between 1 and ${MAX_PAGE_SIZE}` });
    }
    if (!Number.isInteger(offset) || offset < 0) {
      throw new ValidationError({ offset: "must not be negative" });
    }
    const { items, total } = await this.store.list(offset, limit);
    return { items, total, offset, limit };
  }
}

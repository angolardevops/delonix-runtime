/**
 * The example capability: create, read and list short notes. This module holds
 * the rules and the use cases and names what it needs from outside as two small
 * interfaces (NoteStore, NotePublisher). It imports nothing about Fastify,
 * telemetry or storage — test/architecture.test.ts fails if it ever does.
 */
import { randomBytes } from "node:crypto";

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

/** Every field that failed, so a client fixes a request in one round trip. */
export class ValidationError extends Error {
  constructor(readonly fields: Record<string, string>) {
    super("invalid note");
  }
}

export class NotFoundError extends Error {
  constructor() {
    super("note not found");
  }
}

/** Persistence port. The in-memory adapter is ./memory-store.ts. */
export interface NoteStore {
  save(note: Note): Promise<void>;
  get(id: string): Promise<Note | undefined>;
  /** One page, newest first, and the total count. */
  list(offset: number, limit: number): Promise<{ items: Note[]; total: number }>;
}

/** Tells the outside world a note exists. Must not block on the network. */
export interface NotePublisher {
  noteCreated(note: Note): void;
}

export interface CreateNoteInput {
  title: string;
  body?: string;
}

const length = (s: string): number => [...s].length;

export class NotesService {
  constructor(
    private readonly store: NoteStore,
    private readonly publisher?: NotePublisher,
    private readonly now: () => Date = () => new Date(),
    private readonly newId: () => string = () => randomBytes(16).toString("hex"),
  ) {}

  async create(input: CreateNoteInput): Promise<Note> {
    const title = input.title.trim();
    const body = input.body ?? "";
    const fields: Record<string, string> = {};
    if (title === "") fields.title = "is required";
    else if (length(title) > MAX_TITLE_LENGTH)
      fields.title = `must be at most ${MAX_TITLE_LENGTH} characters`;
    if (length(body) > MAX_BODY_LENGTH)
      fields.body = `must be at most ${MAX_BODY_LENGTH} characters`;
    if (Object.keys(fields).length > 0) throw new ValidationError(fields);

    const note: Note = { id: this.newId(), title, body, created_at: this.now().toISOString() };
    await this.store.save(note);
    this.publisher?.noteCreated(note);
    return note;
  }

  async get(id: string): Promise<Note> {
    const note = await this.store.get(id);
    if (!note) throw new NotFoundError();
    return note;
  }

  async list(offset: number, limit: number): Promise<NotePage> {
    if (!Number.isInteger(limit) || limit < 1 || limit > MAX_PAGE_SIZE)
      throw new ValidationError({ limit: `must be between 1 and ${MAX_PAGE_SIZE}` });
    if (!Number.isInteger(offset) || offset < 0)
      throw new ValidationError({ offset: "must not be negative" });
    const { items, total } = await this.store.list(offset, limit);
    return { items, total, offset, limit };
  }
}

/** What the transport needs from the use cases (NotesService and its traced wrapper). */
export type NotesUseCases = Pick<NotesService, "create" | "get" | "list">;

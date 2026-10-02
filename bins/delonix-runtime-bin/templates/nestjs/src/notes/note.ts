// The notes capability: the entity, its limits, and the errors the use cases
// raise. Plain TypeScript — no HTTP, no telemetry, no storage technology. The
// architecture test (test/architecture.e2e-spec.ts) fails if that changes.

/** Limits of a note, enforced by the use case (not by the transport). */
export const MAX_TITLE_LENGTH = 200;
export const MAX_BODY_LENGTH = 10_000;
export const MAX_PAGE_SIZE = 100;
export const DEFAULT_PAGE_SIZE = 20;

export interface Note {
  id: string;
  title: string;
  body: string;
  /** RFC 3339, UTC. */
  created_at: string;
}

export interface NotePage {
  items: Note[];
  total: number;
  offset: number;
  limit: number;
}

export interface CreateNoteInput {
  title: string;
  body?: string;
}

/** Every field that failed, so a client fixes a request in one round trip. */
export class NoteValidationError extends Error {
  constructor(readonly fields: Record<string, string>) {
    super(
      "invalid note: " +
        Object.entries(fields)
          .map(([field, message]) => `${field}: ${message}`)
          .join("; "),
    );
    this.name = "NoteValidationError";
  }
}

export class NoteNotFoundError extends Error {
  constructor(readonly id: string) {
    super("note not found");
    this.name = "NoteNotFoundError";
  }
}

/** Length in characters (code points), not UTF-16 units: "é" and "😀" count 1. */
export function characterCount(value: string): number {
  return [...value].length;
}

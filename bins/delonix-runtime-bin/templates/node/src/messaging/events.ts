/**
 * The `note.created` event, on the wire. Standard library only.
 *
 * One place that says what the event looks like, so the publisher and the
 * consumer cannot drift. The envelope matches the outbound webhook's
 * (`type` + `data`), so a receiver sees the same shape on either transport.
 */
import type { Note } from "../notes/notes.js";

export const EVENT_TYPE = "note.created";
/** Topic-exchange routing key. A consumer binds `note.*` or `note.created`. */
export const ROUTING_KEY = "note.created";

export interface NoteCreatedEvent {
  type: string;
  data: { id: string; title: string; body: string; created_at: string };
}

export function noteCreated(note: Note): NoteCreatedEvent {
  return {
    type: EVENT_TYPE,
    data: { id: note.id, title: note.title, body: note.body, created_at: note.created_at },
  };
}

export function encode(event: NoteCreatedEvent): Buffer {
  return Buffer.from(JSON.stringify(event));
}

/**
 * Parse a message body. Throws on anything that is not a JSON object with a
 * string `type` — a poison message the consumer rejects.
 */
export function decode(body: Buffer): NoteCreatedEvent {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body.toString("utf8"));
  } catch (err) {
    throw new Error(`not JSON: ${err instanceof Error ? err.message : String(err)}`, {
      cause: err,
    });
  }
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    typeof (parsed as { type?: unknown }).type !== "string"
  ) {
    throw new Error("not a valid event envelope");
  }
  return parsed as NoteCreatedEvent;
}

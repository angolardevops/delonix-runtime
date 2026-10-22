// The `note.created` event, on the wire. One place that says what the event
// looks like, so the publisher and the consumer cannot drift. The envelope
// matches the outbound webhook's (`type` + `data`), so a receiver sees the
// same shape on either transport.
import "server-only";
import type { Note } from "@/lib/notes/notes";

export const EVENT_TYPE = "note.created";
/** Topic-exchange routing key. A consumer binds `note.*` or `note.created`. */
export const ROUTING_KEY = "note.created";
/** The durable topic exchange the publisher declares and publishes to. */
export const EXCHANGE = "notes";

export interface NoteCreatedEvent {
  type: typeof EVENT_TYPE;
  data: { id: string; title: string; body: string; created_at: string };
}

export function noteCreated(note: Note): NoteCreatedEvent {
  return {
    type: EVENT_TYPE,
    data: { id: note.id, title: note.title, body: note.body, created_at: note.created_at },
  };
}

export function encode(event: NoteCreatedEvent): string {
  return JSON.stringify(event);
}

/**
 * Parse a message body. Throws on anything that is not a JSON object with a
 * string `type` — a poison message the consumer rejects at once.
 */
export function decode(body: string): { type: string; [key: string]: unknown } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch (err) {
    throw new Error(`not JSON: ${err instanceof Error ? err.message : String(err)}`);
  }
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    Array.isArray(parsed) ||
    typeof (parsed as { type?: unknown }).type !== "string"
  ) {
    throw new Error("not a valid event envelope");
  }
  return parsed as { type: string; [key: string]: unknown };
}

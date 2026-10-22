// The `note.created` event, on the wire. One place that says what the event
// looks like, so the publisher and any consumer cannot drift. The envelope
// matches the outbound webhook's (`type` + `data`), so a receiver sees the same
// shape on either transport.

import type { Note } from "../notes/note";

export const EVENT_TYPE = "note.created";
/** Topic-exchange routing key. A consumer binds `note.*` or `note.created`. */
export const ROUTING_KEY = "note.created";

export interface NoteCreatedEvent {
  type: typeof EVENT_TYPE;
  data: Note;
}

export function noteCreated(note: Note): NoteCreatedEvent {
  return { type: EVENT_TYPE, data: note };
}

export function encode(event: NoteCreatedEvent): Buffer {
  return Buffer.from(JSON.stringify(event));
}

/**
 * Parse a message body. Throws on anything that is not a JSON object with a
 * string `type` — a poison message a consumer rejects rather than retries.
 */
export function decode(body: Buffer): { type: string; data?: unknown } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body.toString());
  } catch (error) {
    throw new Error(`not JSON: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("not a valid event envelope");
  }
  const envelope = parsed as Record<string, unknown>;
  if (typeof envelope.type !== "string") {
    throw new Error("not a valid event envelope");
  }
  return { type: envelope.type, data: envelope.data };
}

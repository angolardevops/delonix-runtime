import type { Note } from "./note";

/**
 * Tells the outside world that a note exists. Implementations must not block
 * the caller on the network: the outbound webhook adapter queues and returns.
 */
export abstract class NotePublisher {
  abstract noteCreated(note: Note): void;
}

/** Used when no outbound webhook is configured. */
export class NoopNotePublisher extends NotePublisher {
  noteCreated(): void {
    // Nothing to tell.
  }
}

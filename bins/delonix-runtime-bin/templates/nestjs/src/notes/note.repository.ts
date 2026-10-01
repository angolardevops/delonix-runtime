import type { Note } from "./note";

/**
 * The persistence port. An abstract class (not an interface) because Nest
 * resolves providers by runtime token: the class is both the contract and the
 * injection token. The in-memory adapter is InMemoryNoteRepository; a database
 * adapter implements the same three methods and replaces it in NotesModule.
 */
export abstract class NoteRepository {
  abstract save(note: Note): Promise<void>;
  /** The note, or undefined when there is none with that id. */
  abstract get(id: string): Promise<Note | undefined>;
  /** One page, newest first, and the total count. */
  abstract list(offset: number, limit: number): Promise<{ items: Note[]; total: number }>;
}

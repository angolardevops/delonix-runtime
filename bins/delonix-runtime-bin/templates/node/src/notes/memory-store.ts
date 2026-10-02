import type { Note, NoteStore } from "./notes.js";

/**
 * In-memory adapter of NoteStore: enough for the example and for tests, and it
 * keeps nothing across restarts. A database adapter implements the same three
 * methods; the use cases do not change.
 */
export class MemoryNoteStore implements NoteStore {
  private readonly notes = new Map<string, Note>();

  async save(note: Note): Promise<void> {
    this.notes.set(note.id, note);
  }

  async get(id: string): Promise<Note | undefined> {
    return this.notes.get(id);
  }

  async list(offset: number, limit: number): Promise<{ items: Note[]; total: number }> {
    // Newest first; ties broken by id so pages are stable.
    const all = [...this.notes.values()].sort(
      (a, b) => b.created_at.localeCompare(a.created_at) || a.id.localeCompare(b.id),
    );
    return { items: all.slice(offset, offset + limit), total: all.length };
  }
}

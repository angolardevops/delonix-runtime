// In-memory adapter of NoteStore. Notes vanish on restart and are not shared
// between replicas — replace it with a database adapter for real use (see
// ARCHITECTURE.md). The store keeps notes in insertion order; listing reads
// them newest first.
import "server-only";
import type { Note, NoteStore } from "@/lib/notes/notes";

export class MemoryNoteStore implements NoteStore {
  private readonly notes: Note[] = [];
  private readonly byId = new Map<string, Note>();

  async save(note: Note): Promise<void> {
    this.notes.push(note);
    this.byId.set(note.id, note);
  }

  async get(id: string): Promise<Note | undefined> {
    return this.byId.get(id);
  }

  async list(offset: number, limit: number): Promise<{ items: Note[]; total: number }> {
    const total = this.notes.length;
    const end = Math.max(0, total - offset);
    const start = Math.max(0, end - limit);
    return { items: this.notes.slice(start, end).reverse(), total };
  }
}

import { Injectable } from "@nestjs/common";

import type { Note } from "./note";
import { NoteRepository } from "./note.repository";

/**
 * NoteRepository in process memory. Everything is lost on restart and nothing
 * is shared between replicas — enough for the example, replace it for real use.
 */
@Injectable()
export class InMemoryNoteRepository extends NoteRepository {
  private readonly notes = new Map<string, Note>();
  /** Insertion order; the newest note is last. */
  private readonly order: string[] = [];

  save(note: Note): Promise<void> {
    if (!this.notes.has(note.id)) {
      this.order.push(note.id);
    }
    this.notes.set(note.id, note);
    return Promise.resolve();
  }

  get(id: string): Promise<Note | undefined> {
    return Promise.resolve(this.notes.get(id));
  }

  list(offset: number, limit: number): Promise<{ items: Note[]; total: number }> {
    const newestFirst = [...this.order].reverse();
    const items = newestFirst
      .slice(offset, offset + limit)
      .map((id) => this.notes.get(id))
      .filter((note): note is Note => note !== undefined);
    return Promise.resolve({ items, total: this.order.length });
  }
}

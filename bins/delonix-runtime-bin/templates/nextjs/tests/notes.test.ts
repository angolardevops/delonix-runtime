import { describe, expect, it } from "vitest";
import {
  isNoteNotFound,
  isValidationError,
  NotesService,
  type Note,
  type NotePublisher,
} from "@/lib/notes/notes";
import { MemoryNoteStore } from "@/lib/server/memory-note-store";

function service(publisher?: NotePublisher) {
  let n = 0;
  return new NotesService(new MemoryNoteStore(), publisher, {
    now: () => new Date(Date.UTC(2026, 0, 1, 0, 0, n)),
    newId: () => `id${++n}`,
  });
}

describe("NotesService", () => {
  it("creates a note with a trimmed title and publishes it", async () => {
    const published: Note[] = [];
    const notes = service({ noteCreated: (note) => published.push(note) });
    const note = await notes.create({ title: "  hello  ", body: "world" });
    expect(note).toEqual({
      id: "id1",
      title: "hello",
      body: "world",
      created_at: "2026-01-01T00:00:01.000Z",
    });
    expect(published).toEqual([note]);
    expect(await notes.get("id1")).toEqual(note);
  });

  it("reports every invalid field at once and stores nothing", async () => {
    const published: Note[] = [];
    const notes = service({ noteCreated: (note) => published.push(note) });
    const err = await notes.create({ title: "   ", body: "x".repeat(10_001) }).catch((e: unknown) => e);
    expect(isValidationError(err) && err.fields).toEqual({
      title: "is required",
      body: "must be at most 10000 characters",
    });
    expect(published).toEqual([]);
    expect((await notes.list(0, 20)).total).toBe(0);
  });

  it("counts characters, not UTF-16 units", async () => {
    const notes = service();
    await expect(notes.create({ title: "😀".repeat(200) })).resolves.toBeTruthy();
    const err = await notes.create({ title: "😀".repeat(201) }).catch((e: unknown) => e);
    expect(isValidationError(err) && err.fields.title).toBe("must be at most 200 characters");
  });

  it("fails with not_found for an unknown id", async () => {
    const err = await service()
      .get("nope")
      .catch((e: unknown) => e);
    expect(isNoteNotFound(err)).toBe(true);
  });

  it("lists newest first, one page at a time", async () => {
    const notes = service();
    for (const title of ["a", "b", "c", "d", "e"]) await notes.create({ title });
    const first = await notes.list(0, 2);
    expect(first.items.map((n) => n.title)).toEqual(["e", "d"]);
    expect(first).toMatchObject({ total: 5, offset: 0, limit: 2 });
    expect((await notes.list(4, 2)).items.map((n) => n.title)).toEqual(["a"]);
    expect((await notes.list(9, 2)).items).toEqual([]);
  });

  it("refuses a page size outside 1..100 and a negative offset", async () => {
    const notes = service();
    for (const [offset, limit, field] of [
      [0, 0, "limit"],
      [0, 101, "limit"],
      [-1, 20, "offset"],
    ] as const) {
      const err = await notes.list(offset, limit).catch((e: unknown) => e);
      expect(isValidationError(err) && Object.keys(err.fields)).toEqual([field]);
    }
  });
});

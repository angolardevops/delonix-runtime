import assert from "node:assert/strict";
import { test } from "node:test";
import { MemoryNoteStore } from "../src/notes/memory-store.js";
import {
  MAX_BODY_LENGTH,
  MAX_PAGE_SIZE,
  NotFoundError,
  NotesService,
  ValidationError,
  type Note,
} from "../src/notes/notes.js";

test("create stores, publishes and trims", async () => {
  const published: Note[] = [];
  const svc = new NotesService(new MemoryNoteStore(), { noteCreated: (n) => published.push(n) });
  const note = await svc.create({ title: "  hello  ", body: "world" });
  assert.equal(note.title, "hello");
  assert.deepEqual(await svc.get(note.id), note);
  assert.deepEqual(published, [note]);
});

test("create rejects every invalid field at once and publishes nothing", async () => {
  const published: Note[] = [];
  const svc = new NotesService(new MemoryNoteStore(), { noteCreated: (n) => published.push(n) });
  await assert.rejects(
    svc.create({ title: " ", body: "x".repeat(MAX_BODY_LENGTH + 1) }),
    (err: unknown) =>
      err instanceof ValidationError && Boolean(err.fields.title) && Boolean(err.fields.body),
  );
  assert.equal(published.length, 0);
});

test("list pages newest first and enforces its bounds", async () => {
  let tick = 0;
  const svc = new NotesService(
    new MemoryNoteStore(),
    undefined,
    () => new Date(Date.UTC(2026, 0, 1, 0, 0, tick++)),
  );
  for (const title of ["a", "b", "c"]) await svc.create({ title });
  const page = await svc.list(1, 1);
  assert.equal(page.total, 3);
  assert.deepEqual(
    page.items.map((n) => n.title),
    ["b"],
  );
  await assert.rejects(svc.list(0, MAX_PAGE_SIZE + 1), ValidationError);
  await assert.rejects(svc.list(-1, 10), ValidationError);
  await assert.rejects(svc.get("missing"), NotFoundError);
});

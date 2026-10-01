import { Test } from "@nestjs/testing";

import { InMemoryNoteRepository } from "./in-memory-note.repository";
import { type Note, NoteNotFoundError, NoteValidationError } from "./note";
import { NotePublisher } from "./note.publisher";
import { NoteRepository } from "./note.repository";
import { NotesService } from "./notes.service";

class RecordingPublisher extends NotePublisher {
  readonly published: Note[] = [];

  noteCreated(note: Note): void {
    this.published.push(note);
  }
}

describe("NotesService", () => {
  let service: NotesService;
  let publisher: RecordingPublisher;

  beforeEach(async () => {
    // The real DI wiring of the use case, with the ports replaced.
    const module = await Test.createTestingModule({
      providers: [
        NotesService,
        { provide: NoteRepository, useClass: InMemoryNoteRepository },
        { provide: NotePublisher, useClass: RecordingPublisher },
      ],
    }).compile();
    service = module.get(NotesService);
    publisher = module.get(NotePublisher);
  });

  it("creates a trimmed note, stores it and publishes it", async () => {
    const note = await service.create({ title: "  hello  ", body: "world" });
    expect(note).toMatchObject({ title: "hello", body: "world" });
    expect(note.id).toMatch(/^[0-9a-f-]{36}$/);
    expect(new Date(note.created_at).toISOString()).toBe(note.created_at);
    expect(await service.get(note.id)).toEqual(note);
    expect(publisher.published).toEqual([note]);
  });

  it("defaults the body to empty", async () => {
    expect((await service.create({ title: "t" })).body).toBe("");
  });

  it.each([
    [{ title: "" }, { title: "is required" }],
    [{ title: "   " }, { title: "is required" }],
    [{ title: "x".repeat(201) }, { title: "must be at most 200 characters" }],
    [
      { title: "", body: "y".repeat(10_001) },
      { title: "is required", body: "must be at most 10000 characters" },
    ],
  ])("refuses %j listing every failing field", async (input, fields) => {
    const error = await service.create(input).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(NoteValidationError);
    expect((error as NoteValidationError).fields).toEqual(fields);
    expect(publisher.published).toEqual([]);
  });

  it("counts characters, not UTF-16 units", async () => {
    await expect(service.create({ title: "😀".repeat(200) })).resolves.toBeDefined();
    await expect(service.create({ title: "😀".repeat(201) })).rejects.toBeInstanceOf(NoteValidationError);
  });

  it("does not publish when the store fails", async () => {
    const failing = new (class extends InMemoryNoteRepository {
      override save(): Promise<void> {
        return Promise.reject(new Error("disk full"));
      }
    })();
    const broken = new NotesService(failing, publisher);
    await expect(broken.create({ title: "t" })).rejects.toThrow("disk full");
    expect(publisher.published).toEqual([]);
  });

  it("raises NoteNotFoundError for an unknown id", async () => {
    await expect(service.get("nope")).rejects.toBeInstanceOf(NoteNotFoundError);
  });

  it("lists newest first with the total", async () => {
    for (const title of ["a", "b", "c"]) {
      await service.create({ title });
    }
    const page = await service.list(0, 2);
    expect(page.items.map((n) => n.title)).toEqual(["c", "b"]);
    expect(page).toMatchObject({ total: 3, offset: 0, limit: 2 });
    expect((await service.list(2, 2)).items.map((n) => n.title)).toEqual(["a"]);
    expect((await service.list(10, 2)).items).toEqual([]);
  });

  it.each([
    [0, 0, { limit: "must be between 1 and 100" }],
    [0, 101, { limit: "must be between 1 and 100" }],
    [-1, 20, { offset: "must not be negative" }],
    [0.5, 1.5, { limit: "must be between 1 and 100", offset: "must not be negative" }],
  ])("refuses offset=%p limit=%p", async (offset, limit, fields) => {
    const error = await service.list(offset, limit).catch((e: unknown) => e);
    expect((error as NoteValidationError).fields).toEqual(fields);
  });
});

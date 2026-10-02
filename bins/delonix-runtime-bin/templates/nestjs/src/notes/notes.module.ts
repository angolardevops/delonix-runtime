import { Module } from "@nestjs/common";

import { TracedNotesService } from "../telemetry/traced-notes.service";
import { InMemoryNoteRepository } from "./in-memory-note.repository";
import { NoteRepository } from "./note.repository";
import { NotesController } from "./notes.controller";
import { NotesService } from "./notes.service";

/**
 * The notes capability. The providers say which adapter implements each port:
 * swap InMemoryNoteRepository for a database adapter here and nothing else
 * changes. NotePublisher comes from the (global) outbound webhook module.
 * NotesService is provided as TracedNotesService, which opens a span per use
 * case — telemetry by decoration, so the use cases never import it.
 */
@Module({
  controllers: [NotesController],
  providers: [
    { provide: NoteRepository, useClass: InMemoryNoteRepository },
    { provide: NotesService, useClass: TracedNotesService },
  ],
  exports: [NotesService],
})
export class NotesModule {}

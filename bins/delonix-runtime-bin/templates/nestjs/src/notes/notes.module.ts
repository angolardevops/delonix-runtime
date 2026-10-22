import { Module } from "@nestjs/common";

import { CachedNotesService } from "../cache/cached-notes.service";
import { InMemoryNoteRepository } from "./in-memory-note.repository";
import { NoteRepository } from "./note.repository";
import { NotesController } from "./notes.controller";
import { NotesService } from "./notes.service";

/**
 * The notes capability. The providers say which adapter implements each port:
 * swap InMemoryNoteRepository for a database adapter here and nothing else
 * changes. NotePublisher comes from the (global) MessagingModule and Cache from
 * the (global) CacheModule. NotesService is provided as CachedNotesService,
 * which reads through the cache and, in turn, extends TracedNotesService (a
 * span per use case) — caching and telemetry by decoration, so the use cases
 * never import either.
 */
@Module({
  controllers: [NotesController],
  providers: [
    { provide: NoteRepository, useClass: InMemoryNoteRepository },
    { provide: NotesService, useClass: CachedNotesService },
  ],
  exports: [NotesService],
})
export class NotesModule {}

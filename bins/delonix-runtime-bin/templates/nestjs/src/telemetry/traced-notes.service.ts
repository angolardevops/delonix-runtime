import { Injectable } from "@nestjs/common";
import { type Span, SpanStatusCode, trace } from "@opentelemetry/api";

import type { CreateNoteInput, Note, NotePage } from "../notes/note";
import { NotesService } from "../notes/notes.service";

const tracer = trace.getTracer("notes");

/**
 * NotesService with one span per use case (`notes.create`, `notes.get`,
 * `notes.list`), children of the request span. It adds telemetry and nothing
 * else; the rules stay in NotesService.
 */
@Injectable()
export class TracedNotesService extends NotesService {
  override create(input: CreateNoteInput): Promise<Note> {
    return traced("notes.create", () => super.create(input));
  }

  override get(id: string): Promise<Note> {
    return traced("notes.get", () => super.get(id));
  }

  override list(offset: number, limit: number): Promise<NotePage> {
    return traced("notes.list", () => super.list(offset, limit));
  }
}

async function traced<T>(name: string, run: () => Promise<T>): Promise<T> {
  return tracer.startActiveSpan(name, async (span: Span) => {
    try {
      return await run();
    } catch (error) {
      span.recordException(error as Error);
      span.setStatus({ code: SpanStatusCode.ERROR });
      throw error;
    } finally {
      span.end();
    }
  });
}

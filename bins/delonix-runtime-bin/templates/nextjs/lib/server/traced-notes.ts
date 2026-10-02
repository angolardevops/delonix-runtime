// Telemetry reaches the use cases by decoration, not by imports inside
// lib/notes: each call becomes a `notes.*` span, a child of the request span
// Next.js opened.
import "server-only";
import { SpanStatusCode, trace, type Span } from "@opentelemetry/api";
import {
  isNoteNotFound,
  isValidationError,
  type CreateNoteInput,
  type Note,
  type NotePage,
  type NotesApi,
} from "@/lib/notes/notes";

export class TracedNotes implements NotesApi {
  constructor(private readonly next: NotesApi) {}

  create(input: CreateNoteInput): Promise<Note> {
    return this.span("notes.create", async (span) => {
      const note = await this.next.create(input);
      span.setAttribute("note.id", note.id);
      return note;
    });
  }

  get(id: string): Promise<Note> {
    return this.span("notes.get", () => this.next.get(id));
  }

  list(offset: number, limit: number): Promise<NotePage> {
    return this.span("notes.list", () => this.next.list(offset, limit));
  }

  private span<T>(name: string, fn: (span: Span) => Promise<T>): Promise<T> {
    return trace.getTracer("notes").startActiveSpan(name, async (span) => {
      try {
        return await fn(span);
      } catch (err) {
        // A refused input or a missing note is an answer, not a failure of the service.
        if (!isValidationError(err) && !isNoteNotFound(err)) {
          span.recordException(err instanceof Error ? err : new Error(String(err)));
          span.setStatus({ code: SpanStatusCode.ERROR });
        }
        throw err;
      } finally {
        span.end();
      }
    });
  }
}

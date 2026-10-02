import { SpanStatusCode, trace } from "@opentelemetry/api";
import type { NotesUseCases } from "../notes/notes.js";
import { SERVICE_NAME } from "../service.js";

const tracer = trace.getTracer(`${SERVICE_NAME}/notes`);

async function span<T>(name: string, run: () => Promise<T>): Promise<T> {
  return tracer.startActiveSpan(name, async (s) => {
    try {
      return await run();
    } catch (err) {
      s.setStatus({ code: SpanStatusCode.ERROR, message: "use case failed" });
      throw err;
    } finally {
      s.end();
    }
  });
}

/**
 * Wraps the use cases with one span each, so a trace shows the business step
 * between the HTTP span and the calls it makes — without notes.ts importing
 * any telemetry.
 */
export function tracedNotes(next: NotesUseCases): NotesUseCases {
  return {
    create: (input) => span("notes.create", () => next.create(input)),
    get: (id) => span("notes.get", () => next.get(id)),
    list: (offset, limit) => span("notes.list", () => next.list(offset, limit)),
  };
}

import { Writable } from "node:stream";
import type { FastifyInstance } from "fastify";
import { buildApp } from "../src/app.js";
import { Readiness } from "../src/health/readiness.js";
import { createLogger } from "../src/logging.js";
import { MemoryNoteStore } from "../src/notes/memory-store.js";
import { NotesService, type NotePublisher } from "../src/notes/notes.js";
import { Dedup } from "../src/webhooks/dedup.js";

export const TEST_KEY = Buffer.alloc(32, 7);

/** Collects log lines so a test can read what was written. */
export function logSink(): { stream: Writable; lines: () => Record<string, unknown>[] } {
  let buf = "";
  const stream = new Writable({
    write(chunk, _enc, done) {
      buf += String(chunk);
      done();
    },
  });
  return {
    stream,
    lines: () =>
      buf
        .split("\n")
        .filter(Boolean)
        .map((l) => JSON.parse(l) as Record<string, unknown>),
  };
}

export async function testApp(
  opts: { publisher?: NotePublisher; now?: () => number } = {},
): Promise<{
  app: FastifyInstance;
  readiness: Readiness;
  logs: () => Record<string, unknown>[];
}> {
  const sink = logSink();
  const readiness = new Readiness();
  readiness.setReady();
  const app = await buildApp({
    logger: createLogger(
      { logLevel: "debug", serviceName: "svc", version: "test", env: "test" },
      sink.stream,
    ),
    notes: new NotesService(new MemoryNoteStore(), opts.publisher),
    readiness,
    maxBodyBytes: 1024,
    inboundWebhook: { key: TEST_KEY, dedup: new Dedup(16), now: opts.now },
  });
  return { app, readiness, logs: sink.lines };
}

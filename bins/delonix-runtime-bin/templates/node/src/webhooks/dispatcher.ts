import {
  context,
  metrics,
  ROOT_CONTEXT,
  SpanKind,
  SpanStatusCode,
  trace,
  type SpanContext,
} from "@opentelemetry/api";
import type { Logger } from "pino";
import type { Note, NotePublisher } from "../notes/notes.js";
import { SERVICE_NAME } from "../service.js";
import { HEADER_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, sign } from "./signature.js";

export interface DispatcherOptions {
  url: string;
  key: Buffer;
  /** Per attempt. */
  timeoutMs: number;
  maxAttempts: number;
  baseBackoffMs: number;
  queueSize: number;
  /** Replaced in tests. */
  sleep?: (ms: number) => Promise<void>;
}

interface Job {
  note: Note;
  parent?: SpanContext;
}

const tracer = trace.getTracer(`${SERVICE_NAME}/webhook`);

/**
 * Delivers `note.created` events to one URL in the background: a bounded
 * queue, one worker, bounded attempts with exponential backoff and full jitter.
 *
 * Delivery is AT-LEAST-ONCE while the process lives and AT-MOST-ONCE across a
 * restart: the queue is memory, so what is queued when the process dies is
 * lost, and a full queue drops (and logs, and counts) instead of blocking the
 * request that created the note. The webhook-id is the note id, so a receiver
 * that de-duplicates by it sees each note once however many attempts it took.
 * A deployment that must not lose an event needs an outbox table written in
 * the same transaction as the note — see docs/adr/0002-webhooks.md.
 */
export class WebhookDispatcher implements NotePublisher {
  private readonly queue: Job[] = [];
  private closed = false;
  private working?: Promise<void>;
  private readonly outcomes = metrics
    .getMeter(`${SERVICE_NAME}/webhook`)
    .createCounter("webhook.deliveries", {
      description: "Outbound webhook deliveries by final outcome",
    });
  private readonly sleep: (ms: number) => Promise<void>;

  constructor(
    private readonly opts: DispatcherOptions,
    private readonly log: Logger,
  ) {
    this.sleep = opts.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
  }

  /** Never blocks on the network. */
  noteCreated(note: Note): void {
    if (this.closed || this.queue.length >= this.opts.queueSize) {
      this.outcomes.add(1, { outcome: "dropped" });
      this.log.error({ note_id: note.id }, "webhook queue full or closed, event dropped");
      return;
    }
    this.queue.push({ note, parent: trace.getActiveSpan()?.spanContext() });
    this.working ??= this.drain();
  }

  /**
   * Stops taking events and waits for the queue to drain, up to the deadline.
   * Returns false when the deadline cut it; what was still queued is lost.
   */
  async close(deadlineMs: number): Promise<boolean> {
    this.closed = true;
    if (!this.working) return true;
    let timer: NodeJS.Timeout | undefined;
    const timeout = new Promise<false>((resolve) => {
      timer = setTimeout(() => resolve(false), deadlineMs);
    });
    const done = await Promise.race([this.working.then(() => true as const), timeout]);
    clearTimeout(timer);
    return done;
  }

  private async drain(): Promise<void> {
    for (let job = this.queue.shift(); job; job = this.queue.shift()) {
      await this.deliver(job);
    }
    this.working = undefined;
  }

  private async deliver(job: Job): Promise<void> {
    // A child of the request that created the note, so the delivery and the
    // HTTP call it makes share that request's trace id.
    const parent = job.parent ? trace.setSpanContext(ROOT_CONTEXT, job.parent) : ROOT_CONTEXT;
    const span = tracer.startSpan(
      "webhook.deliver",
      { kind: SpanKind.PRODUCER, attributes: { "webhook.event": "note.created" } },
      parent,
    );
    await context.with(trace.setSpan(parent, span), async () => {
      const body = JSON.stringify({ type: "note.created", data: job.note });
      let lastError = "no attempt made";
      for (let attempt = 1; attempt <= this.opts.maxAttempts; attempt++) {
        const result = await this.attempt(job.note.id, body);
        if (result.ok) {
          span.setAttribute("webhook.attempts", attempt);
          this.outcomes.add(1, { outcome: "delivered" });
          this.log.info({ note_id: job.note.id, attempts: attempt }, "webhook delivered");
          span.end();
          return;
        }
        lastError = result.error;
        if (!result.retry || attempt === this.opts.maxAttempts) break;
        // Full jitter: uniform in [0, base * 2^(attempt-1)).
        await this.sleep(Math.random() * this.opts.baseBackoffMs * 2 ** (attempt - 1));
      }
      span.setStatus({ code: SpanStatusCode.ERROR, message: "delivery failed" });
      this.outcomes.add(1, { outcome: "failed" });
      this.log.error({ note_id: job.note.id, error: lastError }, "webhook delivery failed");
      span.end();
    });
  }

  /** One attempt. `retry` says whether trying again could help. */
  private async attempt(
    id: string,
    body: string,
  ): Promise<{ ok: true } | { ok: false; retry: boolean; error: string }> {
    const now = Math.floor(Date.now() / 1000);
    try {
      const res = await fetch(this.opts.url, {
        method: "POST",
        body,
        signal: AbortSignal.timeout(this.opts.timeoutMs),
        headers: {
          "content-type": "application/json",
          [HEADER_ID]: id,
          [HEADER_TIMESTAMP]: String(now),
          [HEADER_SIGNATURE]: sign(this.opts.key, id, now, Buffer.from(body)),
        },
      });
      await res.body?.cancel();
      if (res.ok) return { ok: true };
      // 429 and 5xx may pass; any other 4xx means the receiver refused this message.
      const retry = res.status === 429 || res.status >= 500;
      return { ok: false, retry, error: `receiver answered ${res.status}` };
    } catch (err) {
      // Network error or timeout.
      return { ok: false, retry: true, error: err instanceof Error ? err.message : String(err) };
    }
  }
}

import type { OnApplicationShutdown } from "@nestjs/common";
import {
  type Counter,
  metrics,
  ROOT_CONTEXT,
  type Span,
  type SpanContext,
  SpanKind,
  SpanStatusCode,
  trace,
} from "@opentelemetry/api";

import type { AppLogger } from "../logging/app-logger";
import type { Note } from "../notes/note";
import { NotePublisher } from "../notes/note.publisher";
import { HEADER_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, sign } from "./signature";

export interface DispatcherOptions {
  url: string;
  key: Buffer;
  /** Per attempt. */
  timeoutMs: number;
  maxAttempts: number;
  baseBackoffMs: number;
  queueSize: number;
  /** Injected in tests. */
  fetch?: typeof fetch;
  sleep?: (ms: number) => Promise<void>;
  random?: () => number;
  nowSeconds?: () => number;
}

interface Job {
  note: Note;
  /** The span of the request that created the note: the delivery is its child. */
  parent?: SpanContext;
}

/** What the receiver gets as the JSON body. */
export interface NoteCreatedEvent {
  type: "note.created";
  data: Note;
}

const tracer = trace.getTracer("webhook");

/**
 * Delivers `note.created` to one URL in the background: a bounded queue, one
 * worker, bounded attempts with exponential backoff and full jitter.
 *
 * Delivery is AT-LEAST-ONCE while the process lives and AT-MOST-ONCE across a
 * restart: the queue is memory, so what is queued when the process dies is
 * lost, and a full queue drops (and logs, and counts) instead of blocking the
 * request that created the note. The webhook-id is the note id, so a receiver
 * that de-duplicates by it sees each note once however many attempts it took.
 * Never exactly-once. A deployment that must not lose an event needs an outbox
 * table written in the same transaction as the note — see docs/adr/0002.
 */
export class WebhookDispatcher extends NotePublisher implements OnApplicationShutdown {
  private readonly queue: Job[] = [];
  private closed = false;
  private worker?: Promise<void>;
  private readonly deliveries: Counter;
  private readonly fetchFn: typeof fetch;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly random: () => number;
  private readonly nowSeconds: () => number;

  constructor(
    private readonly options: DispatcherOptions,
    private readonly logger: AppLogger,
  ) {
    super();
    this.fetchFn = options.fetch ?? fetch;
    this.sleep = options.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
    this.random = options.random ?? Math.random;
    this.nowSeconds = options.nowSeconds ?? (() => Math.floor(Date.now() / 1000));
    this.deliveries = metrics.getMeter("webhook").createCounter("webhook.deliveries", {
      description: "Outbound webhook deliveries by final outcome",
    });
  }

  /** NotePublisher: queues and returns; never waits on the network. */
  noteCreated(note: Note): void {
    if (this.closed) {
      this.count("dropped");
      this.logger.warn("webhook dispatcher closed, event dropped", { note_id: note.id });
      return;
    }
    if (this.queue.length >= this.options.queueSize) {
      this.count("dropped");
      this.logger.error("webhook queue full, event dropped", { note_id: note.id });
      return;
    }
    this.queue.push({ note, parent: trace.getActiveSpan()?.spanContext() });
    this.startWorker();
  }

  /**
   * Stops taking events and waits for the queue to drain. Called by Nest after
   * the HTTP server has closed (in-flight requests may still enqueue); the
   * whole shutdown is bounded by SHUTDOWN_TIMEOUT in main.ts, and what is
   * still queued when that deadline hits is lost.
   */
  async onApplicationShutdown(): Promise<void> {
    this.closed = true;
    while (this.worker !== undefined) {
      await this.worker;
    }
  }

  /** Events waiting to be delivered (for tests and diagnostics). */
  get pending(): number {
    return this.queue.length;
  }

  private startWorker(): void {
    if (this.worker !== undefined) {
      return;
    }
    this.worker = this.drain().finally(() => {
      this.worker = undefined;
      // An event queued between the last shift and this callback.
      if (this.queue.length > 0) {
        this.startWorker();
      }
    });
  }

  private async drain(): Promise<void> {
    for (let job = this.queue.shift(); job !== undefined; job = this.queue.shift()) {
      await this.deliver(job);
    }
  }

  private deliver(job: Job): Promise<void> {
    // A child of the request that created the note, so the delivery and the
    // HTTP call it makes share that request's trace id. The request's own
    // context is not reused: the delivery outlives the request.
    const parent =
      job.parent !== undefined && trace.isSpanContextValid(job.parent)
        ? trace.setSpanContext(ROOT_CONTEXT, job.parent)
        : ROOT_CONTEXT;
    return tracer.startActiveSpan(
      "webhook.deliver",
      { kind: SpanKind.PRODUCER, attributes: { "webhook.event": "note.created" } },
      parent,
      (span) => this.attempts(job.note, span).finally(() => span.end()),
    );
  }

  private async attempts(note: Note, span: Span): Promise<void> {
    const event: NoteCreatedEvent = { type: "note.created", data: note };
    const body = Buffer.from(JSON.stringify(event));
    let lastError = "";
    for (let attempt = 1; attempt <= this.options.maxAttempts; attempt++) {
      const { ok, retry, error } = await this.attempt(note.id, body);
      if (ok) {
        span.setAttribute("webhook.attempts", attempt);
        this.count("delivered");
        this.logger.log("webhook delivered", { note_id: note.id, attempts: attempt });
        return;
      }
      lastError = error;
      if (!retry || attempt === this.options.maxAttempts) {
        break;
      }
      // Full jitter: uniform in [0, base * 2^(attempt-1)).
      await this.sleep(Math.floor(this.random() * this.options.baseBackoffMs * 2 ** (attempt - 1)));
    }
    span.recordException(new Error(lastError));
    span.setStatus({ code: SpanStatusCode.ERROR, message: "delivery failed" });
    this.count("failed");
    this.logger.error("webhook delivery failed", { note_id: note.id, error: lastError });
  }

  /** One attempt. `retry`: could trying again help? */
  private async attempt(id: string, body: Buffer): Promise<{ ok: boolean; retry: boolean; error: string }> {
    const timestamp = this.nowSeconds();
    try {
      const response = await this.fetchFn(this.options.url, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          [HEADER_ID]: id,
          [HEADER_TIMESTAMP]: String(timestamp),
          [HEADER_SIGNATURE]: sign(this.options.key, id, timestamp, body),
        },
        body: new Uint8Array(body),
        signal: AbortSignal.timeout(this.options.timeoutMs),
      });
      await response.body?.cancel();
      if (response.status >= 200 && response.status < 300) {
        return { ok: true, retry: false, error: "" };
      }
      // 429 and 5xx may pass later; any other 4xx refused this message for good.
      const retry = response.status === 429 || response.status >= 500;
      return { ok: false, retry, error: `receiver answered ${response.status}` };
    } catch (error) {
      // Network error or timeout.
      return { ok: false, retry: true, error: error instanceof Error ? error.message : String(error) };
    }
  }

  private count(outcome: "delivered" | "failed" | "dropped"): void {
    this.deliveries.add(1, { outcome });
  }
}

// Delivers `note.created` to one URL, signed (Standard Webhooks), after the
// response has been sent: bounded concurrency, bounded attempts, a timeout per
// attempt, exponential backoff with full jitter.
//
// Delivery is AT-LEAST-ONCE while the process lives and AT-MOST-ONCE across a
// restart: pending deliveries live in memory, so what is pending when the
// process dies is lost, and a full dispatcher drops (logs, counts) instead of
// slowing the request that created the note. The webhook-id is the note id,
// so a receiver that de-duplicates by it sees each note once however many
// attempts it took. Never exactly-once. docs/adr/0002-webhooks.md has the
// durable alternative (an outbox).
import "server-only";
import {
  context,
  metrics,
  propagation,
  SpanKind,
  SpanStatusCode,
  trace,
  type Counter,
} from "@opentelemetry/api";
import type { Note, NotePublisher } from "@/lib/notes/notes";
import type { Logger } from "../logger";
import { HEADER_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, sign } from "./signature";

/** Runs a task after the current response; `after` from next/server in production. */
export type Scheduler = (task: () => Promise<void>) => void;

export interface DispatcherOptions {
  url: string;
  key: Buffer;
  log: Logger;
  schedule: Scheduler;
  timeoutMs?: number;
  maxAttempts?: number;
  baseBackoffMs?: number;
  maxPending?: number;
  fetch?: typeof fetch;
  nowMs?: () => number;
  sleep?: (ms: number) => Promise<void>;
  random?: () => number;
}

export interface WebhookEvent {
  type: "note.created";
  data: Note;
}

const INSTRUMENTATION_SCOPE = "webhook";

type Outcome = "delivered" | "failed" | "dropped";

export class WebhookDispatcher implements NotePublisher {
  private readonly pending = new Set<Promise<void>>();
  private closed = false;
  private readonly outcomes: Counter;
  private readonly o: Required<Omit<DispatcherOptions, "fetch">> & { fetch: typeof fetch };

  constructor(options: DispatcherOptions) {
    this.o = {
      url: options.url,
      key: options.key,
      log: options.log,
      schedule: options.schedule,
      timeoutMs: options.timeoutMs ?? 5_000,
      maxAttempts: options.maxAttempts ?? 4,
      baseBackoffMs: options.baseBackoffMs ?? 500,
      maxPending: options.maxPending ?? 100,
      // Looked up per call: Next.js replaces globalThis.fetch after startup.
      fetch: options.fetch ?? ((input, init) => globalThis.fetch(input, init)),
      nowMs: options.nowMs ?? (() => Date.now()),
      sleep: options.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms))),
      random: options.random ?? Math.random,
    };
    this.outcomes = metrics
      .getMeter(INSTRUMENTATION_SCOPE)
      .createCounter("webhook.deliveries", { description: "Outbound webhook deliveries by final outcome" });
  }

  get inFlight(): number {
    return this.pending.size;
  }

  /** NotePublisher: schedules the delivery; never waits on the network. */
  noteCreated(note: Note): void {
    const log = this.o.log;
    if (this.closed) {
      this.count("dropped");
      log.warn("webhook dispatcher closed, event dropped", { note_id: note.id });
      return;
    }
    if (this.pending.size >= this.o.maxPending) {
      this.count("dropped");
      log.error("webhook dispatcher full, event dropped", { note_id: note.id });
      return;
    }
    // The delivery runs after the response, outside the request's async
    // scope; the captured context makes it a child of the request's trace.
    const parent = context.active();
    let settle!: () => void;
    const done = new Promise<void>((resolve) => (settle = resolve));
    this.pending.add(done);
    void done.then(() => this.pending.delete(done));
    const run = () =>
      context
        .with(parent, () => this.deliver({ type: "note.created", data: note }))
        .catch((err: unknown) =>
          log.error("webhook delivery crashed", { note_id: note.id, error: String(err) }),
        )
        .finally(settle);
    try {
      this.o.schedule(run);
    } catch (err) {
      // `after()` throws outside a request scope; deliver anyway.
      log.warn("could not schedule webhook after the response, delivering now", { error: String(err) });
      void run();
    }
  }

  /** Stops taking events and waits for pending deliveries, up to timeoutMs. */
  async close(timeoutMs: number): Promise<boolean> {
    this.closed = true;
    if (this.pending.size === 0) return true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timedOut = new Promise<false>((resolve) => (timer = setTimeout(() => resolve(false), timeoutMs)));
    const drained = Promise.allSettled([...this.pending]).then(() => true as const);
    const result = await Promise.race([drained, timedOut]);
    clearTimeout(timer);
    if (!result) {
      this.o.log.error("webhook deliveries not drained before the deadline", { pending: this.pending.size });
    }
    return result;
  }

  private async deliver(event: WebhookEvent): Promise<void> {
    const tracer = trace.getTracer(INSTRUMENTATION_SCOPE);
    const body = JSON.stringify(event);
    await tracer.startActiveSpan(
      "webhook.deliver",
      { kind: SpanKind.PRODUCER, attributes: { "webhook.event": event.type } },
      async (span) => {
        let lastError = "";
        let attempt = 1;
        try {
          for (; attempt <= this.o.maxAttempts; attempt++) {
            const result = await this.attempt(event.data.id, body, attempt);
            if (result.ok) {
              span.setAttribute("webhook.attempts", attempt);
              this.count("delivered");
              this.o.log.info("webhook delivered", { note_id: event.data.id, attempts: attempt });
              return;
            }
            lastError = result.error;
            if (!result.retry || attempt === this.o.maxAttempts) break;
            // Full jitter: uniform in [0, base * 2^(attempt-1)].
            await this.o.sleep(Math.floor(this.o.random() * this.o.baseBackoffMs * 2 ** (attempt - 1)));
          }
          span.recordException(new Error(lastError));
          span.setStatus({ code: SpanStatusCode.ERROR, message: "delivery failed" });
          this.count("failed");
          this.o.log.error("webhook delivery failed", {
            note_id: event.data.id,
            attempts: Math.min(attempt, this.o.maxAttempts),
            error: lastError,
          });
        } finally {
          span.end();
        }
      },
    );
  }

  /** Sends once. `retry` says whether trying again could help. */
  private attempt(
    id: string,
    body: string,
    attempt: number,
  ): Promise<{ ok: true } | { ok: false; retry: boolean; error: string }> {
    const tracer = trace.getTracer(INSTRUMENTATION_SCOPE);
    return tracer.startActiveSpan(
      "POST",
      {
        kind: SpanKind.CLIENT,
        attributes: { "http.request.method": "POST", "url.full": this.o.url, "webhook.attempt": attempt },
      },
      async (span) => {
        const ts = Math.floor(this.o.nowMs() / 1000);
        const headers: Record<string, string> = {
          "content-type": "application/json",
          [HEADER_ID]: id,
          [HEADER_TIMESTAMP]: String(ts),
          [HEADER_SIGNATURE]: sign(this.o.key, id, ts, body),
        };
        // traceparent: the receiver's spans join this trace.
        propagation.inject(context.active(), headers);
        try {
          const res = await this.o.fetch(this.o.url, {
            method: "POST",
            headers,
            body,
            cache: "no-store",
            redirect: "manual",
            signal: AbortSignal.timeout(this.o.timeoutMs),
          });
          span.setAttribute("http.response.status_code", res.status);
          await res.body?.cancel();
          if (res.status >= 200 && res.status < 300) return { ok: true as const };
          span.setStatus({ code: SpanStatusCode.ERROR });
          // 429 and 5xx may pass; any other answer refused this message for good.
          const retry = res.status === 429 || res.status >= 500;
          return { ok: false as const, retry, error: `receiver answered ${res.status}` };
        } catch (err) {
          span.setStatus({ code: SpanStatusCode.ERROR });
          const error = err instanceof Error && err.name === "TimeoutError" ? "timeout" : String(err);
          return { ok: false as const, retry: true, error };
        } finally {
          span.end();
        }
      },
    );
  }

  private count(outcome: Outcome): void {
    this.outcomes.add(1, { outcome });
  }
}

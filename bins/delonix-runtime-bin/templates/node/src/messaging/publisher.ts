/**
 * Publish `note.created` to RabbitMQ without ever blocking a request.
 *
 * `RabbitPublisher` implements `notes.NotePublisher`: `noteCreated` only
 * enqueues (and returns), one worker drains the queue to the broker. The queue
 * is bounded and DROPS — never blocks — when full, exactly like the webhook
 * dispatcher. The AMQP specifics live behind `Sink`, injected, so the
 * queue/worker/backoff logic is tested with no broker; the default sink opens
 * an amqplib topic exchange with persistent messages.
 *
 * `CompositePublisher` fans one note out to several publishers (the webhook
 * dispatcher AND this one), so wiring both does not mean choosing between them.
 */
import { metrics } from "@opentelemetry/api";
import type { Logger } from "pino";
import type { Note, NotePublisher } from "../notes/notes.js";
import { SERVICE_NAME } from "../service.js";
import { encode, noteCreated, ROUTING_KEY } from "./events.js";
import { declareTopology, EXCHANGE } from "./topology.js";

/** Where a worker publishes. The default is an amqplib exchange; a test passes
 * a fake. `publish` rejects on a broker error — the worker reopens. */
export interface Sink {
  publish(routingKey: string, body: Buffer): Promise<void>;
  close(): Promise<void>;
}

/** Opens a sink for a URL. Injected, so the worker is broker-free in tests. */
export type Opener = (url: string) => Promise<Sink>;

interface Job {
  routingKey: string;
  body: Buffer;
}

export interface RabbitPublisherOptions {
  url: string;
  opener: Opener;
  queueSize?: number;
  reopenBackoffMs?: number;
  reopenBackoffMaxMs?: number;
  /** Replaced in tests. */
  sleep?: (ms: number) => Promise<void>;
  /** Replaced in tests; full-jitter multiplier in [0, 1). */
  jitter?: () => number;
  log?: Logger;
}

/**
 * A `NotePublisher` that forwards to several. One failing to enqueue never
 * stops the others — each is already non-blocking and bounded.
 */
export class CompositePublisher implements NotePublisher {
  constructor(private readonly publishers: NotePublisher[]) {}

  noteCreated(note: Note): void {
    for (const publisher of this.publishers) publisher.noteCreated(note);
  }
}

export class RabbitPublisher implements NotePublisher {
  private readonly queue: Job[] = [];
  private readonly opts: Required<Omit<RabbitPublisherOptions, "log">> & { log?: Logger };
  private readonly outcomes = metrics
    .getMeter(`${SERVICE_NAME}/rabbit`)
    .createCounter("rabbit.publishes", { description: "RabbitMQ publishes by outcome" });
  private started = false;
  private closed = false;
  private aborted = false;
  private sink?: Sink;
  private working?: Promise<void>;

  constructor(opts: RabbitPublisherOptions) {
    this.opts = {
      queueSize: 256,
      reopenBackoffMs: 500,
      reopenBackoffMaxMs: 10_000,
      sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
      jitter: Math.random,
      ...opts,
    };
  }

  start(): void {
    this.started = true;
    // Only kick the worker when there is something to send; draining an empty
    // queue would complete at once and leave `working` resolved, so the next
    // `noteCreated` would not restart it.
    if (this.queue.length > 0) this.working ??= this.drain();
  }

  noteCreated(note: Note): void {
    if (this.closed || this.queue.length >= this.opts.queueSize) {
      this.outcomes.add(1, { outcome: "dropped" });
      this.opts.log?.error({ note_id: note.id }, "rabbit publish queue full or closed, dropped");
      return;
    }
    this.queue.push({ routingKey: ROUTING_KEY, body: encode(noteCreated(note)) });
    if (this.started) this.working ??= this.drain();
  }

  /** Stops taking events and waits for the queue to drain, up to the deadline.
   * Returns false when the deadline cut it; what was still queued is lost. */
  async close(withinMs: number): Promise<boolean> {
    this.closed = true;
    let drained = true;
    if (this.working) {
      let timer: NodeJS.Timeout | undefined;
      const timeout = new Promise<false>((resolve) => {
        timer = setTimeout(() => {
          this.aborted = true;
          resolve(false);
        }, withinMs);
      });
      drained = await Promise.race([this.working.then(() => true as const), timeout]);
      clearTimeout(timer);
    }
    for (let left = this.queue.length; left > 0; left--) {
      this.outcomes.add(1, { outcome: "dropped" });
    }
    this.queue.length = 0;
    if (this.sink) {
      await this.sink.close();
      this.sink = undefined;
    }
    return drained;
  }

  private async drain(): Promise<void> {
    for (let job = this.queue.shift(); job; job = this.queue.shift()) {
      await this.deliver(job);
    }
    this.working = undefined;
  }

  /** Publish `job`, reopening the sink with capped backoff on failure. Retries
   * until it succeeds or `close` aborts it, so a dead broker never hangs. */
  private async deliver(job: Job): Promise<void> {
    let attempt = 0;
    for (;;) {
      if (this.aborted) {
        this.outcomes.add(1, { outcome: "dropped" });
        return;
      }
      try {
        const sink = await this.ensureSink();
        await sink.publish(job.routingKey, job.body);
        this.outcomes.add(1, { outcome: "published" });
        return;
      } catch (err) {
        await this.dropSink(err);
        this.outcomes.add(1, { outcome: "failed" });
        attempt += 1;
        const backoff = Math.min(
          this.opts.reopenBackoffMs * 2 ** (attempt - 1),
          this.opts.reopenBackoffMaxMs,
        );
        await this.opts.sleep(this.opts.jitter() * backoff);
      }
    }
  }

  private async ensureSink(): Promise<Sink> {
    this.sink ??= await this.opts.opener(this.opts.url);
    return this.sink;
  }

  private async dropSink(err: unknown): Promise<void> {
    this.opts.log?.warn(
      { error: err instanceof Error ? err.name : "error" },
      "rabbit sink error, reopening",
    );
    const sink = this.sink;
    this.sink = undefined;
    if (sink) await sink.close().catch(() => {});
  }
}

/**
 * The default `Opener`: a persistent publish to the topic exchange over
 * amqplib, declaring the full topology so the queue exists even if the
 * consumer starts last. Imported lazily so the module needs no broker in tests.
 */
export const amqpOpener: Opener = async (url) => {
  const amqp = await import("amqplib");
  const connection = await amqp.connect(url);
  const channel = await connection.createChannel();
  await declareTopology(channel);
  return {
    async publish(routingKey, body) {
      channel.publish(EXCHANGE, routingKey, body, { persistent: true });
    },
    async close() {
      await connection.close();
    },
  };
};

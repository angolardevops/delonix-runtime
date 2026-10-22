import type { OnApplicationShutdown } from "@nestjs/common";
import { type Counter, metrics } from "@opentelemetry/api";

import type { AppLogger } from "../logging/app-logger";
import type { Note } from "../notes/note";
import { NotePublisher } from "../notes/note.publisher";
import { encode, noteCreated, ROUTING_KEY } from "./events";

/**
 * Where the worker publishes. The default is an AMQP topic exchange (see
 * amqp.ts); a test passes a fake. `publish` throws on a broker error — the
 * worker closes the sink and reopens.
 */
export interface Sink {
  publish(routingKey: string, body: Buffer): Promise<void>;
  close(): Promise<void>;
}

/** Opens a sink. Injected, so the worker is broker-free in tests. */
export type Opener = () => Promise<Sink>;

export interface RabbitPublisherOptions {
  opener: Opener;
  queueSize?: number;
  /** Attempts to publish one note before it is dropped. */
  maxAttempts?: number;
  baseBackoffMs?: number;
  maxBackoffMs?: number;
  /** How long onApplicationShutdown waits for the queue to drain. */
  drainTimeoutMs?: number;
  /** Injected in tests. */
  sleep?: (ms: number) => Promise<void>;
  random?: () => number;
}

interface Job {
  routingKey: string;
  body: Buffer;
}

/**
 * Publishes `note.created` to RabbitMQ without ever blocking a request: a
 * bounded in-memory queue, one worker, and a sink it reopens with capped
 * exponential backoff (full jitter) when the broker errors.
 *
 * Delivery is AT-LEAST-ONCE while the process lives and AT-MOST-ONCE across a
 * restart — the queue is memory, so what is queued when the process dies is
 * lost, and a full queue drops (and logs, and counts) instead of blocking the
 * request that created the note. Reuses the NotePublisher abstraction, so it
 * fans out alongside the webhook dispatcher through CompositePublisher with no
 * change to the use cases. A deployment that must not lose an event needs an
 * outbox written in the same transaction as the note (see docs/adr/0002).
 */
export class RabbitPublisher extends NotePublisher implements OnApplicationShutdown {
  private readonly queue: Job[] = [];
  private readonly queueSize: number;
  private readonly maxAttempts: number;
  private readonly baseBackoffMs: number;
  private readonly maxBackoffMs: number;
  private readonly drainTimeoutMs: number;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly random: () => number;
  private readonly publishes: Counter;
  private sink?: Sink;
  private worker?: Promise<void>;
  private closed = false;

  constructor(
    private readonly options: RabbitPublisherOptions,
    private readonly logger: AppLogger,
  ) {
    super();
    this.queueSize = options.queueSize ?? 256;
    this.maxAttempts = options.maxAttempts ?? 8;
    this.baseBackoffMs = options.baseBackoffMs ?? 500;
    this.maxBackoffMs = options.maxBackoffMs ?? 10_000;
    this.drainTimeoutMs = options.drainTimeoutMs ?? 5_000;
    this.sleep = options.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
    this.random = options.random ?? Math.random;
    this.publishes = metrics.getMeter("rabbit").createCounter("rabbit.publishes", {
      description: "RabbitMQ publishes by outcome",
    });
  }

  /** NotePublisher: queues and returns; never waits on the broker. */
  noteCreated(note: Note): void {
    if (this.closed) {
      this.count("dropped");
      this.logger.warn("rabbit publisher closed, event dropped", { note_id: note.id });
      return;
    }
    if (this.queue.length >= this.queueSize) {
      this.count("dropped");
      this.logger.error("rabbit publish queue full, event dropped", { note_id: note.id });
      return;
    }
    this.queue.push({ routingKey: ROUTING_KEY, body: encode(noteCreated(note)) });
    this.startWorker();
  }

  /** Events waiting to be published (for tests and diagnostics). */
  get pending(): number {
    return this.queue.length;
  }

  /**
   * Stops taking events and waits for the queue to drain, bounded by `within`.
   * Returns whether it drained in time; what is still queued after is counted
   * as dropped. Called by Nest on shutdown and, directly, by the tests.
   */
  async close(within: number = this.drainTimeoutMs): Promise<boolean> {
    this.closed = true;
    const timeout = new Promise<"timeout">((resolve) => {
      setTimeout(() => resolve("timeout"), within).unref?.();
    });
    while (this.worker !== undefined) {
      const outcome = await Promise.race([this.worker.then(() => "done" as const), timeout]);
      if (outcome === "timeout") {
        break;
      }
    }
    const drained = this.queue.length === 0 && this.worker === undefined;
    if (!drained) {
      const left = this.queue.length;
      this.queue.length = 0;
      for (let i = 0; i < left; i++) {
        this.count("dropped");
      }
      this.logger.error("rabbit queue not drained before the deadline", { dropped: left });
    }
    if (this.sink !== undefined) {
      await this.sink.close();
      this.sink = undefined;
    }
    return drained;
  }

  onApplicationShutdown(): Promise<void> {
    return this.close().then(() => undefined);
  }

  private startWorker(): void {
    if (this.worker !== undefined) {
      return;
    }
    this.worker = this.drain().finally(() => {
      this.worker = undefined;
      // An event queued between the last shift and this callback.
      if (this.queue.length > 0 && !this.closed) {
        this.startWorker();
      }
    });
  }

  private async drain(): Promise<void> {
    for (let job = this.queue.shift(); job !== undefined; job = this.queue.shift()) {
      await this.deliver(job);
    }
  }

  private async deliver(job: Job): Promise<void> {
    for (let attempt = 1; ; attempt++) {
      let failure: unknown;
      try {
        const sink = await this.ensureSink();
        await sink.publish(job.routingKey, job.body);
        this.count("published");
        return;
      } catch (error) {
        failure = error;
        await this.dropSink(error);
      }
      this.count("failed");
      if (attempt >= this.maxAttempts) {
        this.logger.error("rabbit publish failed, event dropped", {
          error: failure instanceof Error ? failure.name : "unknown",
        });
        return;
      }
      // Full jitter: uniform in [0, base * 2^(attempt-1)), capped.
      const ceiling = Math.min(this.baseBackoffMs * 2 ** (attempt - 1), this.maxBackoffMs);
      await this.sleep(Math.floor(this.random() * ceiling));
    }
  }

  private async ensureSink(): Promise<Sink> {
    if (this.sink === undefined) {
      this.sink = await this.options.opener();
    }
    return this.sink;
  }

  private async dropSink(error: unknown): Promise<void> {
    this.logger.warn("rabbit sink error, reopening", {
      error: error instanceof Error ? error.name : "unknown",
    });
    if (this.sink !== undefined) {
      const sink = this.sink;
      this.sink = undefined;
      try {
        await sink.close();
      } catch {
        // Already broken; nothing to do.
      }
    }
  }

  private count(outcome: "published" | "failed" | "dropped"): void {
    this.publishes.add(1, { outcome });
  }
}

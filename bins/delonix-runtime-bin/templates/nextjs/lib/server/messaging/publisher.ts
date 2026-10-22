// Publish `note.created` to RabbitMQ without ever blocking a request.
//
// RabbitPublisher implements notes.NotePublisher: noteCreated only enqueues
// (and returns), a single worker drains the queue to the broker. The queue is
// bounded and DROPS — never blocks — when full, exactly like the webhook
// dispatcher. The AMQP specifics live behind Sink, injected, so the
// queue/worker/backoff logic is tested with no broker; the default sink opens
// an amqplib topic exchange with persistent messages.
//
// CompositePublisher fans one note out to several publishers (the webhook
// dispatcher AND this one), so wiring both does not mean choosing between them.
import "server-only";
import type { Note, NotePublisher } from "@/lib/notes/notes";
import { EXCHANGE, ROUTING_KEY, encode, noteCreated } from "./events";

/**
 * Where a worker publishes. The default is an amqplib exchange; a test passes
 * a fake. `publish` rejects on a broker error — the worker reopens.
 */
export interface Sink {
  publish(routingKey: string, body: string): Promise<void>;
  close(): Promise<void>;
}

/** Opens a sink for a URL. Injected, so the worker is broker-free in tests. */
export type Opener = (url: string) => Promise<Sink>;

/**
 * The default opener: a confirm channel on a durable topic exchange, so a
 * publish only resolves once the broker has acked it. `amqplib` is imported
 * lazily — a project that never sets RABBITMQ_URL never loads it and the unit
 * tests inject a fake opener instead. Exercised against the compose `rabbitmq`
 * service by the CI integration run, not by `pnpm test`.
 */
export async function amqpOpener(url: string): Promise<Sink> {
  const amqp = await import("amqplib");
  const connection = await amqp.connect(url);
  const channel = await connection.createConfirmChannel();
  await channel.assertExchange(EXCHANGE, "topic", { durable: true });
  return {
    publish(routingKey, body) {
      return new Promise<void>((resolve, reject) => {
        channel.publish(EXCHANGE, routingKey, Buffer.from(body), { persistent: true }, (err) =>
          err ? reject(err) : resolve(),
        );
      });
    },
    async close() {
      await connection.close();
    },
  };
}

interface Job {
  routingKey: string;
  body: string;
}

/**
 * A NotePublisher that forwards to several. One failing to enqueue never stops
 * the others — each is already non-blocking and bounded.
 */
export class CompositePublisher implements NotePublisher {
  constructor(private readonly publishers: NotePublisher[]) {}

  noteCreated(note: Note): void {
    for (const publisher of this.publishers) publisher.noteCreated(note);
  }
}

export interface RabbitPublisherOptions {
  url: string;
  opener: Opener;
  queueSize?: number;
  reopenBackoff?: number;
  reopenBackoffMax?: number;
  sleep?: (seconds: number) => Promise<void>;
  jitter?: () => number;
}

export class RabbitPublisher implements NotePublisher {
  private readonly url: string;
  private readonly opener: Opener;
  private readonly queueSize: number;
  private readonly reopenBackoff: number;
  private readonly reopenBackoffMax: number;
  private readonly sleep: (seconds: number) => Promise<void>;
  private readonly jitter: () => number;

  private readonly queue: Job[] = [];
  private closed = false;
  private wake?: () => void;
  private sink?: Sink;
  private worker?: Promise<void>;
  readonly counts = { published: 0, failed: 0, dropped: 0 };

  constructor(options: RabbitPublisherOptions) {
    this.url = options.url;
    this.opener = options.opener;
    this.queueSize = options.queueSize ?? 256;
    this.reopenBackoff = options.reopenBackoff ?? 0.5;
    this.reopenBackoffMax = options.reopenBackoffMax ?? 10.0;
    this.sleep =
      options.sleep ?? ((seconds) => new Promise((r) => setTimeout(r, seconds * 1000)));
    this.jitter = options.jitter ?? Math.random;
  }

  start(): void {
    this.worker = this.run();
  }

  noteCreated(note: Note): void {
    if (this.closed) {
      this.counts.dropped++;
      return;
    }
    if (this.queue.length >= this.queueSize) {
      this.counts.dropped++; // full: drop, never block the caller
      return;
    }
    this.queue.push({ routingKey: ROUTING_KEY, body: encode(noteCreated(note)) });
    this.wake?.();
  }

  /** Drain within `seconds`, then stop. Returns whether the queue was drained. */
  async close(seconds: number): Promise<boolean> {
    this.closed = true;
    this.wake?.();
    let drained = true;
    if (this.worker !== undefined) {
      const timeout = new Promise<boolean>((resolve) => setTimeout(() => resolve(false), seconds * 1000));
      drained = await Promise.race([this.worker.then(() => true), timeout]);
      if (!drained) {
        this.counts.dropped += this.queue.length;
        this.queue.length = 0;
        this.wake?.(); // let the worker observe the emptied queue and exit
      }
    }
    if (this.sink !== undefined) {
      await this.sink.close();
      this.sink = undefined;
    }
    return drained;
  }

  private async run(): Promise<void> {
    for (;;) {
      if (this.queue.length === 0) {
        if (this.closed) return;
        await new Promise<void>((resolve) => {
          this.wake = resolve;
          if (this.closed || this.queue.length > 0) resolve(); // no lost wake-up
        });
        this.wake = undefined;
        continue;
      }
      await this.publish(this.queue[0]!);
      this.queue.shift();
    }
  }

  /** Publish `job`, reopening the sink with capped backoff on failure. */
  private async publish(job: Job): Promise<void> {
    let attempt = 0;
    for (;;) {
      const sink = await this.ensureSink();
      try {
        await sink.publish(job.routingKey, job.body);
      } catch {
        await this.dropSink();
        this.counts.failed++;
        attempt++;
        const backoff = Math.min(this.reopenBackoff * 2 ** (attempt - 1), this.reopenBackoffMax);
        await this.sleep(this.jitter() * backoff);
        continue;
      }
      this.counts.published++;
      return;
    }
  }

  private async ensureSink(): Promise<Sink> {
    if (this.sink === undefined) this.sink = await this.opener(this.url);
    return this.sink;
  }

  private async dropSink(): Promise<void> {
    if (this.sink !== undefined) {
      try {
        await this.sink.close();
      } catch {
        // already broken; nothing to salvage
      }
      this.sink = undefined;
    }
  }
}

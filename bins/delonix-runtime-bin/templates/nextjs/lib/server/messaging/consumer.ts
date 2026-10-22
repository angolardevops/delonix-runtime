// Consume `note.created` from RabbitMQ, with manual acks and a bounded retry.
//
// The per-message decision — acknowledge, retry, or dead-letter — lives in
// RabbitConsumer.handle, which talks to an Incoming message (ack / retry /
// deadLetter), so it is tested with no broker. The amqplib connect-and-consume
// loop (the thin glue that wraps each real delivery as an Incoming) is left to
// the developer and exercised by the CI integration run against the compose
// `rabbitmq` service, not here.
//
// Rules:
//   - the handler returns          -> ack (processed; at-least-once, so make it
//                                     idempotent);
//   - the body is not a valid event-> dead-letter at once (a retry cannot fix it);
//   - the handler throws           -> retry up to maxRetries (republished with an
//                                     incremented x-retry header), then
//                                     dead-letter — a poison message never loops
//                                     for ever.
import "server-only";
import { decode } from "./events";

/** Processes one decoded event. Rejecting means "transient failure, retry". */
export type Handler = (event: { type: string; [key: string]: unknown }) => Promise<void>;

/** One delivery, abstracted from amqplib so `handle` is broker-free. */
export interface Incoming {
  readonly body: string;
  /** The `x-retry` header, 0 on first delivery. */
  readonly retryCount: number;
  ack(): Promise<void>;
  /** Reject without requeue — routed to the dead-letter exchange. */
  deadLetter(): Promise<void>;
  /** Republish with `x-retry` incremented after `delay` seconds, then ack the original. */
  retry(delay: number): Promise<void>;
}

export interface RabbitConsumerOptions {
  handler: Handler;
  maxRetries?: number;
  retryDelay?: number;
}

export class RabbitConsumer {
  private readonly handler: Handler;
  private readonly maxRetries: number;
  private readonly retryDelay: number;
  readonly counts = { acked: 0, poison: 0, retried: 0, deadLettered: 0 };

  constructor(options: RabbitConsumerOptions) {
    this.handler = options.handler;
    this.maxRetries = options.maxRetries ?? 3;
    this.retryDelay = options.retryDelay ?? 1.0;
  }

  /**
   * Decide the fate of one delivery. Never throws: every path ends in an ack,
   * a dead-letter, or a retry, so a delivery is never left unanswered.
   */
  async handle(message: Incoming): Promise<void> {
    let event: { type: string; [key: string]: unknown };
    try {
      event = decode(message.body);
    } catch {
      this.counts.poison++;
      await message.deadLetter();
      return;
    }
    try {
      await this.handler(event);
    } catch {
      if (message.retryCount >= this.maxRetries) {
        this.counts.deadLettered++;
        await message.deadLetter();
      } else {
        this.counts.retried++;
        await message.retry(this.retryDelay);
      }
      return;
    }
    this.counts.acked++;
    await message.ack();
  }
}

/** The default handler: records receipt. Replace it with the real side-effect. */
export async function logHandler(event: {
  type: string;
  [key: string]: unknown;
}): Promise<void> {
  void event;
}

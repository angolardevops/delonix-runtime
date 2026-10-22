/**
 * Consume `note.created` from RabbitMQ, with manual acks and a bounded retry.
 *
 * The per-message decision — acknowledge, retry, or dead-letter — lives in
 * `RabbitConsumer.handle`, which talks to an `Incoming` message (ack / retry /
 * deadLetter), so it is tested with no broker. The amqplib connect-and-consume
 * loop in `start` is the thin glue that wraps each real delivery.
 *
 * Rules:
 * - the handler resolves  -> ack (processed; at-least-once, so be idempotent);
 * - the body is not a valid event -> dead-letter at once (a retry cannot fix);
 * - the handler rejects -> retry up to `maxRetries` (republished with an
 *   incremented `x-retry` header), then dead-letter — a poison message can
 *   never loop for ever.
 *
 * Delivery is AT-LEAST-ONCE: a crash mid-handling redelivers. It is NOT
 * exactly-once — the event carries the note id as a natural key to deduplicate.
 */
import { metrics } from "@opentelemetry/api";
import type { Logger } from "pino";
import { SERVICE_NAME } from "../service.js";
import { decode, type NoteCreatedEvent, ROUTING_KEY } from "./events.js";
import { declareTopology, EXCHANGE, QUEUE } from "./topology.js";

/** Processes one decoded event. Rejecting means "transient failure, retry". */
export type Handler = (event: NoteCreatedEvent) => Promise<void>;

/** One delivery, abstracted from amqplib so `handle` is broker-free. */
export interface Incoming {
  readonly body: Buffer;
  /** The `x-retry` header, 0 on first delivery. */
  readonly retryCount: number;
  ack(): Promise<void>;
  /** Reject without requeue — routed to the dead-letter exchange. */
  deadLetter(): Promise<void>;
  /** Republish with `x-retry` incremented, then ack the original. */
  retry(delay: number): Promise<void>;
}

export interface RabbitConsumerOptions {
  url: string;
  handler?: Handler;
  maxRetries?: number;
  /** Seconds. */
  retryDelay?: number;
  prefetch?: number;
  log?: Logger;
}

export class RabbitConsumer {
  private readonly url: string;
  private readonly handler: Handler;
  private readonly maxRetries: number;
  private readonly retryDelay: number;
  private readonly prefetch: number;
  private readonly log?: Logger;
  private readonly outcomes = metrics
    .getMeter(`${SERVICE_NAME}/rabbit`)
    .createCounter("rabbit.consumed", { description: "RabbitMQ messages by outcome" });
  private closer?: () => Promise<void>;

  constructor(opts: RabbitConsumerOptions) {
    this.url = opts.url;
    this.handler = opts.handler ?? ((event) => this.logHandler(event));
    this.maxRetries = opts.maxRetries ?? 3;
    this.retryDelay = opts.retryDelay ?? 1;
    this.prefetch = opts.prefetch ?? 16;
    this.log = opts.log;
  }

  /** The default handler: records receipt. Replace it with the real
   * side-effect (a projection, an index write, a notification). */
  private async logHandler(event: NoteCreatedEvent): Promise<void> {
    this.log?.info({ note_id: event.data?.id }, "note.created consumed");
  }

  /** Decide the fate of one delivery. Never rejects: every path ends in an
   * ack, a dead-letter, or a retry, so a delivery is never left unanswered. */
  async handle(message: Incoming): Promise<void> {
    let event: NoteCreatedEvent;
    try {
      event = decode(message.body);
    } catch (err) {
      this.outcomes.add(1, { outcome: "poison" });
      this.log?.warn(
        { error: err instanceof Error ? err.message : String(err) },
        "dead-lettering an invalid message",
      );
      await message.deadLetter();
      return;
    }
    try {
      await this.handler(event);
    } catch (err) {
      const name = err instanceof Error ? err.name : "error";
      if (message.retryCount >= this.maxRetries) {
        this.outcomes.add(1, { outcome: "dead_lettered" });
        this.log?.error(
          { error: name, retries: message.retryCount },
          "handler failed, retries exhausted, dead-lettering",
        );
        await message.deadLetter();
      } else {
        this.outcomes.add(1, { outcome: "retried" });
        this.log?.warn(
          { error: name, attempt: message.retryCount + 1 },
          "handler failed, retrying",
        );
        await message.retry(this.retryDelay);
      }
      return;
    }
    this.outcomes.add(1, { outcome: "acked" });
    await message.ack();
  }

  // ---- amqplib glue (not unit-tested; exercised by the CI integration run) ----

  /** Connect and consume. A down broker is logged, not fatal: the app serves
   * without a consumer and the next deploy, or a broker restart, reconnects. */
  async start(): Promise<void> {
    let amqp: typeof import("amqplib");
    try {
      amqp = await import("amqplib");
    } catch (err) {
      this.log?.warn(
        { error: err instanceof Error ? err.name : "error" },
        "amqplib not installed; the consumer is off",
      );
      return;
    }
    let connection: Awaited<ReturnType<typeof amqp.connect>>;
    try {
      connection = await amqp.connect(this.url);
    } catch (err) {
      this.log?.warn(
        { error: err instanceof Error ? err.name : "error" },
        "rabbit consumer could not connect; messages will not be consumed",
      );
      return;
    }
    this.closer = () => connection.close();
    const channel = await connection.createChannel();
    await channel.prefetch(this.prefetch);
    await declareTopology(channel);

    await channel.consume(QUEUE, (msg) => {
      if (msg === null) return;
      void this.handle({
        body: msg.content,
        retryCount: Number(msg.properties.headers?.["x-retry"] ?? 0) || 0,
        ack: async () => {
          channel.ack(msg);
        },
        deadLetter: async () => {
          channel.reject(msg, false);
        },
        retry: async (delay) => {
          await new Promise((resolve) => setTimeout(resolve, delay * 1000));
          channel.publish(EXCHANGE, ROUTING_KEY, msg.content, {
            persistent: true,
            headers: { "x-retry": (Number(msg.properties.headers?.["x-retry"] ?? 0) || 0) + 1 },
          });
          channel.ack(msg);
        },
      });
    });
    this.log?.info("rabbit consumer ready");
  }

  async close(): Promise<void> {
    if (this.closer) {
      await this.closer().catch(() => {});
      this.closer = undefined;
    }
  }
}

import type { ChannelModel, ConfirmChannel } from "amqplib";

import type { Opener, Sink } from "./rabbit-publisher";

/** The durable topic exchange `note.created` is published to. */
export const EXCHANGE = "notes";

/**
 * The default {@link Opener}: a persistent publish to a durable topic exchange,
 * over a CONFIRM channel — `publish` only resolves once the broker has acked,
 * so the worker learns of a failure and reopens. The `amqplib` driver is
 * required lazily so a run without RABBITMQ_URL never loads it.
 */
export function amqpOpener(url: string): Opener {
  return async (): Promise<Sink> => {
    const amqp = await import("amqplib");
    const connection: ChannelModel = await amqp.connect(url);
    const channel: ConfirmChannel = await connection.createConfirmChannel();
    await channel.assertExchange(EXCHANGE, "topic", { durable: true });
    return new AmqpSink(connection, channel);
  };
}

class AmqpSink implements Sink {
  constructor(
    private readonly connection: ChannelModel,
    private readonly channel: ConfirmChannel,
  ) {}

  publish(routingKey: string, body: Buffer): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      this.channel.publish(EXCHANGE, routingKey, body, { persistent: true }, (error) =>
        error === null ? resolve() : reject(error),
      );
    });
  }

  async close(): Promise<void> {
    await this.connection.close();
  }
}

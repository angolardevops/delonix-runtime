/**
 * The AMQP topology, declared the same way by the publisher and the consumer.
 *
 * One durable topic exchange carries `note.*`. The work queue dead-letters to
 * a separate exchange whose only binding is the dead-letter queue, so a
 * message the consumer rejects (poison, or retries exhausted) is kept for
 * inspection instead of being dropped or requeued into a loop. Declarations
 * are idempotent: whoever connects first creates them, the other finds them.
 *
 * `amqplib` is imported lazily by the callers (publisher/consumer glue); the
 * `Channel` here is its `amqplib.Channel`, kept as a structural type so this
 * module needs no value import of the package.
 */
export const EXCHANGE = "notes.events";
export const QUEUE = "notes.created";
export const BINDING_KEY = "note.*";

export const DLX = "notes.events.dlx";
export const DLQ = "notes.created.dead";

/** The slice of an amqplib `Channel` the topology needs. */
export interface TopologyChannel {
  assertExchange(exchange: string, type: string, options?: object): Promise<unknown>;
  assertQueue(queue: string, options?: object): Promise<unknown>;
  bindQueue(queue: string, source: string, pattern: string): Promise<unknown>;
}

/**
 * Declare the exchange, the dead-letter path and the work queue, and bind
 * them. Idempotent; safe to call from either side.
 */
export async function declareTopology(channel: TopologyChannel): Promise<void> {
  await channel.assertExchange(EXCHANGE, "topic", { durable: true });
  await channel.assertExchange(DLX, "fanout", { durable: true });
  await channel.assertQueue(DLQ, { durable: true });
  await channel.bindQueue(DLQ, DLX, "");

  await channel.assertQueue(QUEUE, { durable: true, arguments: { "x-dead-letter-exchange": DLX } });
  await channel.bindQueue(QUEUE, EXCHANGE, BINDING_KEY);
}

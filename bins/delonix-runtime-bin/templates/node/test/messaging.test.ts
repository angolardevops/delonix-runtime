/**
 * Messaging without a broker: the wire format, the fan-out publisher, the
 * bounded publish queue, and the consumer's ack / retry / dead-letter decision.
 * The amqplib glue (`start`, the amqplib opener) is exercised by the CI
 * integration run against the compose `rabbitmq` service, not here.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  RabbitConsumer,
  type Handler,
  type Incoming,
  type RabbitConsumerOptions,
} from "../src/messaging/consumer.js";
import { decode, encode, noteCreated, ROUTING_KEY } from "../src/messaging/events.js";
import {
  CompositePublisher,
  RabbitPublisher,
  type Opener,
  type Sink,
} from "../src/messaging/publisher.js";
import type { Note, NotePublisher } from "../src/notes/notes.js";

function aNote(id = "n1"): Note {
  return { id, title: "t", body: "b", created_at: "2026-01-01T00:00:00.000Z" };
}

const immediate = async (): Promise<void> => {};

// ---- events ----

test("the event round-trips and names the note", () => {
  const event = noteCreated(aNote());
  assert.equal(event.type, "note.created");
  assert.equal(event.data.id, "n1");
  assert.deepEqual(decode(encode(event)), event);
});

test("decode rejects a poison body", () => {
  for (const body of ["not json", "123", '{"no":"type"}', '{"type":5}']) {
    assert.throws(() => decode(Buffer.from(body)));
  }
});

// ---- CompositePublisher ----

class RecordingPublisher implements NotePublisher {
  seen: string[] = [];
  noteCreated(note: Note): void {
    this.seen.push(note.id);
  }
}

test("the composite forwards to every publisher", () => {
  const a = new RecordingPublisher();
  const b = new RecordingPublisher();
  new CompositePublisher([a, b]).noteCreated(aNote("x"));
  assert.deepEqual(a.seen, ["x"]);
  assert.deepEqual(b.seen, ["x"]);
});

// ---- RabbitPublisher (fake sink) ----

class FakeSink implements Sink {
  published: { routingKey: string; body: Buffer }[] = [];
  closed = false;
  constructor(private failTimes = 0) {}
  async publish(routingKey: string, body: Buffer): Promise<void> {
    if (this.failTimes > 0) {
      this.failTimes -= 1;
      throw new Error("broker down");
    }
    this.published.push({ routingKey, body });
  }
  async close(): Promise<void> {
    this.closed = true;
  }
}

function openerFor(...sinks: FakeSink[]): Opener {
  const seq = sinks[Symbol.iterator]();
  return async () => {
    const next = seq.next();
    if (next.done) throw new Error("no more sinks");
    return next.value;
  };
}

test("the publisher delivers queued notes", async () => {
  const sink = new FakeSink();
  const pub = new RabbitPublisher({ url: "amqp://x", opener: openerFor(sink) });
  pub.start();
  pub.noteCreated(aNote("n1"));
  pub.noteCreated(aNote("n2"));
  assert.equal(await pub.close(1000), true);
  assert.deepEqual(
    sink.published.map((p) => p.routingKey),
    [ROUTING_KEY, ROUTING_KEY],
  );
  assert.equal(decode(sink.published[0]!.body).data.id, "n1");
  assert.ok(sink.closed);
});

test("the publisher reopens the sink after a failure", async () => {
  const broken = new FakeSink(1);
  const good = new FakeSink();
  const pub = new RabbitPublisher({
    url: "amqp://x",
    opener: openerFor(broken, good),
    sleep: immediate,
    jitter: () => 0,
  });
  pub.start();
  pub.noteCreated(aNote("n1"));
  assert.equal(await pub.close(1000), true);
  assert.equal(good.published.length, 1); // reopened and redelivered on the new sink
});

test("the publisher drops when the queue is full", async () => {
  const sink = new FakeSink();
  const pub = new RabbitPublisher({ url: "amqp://x", opener: openerFor(sink), queueSize: 1 });
  // No worker running yet: the queue fills and extra notes drop, never block.
  pub.noteCreated(aNote("n1"));
  pub.noteCreated(aNote("n2"));
  pub.noteCreated(aNote("n3"));
  pub.start();
  await pub.close(1000);
  assert.equal(sink.published.length, 1);
});

// ---- RabbitConsumer.handle (fake delivery) ----

class FakeIncoming implements Incoming {
  acked = false;
  deadLettered = false;
  retriedWith: number | null = null;
  constructor(
    readonly body: Buffer,
    readonly retryCount = 0,
  ) {}
  async ack(): Promise<void> {
    this.acked = true;
  }
  async deadLetter(): Promise<void> {
    this.deadLettered = true;
  }
  async retry(delay: number): Promise<void> {
    this.retriedWith = delay;
  }
}

function consumer(handler: Handler, extra: Partial<RabbitConsumerOptions> = {}): RabbitConsumer {
  return new RabbitConsumer({ url: "amqp://x", handler, ...extra });
}

const ok: Handler = async () => {};
const boom: Handler = async () => {
  throw new Error("transient");
};

test("handle acks a processed message", async () => {
  const msg = new FakeIncoming(encode(noteCreated(aNote())));
  await consumer(ok).handle(msg);
  assert.ok(msg.acked);
  assert.ok(!msg.deadLettered);
});

test("handle dead-letters a poison message at once", async () => {
  const msg = new FakeIncoming(Buffer.from("not an event"));
  await consumer(ok).handle(msg);
  assert.ok(msg.deadLettered);
  assert.ok(!msg.acked);
  assert.equal(msg.retriedWith, null);
});

test("handle retries a transient failure", async () => {
  const msg = new FakeIncoming(encode(noteCreated(aNote())), 0);
  await consumer(boom, { maxRetries: 3, retryDelay: 0.25 }).handle(msg);
  assert.equal(msg.retriedWith, 0.25);
  assert.ok(!msg.deadLettered);
  assert.ok(!msg.acked);
});

test("handle dead-letters when retries are exhausted", async () => {
  const msg = new FakeIncoming(encode(noteCreated(aNote())), 3);
  await consumer(boom, { maxRetries: 3 }).handle(msg);
  assert.ok(msg.deadLettered);
  assert.ok(!msg.acked);
  assert.equal(msg.retriedWith, null);
});

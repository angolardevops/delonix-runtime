import { describe, expect, it } from "vitest";
import type { Note, NotePublisher } from "@/lib/notes/notes";
import { ROUTING_KEY, decode, encode, noteCreated } from "@/lib/server/messaging/events";
import {
  CompositePublisher,
  RabbitPublisher,
  type Sink,
} from "@/lib/server/messaging/publisher";
import { RabbitConsumer, type Incoming } from "@/lib/server/messaging/consumer";

function aNote(id = "n1"): Note {
  return { id, title: "t", body: "b", created_at: "2026-01-01T00:00:00.000Z" };
}

// ---- events ----

describe("events", () => {
  it("round-trips and names the note", () => {
    const event = noteCreated(aNote());
    expect(event.type).toBe("note.created");
    expect(event.data.id).toBe("n1");
    expect(decode(encode(event))).toEqual(event);
  });

  it.each(["not json", "123", '{"no":"type"}', '{"type":5}', "[1,2]"])(
    "rejects the poison body %j",
    (body) => {
      expect(() => decode(body)).toThrow();
    },
  );
});

// ---- CompositePublisher ----

class RecordingPublisher implements NotePublisher {
  readonly seen: string[] = [];
  noteCreated(note: Note): void {
    this.seen.push(note.id);
  }
}

describe("CompositePublisher", () => {
  it("forwards to every publisher", () => {
    const a = new RecordingPublisher();
    const b = new RecordingPublisher();
    new CompositePublisher([a, b]).noteCreated(aNote("x"));
    expect(a.seen).toEqual(["x"]);
    expect(b.seen).toEqual(["x"]);
  });
});

// ---- RabbitPublisher (fake sink) ----

class FakeSink implements Sink {
  readonly published: Array<[string, string]> = [];
  closed = false;
  constructor(private failTimes = 0) {}

  async publish(routingKey: string, body: string): Promise<void> {
    if (this.failTimes > 0) {
      this.failTimes--;
      throw new Error("broker down");
    }
    this.published.push([routingKey, body]);
  }

  async close(): Promise<void> {
    this.closed = true;
  }
}

function openerFor(...sinks: Sink[]): (url: string) => Promise<Sink> {
  const seq = sinks[Symbol.iterator]();
  return () => Promise.resolve(seq.next().value as Sink);
}

const immediate = () => Promise.resolve();

describe("RabbitPublisher", () => {
  it("delivers queued notes in order", async () => {
    const sink = new FakeSink();
    const pub = new RabbitPublisher({ url: "amqp://x", opener: openerFor(sink) });
    pub.start();
    pub.noteCreated(aNote("n1"));
    pub.noteCreated(aNote("n2"));
    expect(await pub.close(1)).toBe(true);
    expect(sink.published.map(([rk]) => rk)).toEqual([ROUTING_KEY, ROUTING_KEY]);
    expect(decode(sink.published[0]![1]).data).toMatchObject({ id: "n1" });
    expect(sink.closed).toBe(true);
  });

  it("reopens the sink after a failure", async () => {
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
    expect(await pub.close(1)).toBe(true);
    expect(good.published).toHaveLength(1); // reopened and redelivered
  });

  it("drops when the queue is full, never blocks", async () => {
    const sink = new FakeSink();
    const pub = new RabbitPublisher({ url: "amqp://x", opener: openerFor(sink), queueSize: 1 });
    // No worker running yet: the queue fills and extra notes are dropped.
    pub.noteCreated(aNote("n1"));
    pub.noteCreated(aNote("n2"));
    pub.noteCreated(aNote("n3"));
    pub.start();
    await pub.close(1);
    expect(sink.published).toHaveLength(1);
    expect(pub.counts.dropped).toBe(2);
  });
});

// ---- RabbitConsumer.handle (fake delivery) ----

class FakeIncoming implements Incoming {
  acked = false;
  deadLettered = false;
  retriedWith: number | undefined;
  constructor(
    readonly body: string,
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

function consumer(
  handler: (event: { type: string; [key: string]: unknown }) => Promise<void>,
  kw: { maxRetries?: number; retryDelay?: number } = {},
): RabbitConsumer {
  return new RabbitConsumer({ handler, ...kw });
}

const ok = async () => {};
const boom = async () => {
  throw new Error("transient");
};

describe("RabbitConsumer.handle", () => {
  it("acks a processed message", async () => {
    const msg = new FakeIncoming(encode(noteCreated(aNote())));
    await consumer(ok).handle(msg);
    expect(msg.acked).toBe(true);
    expect(msg.deadLettered).toBe(false);
  });

  it("dead-letters a poison message at once", async () => {
    const msg = new FakeIncoming("not an event");
    await consumer(ok).handle(msg);
    expect(msg.deadLettered).toBe(true);
    expect(msg.acked).toBe(false);
    expect(msg.retriedWith).toBeUndefined();
  });

  it("retries a transient failure", async () => {
    const msg = new FakeIncoming(encode(noteCreated(aNote())), 0);
    await consumer(boom, { maxRetries: 3, retryDelay: 0.25 }).handle(msg);
    expect(msg.retriedWith).toBe(0.25);
    expect(msg.deadLettered).toBe(false);
    expect(msg.acked).toBe(false);
  });

  it("dead-letters when retries are exhausted", async () => {
    const msg = new FakeIncoming(encode(noteCreated(aNote())), 3);
    await consumer(boom, { maxRetries: 3 }).handle(msg);
    expect(msg.deadLettered).toBe(true);
    expect(msg.acked).toBe(false);
    expect(msg.retriedWith).toBeUndefined();
  });
});

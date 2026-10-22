import { AppLogger } from "../logging/app-logger";
import type { Note } from "../notes/note";
import { NotePublisher } from "../notes/note.publisher";
import { CompositePublisher } from "./composite-publisher";
import { decode, encode, noteCreated, ROUTING_KEY } from "./events";
import { RabbitPublisher, type Sink } from "./rabbit-publisher";

const note = (id = "n1"): Note => ({ id, title: "t", body: "b", created_at: "2026-01-01T00:00:00.000Z" });

const silentLogger = (): AppLogger =>
  AppLogger.create({
    level: "error",
    service: "test",
    version: "test",
    environment: "test",
    destination: { write: () => undefined },
  });

// ---- events ----

describe("events", () => {
  it("round-trips and names the note", () => {
    const event = noteCreated(note());
    expect(event.type).toBe("note.created");
    expect(event.data.id).toBe("n1");
    expect(decode(encode(event))).toEqual({ type: "note.created", data: note() });
  });

  it.each([Buffer.from("not json"), Buffer.from("123"), Buffer.from('{"no":"type"}'), Buffer.from('{"type":5}')])(
    "rejects a poison body %p",
    (body) => {
      expect(() => decode(body)).toThrow();
    },
  );
});

// ---- CompositePublisher ----

class RecordingPublisher extends NotePublisher {
  readonly seen: string[] = [];
  shutdownCalls = 0;

  noteCreated(n: Note): void {
    this.seen.push(n.id);
  }

  onApplicationShutdown(): Promise<void> {
    this.shutdownCalls++;
    return Promise.resolve();
  }
}

describe("CompositePublisher", () => {
  it("forwards one note to every publisher", () => {
    const a = new RecordingPublisher();
    const b = new RecordingPublisher();
    new CompositePublisher([a, b]).noteCreated(note("x"));
    expect(a.seen).toEqual(["x"]);
    expect(b.seen).toEqual(["x"]);
  });

  it("drains every delegate that has a shutdown hook", async () => {
    const a = new RecordingPublisher();
    const b = new RecordingPublisher();
    await new CompositePublisher([a, b]).onApplicationShutdown("SIGTERM");
    expect(a.shutdownCalls).toBe(1);
    expect(b.shutdownCalls).toBe(1);
  });
});

// ---- RabbitPublisher (fake sink) ----

class FakeSink implements Sink {
  readonly published: [string, Buffer][] = [];
  closed = false;

  constructor(private failTimes = 0) {}

  publish(routingKey: string, body: Buffer): Promise<void> {
    if (this.failTimes > 0) {
      this.failTimes--;
      return Promise.reject(new Error("broker down"));
    }
    this.published.push([routingKey, body]);
    return Promise.resolve();
  }

  close(): Promise<void> {
    this.closed = true;
    return Promise.resolve();
  }
}

/** An opener handing out the given sinks in order. */
function openerFor(...sinks: Sink[]): () => Promise<Sink> {
  const queue = [...sinks];
  return () => {
    const next = queue.shift();
    if (next === undefined) {
      throw new Error("no sink left");
    }
    return Promise.resolve(next);
  };
}

const immediate = (): Promise<void> => Promise.resolve();

describe("RabbitPublisher", () => {
  it("delivers queued notes and closes the sink on shutdown", async () => {
    const sink = new FakeSink();
    const pub = new RabbitPublisher({ opener: openerFor(sink) }, silentLogger());
    pub.noteCreated(note("n1"));
    pub.noteCreated(note("n2"));
    expect(await pub.close(1000)).toBe(true);
    expect(sink.published.map(([rk]) => rk)).toEqual([ROUTING_KEY, ROUTING_KEY]);
    expect(decode(sink.published[0][1]).data).toMatchObject({ id: "n1" });
    expect(sink.closed).toBe(true);
  });

  it("reopens the sink after a failure and redelivers", async () => {
    const broken = new FakeSink(1);
    const good = new FakeSink();
    const pub = new RabbitPublisher(
      { opener: openerFor(broken, good), sleep: immediate, random: () => 0 },
      silentLogger(),
    );
    pub.noteCreated(note("n1"));
    expect(await pub.close(1000)).toBe(true);
    expect(broken.closed).toBe(true); // reopened
    expect(good.published).toHaveLength(1);
  });

  it("drops instead of blocking when the queue is full", async () => {
    const sink = new FakeSink();
    const lines: string[] = [];
    const logger = AppLogger.create({
      level: "error",
      service: "test",
      version: "test",
      environment: "test",
      destination: { write: (line: string) => void lines.push(line) },
    });
    // queueSize 2: n1 is taken by the worker at once, n2/n3 wait, n4 is dropped.
    const pub = new RabbitPublisher({ opener: openerFor(sink), queueSize: 2 }, logger);
    for (const id of ["n1", "n2", "n3", "n4"]) {
      pub.noteCreated(note(id));
    }
    await pub.close(1000);
    expect(sink.published).toHaveLength(3);
    expect(lines.map((l) => (JSON.parse(l) as { msg: string }).msg)).toContain(
      "rabbit publish queue full, event dropped",
    );
  });

  it("drops events after close", async () => {
    const sink = new FakeSink();
    const lines: string[] = [];
    const logger = AppLogger.create({
      level: "debug",
      service: "test",
      version: "test",
      environment: "test",
      destination: { write: (line: string) => void lines.push(line) },
    });
    const pub = new RabbitPublisher({ opener: openerFor(sink) }, logger);
    await pub.close(1000);
    pub.noteCreated(note("after"));
    expect(sink.published).toHaveLength(0);
    expect(lines.map((l) => (JSON.parse(l) as { msg: string }).msg)).toContain(
      "rabbit publisher closed, event dropped",
    );
  });
});

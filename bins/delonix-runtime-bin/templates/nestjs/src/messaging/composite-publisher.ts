import type { OnApplicationShutdown } from "@nestjs/common";

import type { Note } from "../notes/note";
import { NotePublisher } from "../notes/note.publisher";

/**
 * A {@link NotePublisher} that forwards one note to several — so wiring both the
 * outbound webhook dispatcher AND the RabbitMQ publisher is not a choice between
 * them. Each delegate is already non-blocking and bounded, so one failing to
 * enqueue never stops the others. On shutdown it drains every delegate that has
 * its own `onApplicationShutdown` (both of ours do), so Nest need only know this
 * one provider.
 */
export class CompositePublisher extends NotePublisher implements OnApplicationShutdown {
  constructor(private readonly publishers: NotePublisher[]) {
    super();
  }

  noteCreated(note: Note): void {
    for (const publisher of this.publishers) {
      publisher.noteCreated(note);
    }
  }

  async onApplicationShutdown(signal?: string): Promise<void> {
    await Promise.all(
      this.publishers.map((publisher) => {
        const hook = (publisher as Partial<OnApplicationShutdown>).onApplicationShutdown;
        return hook === undefined ? undefined : hook.call(publisher, signal);
      }),
    );
  }
}

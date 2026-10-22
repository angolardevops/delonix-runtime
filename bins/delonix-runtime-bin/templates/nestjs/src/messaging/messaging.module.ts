import { Global, Module } from "@nestjs/common";

import { AppConfig } from "../config/app-config";
import { AppLogger } from "../logging/app-logger";
import { NoopNotePublisher, NotePublisher } from "../notes/note.publisher";
import { parseSecret } from "../webhooks/signature";
import { WebhookDispatcher } from "../webhooks/webhook-dispatcher";
import { amqpOpener } from "./amqp";
import { CompositePublisher } from "./composite-publisher";
import { RabbitPublisher } from "./rabbit-publisher";

/**
 * Owns the one {@link NotePublisher} the whole application injects — the single
 * seam the notes use case publishes through. The configuration decides what is
 * behind it:
 *
 *   - nothing set  → NoopNotePublisher (publishing is off);
 *   - one target   → that publisher directly;
 *   - both set     → CompositePublisher fanning the note out to each.
 *
 * The outbound webhook dispatcher (an adapter under webhooks/) and the RabbitMQ
 * publisher are composed here rather than each claiming the token, so they are
 * additive. Both drain on shutdown through the single provided instance.
 */
@Global()
@Module({
  providers: [
    {
      provide: NotePublisher,
      inject: [AppConfig, AppLogger],
      useFactory: (config: AppConfig, logger: AppLogger): NotePublisher =>
        buildPublisher(config, logger),
    },
  ],
  exports: [NotePublisher],
})
export class MessagingModule {}

function buildPublisher(config: AppConfig, logger: AppLogger): NotePublisher {
  const publishers: NotePublisher[] = [];
  if (config.webhookTargetUrl !== "") {
    publishers.push(
      new WebhookDispatcher(
        {
          url: config.webhookTargetUrl,
          key: parseSecret(config.webhookTargetSecret),
          timeoutMs: 5_000,
          maxAttempts: 4,
          baseBackoffMs: 500,
          queueSize: 100,
        },
        logger,
      ),
    );
  }
  if (config.rabbitmqUrl !== "") {
    publishers.push(new RabbitPublisher({ opener: amqpOpener(config.rabbitmqUrl) }, logger));
  }
  if (publishers.length === 0) {
    return new NoopNotePublisher();
  }
  if (publishers.length === 1) {
    return publishers[0];
  }
  return new CompositePublisher(publishers);
}

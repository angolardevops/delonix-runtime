import { type DynamicModule, Global, Module } from "@nestjs/common";

import { AppConfig } from "../config/app-config";
import { AppLogger } from "../logging/app-logger";
import { NoopNotePublisher, NotePublisher } from "../notes/note.publisher";
import { NotesModule } from "../notes/notes.module";
import { DeliveryDedup } from "./dedup";
import { InboundWebhookController } from "./inbound-webhook.controller";
import { InboundWebhookKey } from "./inbound-webhook.key";
import { parseSecret } from "./signature";
import { WebhookDispatcher } from "./webhook-dispatcher";

/**
 * The outbound side: provides NotePublisher to the whole application — the
 * dispatcher when WEBHOOK_TARGET_URL is set, a no-op otherwise.
 */
@Global()
@Module({
  providers: [
    {
      provide: NotePublisher,
      inject: [AppConfig, AppLogger],
      useFactory: (config: AppConfig, logger: AppLogger): NotePublisher =>
        config.webhookTargetUrl === ""
          ? new NoopNotePublisher()
          : new WebhookDispatcher(
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
    },
  ],
  exports: [NotePublisher],
})
export class OutboundWebhookModule {}

/** The inbound side: the endpoint exists only when a secret is configured. */
@Module({})
export class InboundWebhookModule {
  static register(config: AppConfig): DynamicModule {
    if (config.webhookInboundSecret === "") {
      return { module: InboundWebhookModule };
    }
    return {
      module: InboundWebhookModule,
      imports: [NotesModule],
      controllers: [InboundWebhookController],
      providers: [
        {
          provide: InboundWebhookKey,
          useValue: new InboundWebhookKey(parseSecret(config.webhookInboundSecret)),
        },
        { provide: DeliveryDedup, useValue: new DeliveryDedup(4096) },
      ],
    };
  }
}

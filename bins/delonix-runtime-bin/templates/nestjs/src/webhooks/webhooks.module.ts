import { type DynamicModule, Module } from "@nestjs/common";

import { AppConfig } from "../config/app-config";
import { NotesModule } from "../notes/notes.module";
import { DeliveryDedup } from "./dedup";
import { InboundWebhookController } from "./inbound-webhook.controller";
import { InboundWebhookKey } from "./inbound-webhook.key";
import { parseSecret } from "./signature";

/**
 * The inbound side: the endpoint exists only when a secret is configured. The
 * OUTBOUND side — providing NotePublisher — lives in MessagingModule, which
 * composes the webhook dispatcher with the RabbitMQ publisher.
 */
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

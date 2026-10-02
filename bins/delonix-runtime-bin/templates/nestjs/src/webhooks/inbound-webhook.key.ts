/** The decoded inbound secret, as an injectable value. */
export class InboundWebhookKey {
  constructor(readonly value: Buffer) {}
}

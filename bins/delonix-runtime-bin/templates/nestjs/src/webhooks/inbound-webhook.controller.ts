import { Controller, HttpCode, Post, type RawBodyRequest, Req } from "@nestjs/common";
import {
  ApiBadRequestResponse,
  ApiBody,
  ApiHeader,
  ApiNoContentResponse,
  ApiOperation,
  ApiPayloadTooLargeResponse,
  ApiTags,
  ApiUnauthorizedResponse,
  ApiUnprocessableEntityResponse,
} from "@nestjs/swagger";
import type { Request } from "express";

import { ErrorBody, HttpProblem } from "../http/error-body";
import { AppLogger } from "../logging/app-logger";
import { NotesService } from "../notes/notes.service";
import { DeliveryDedup } from "./dedup";
import { WebhookEventDto } from "./dto/webhook-event.dto";
import { InboundWebhookKey } from "./inbound-webhook.key";
import { HEADER_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, verify } from "./signature";

/** An inbound delivery is bounded independently of the API body limit. */
export const WEBHOOK_MAX_BODY_BYTES = 256 * 1024;

/**
 * Receives signed deliveries. Registered only when WEBHOOK_INBOUND_SECRET is
 * set (see AppModule). `note.create` runs the same use case as POST
 * /api/v1/notes; `ping` only proves the signature. A delivery id already
 * accepted is acknowledged again without being processed again.
 */
@ApiTags("webhooks")
@Controller("api/v1/webhooks")
export class InboundWebhookController {
  constructor(
    private readonly notes: NotesService,
    private readonly key: InboundWebhookKey,
    private readonly dedup: DeliveryDedup,
    private readonly logger: AppLogger,
  ) {}

  @Post("inbound")
  @ApiOperation({
    summary: "Receive a signed webhook (Standard Webhooks). Only served when WEBHOOK_INBOUND_SECRET is set.",
  })
  @HttpCode(204)
  @ApiHeader({ name: HEADER_ID, required: true })
  @ApiHeader({ name: HEADER_TIMESTAMP, required: true, description: "Unix seconds." })
  @ApiHeader({ name: HEADER_SIGNATURE, required: true, description: "v1,<base64 HMAC-SHA256>" })
  @ApiBody({ type: WebhookEventDto })
  @ApiNoContentResponse({ description: "Accepted, or a duplicate of an accepted delivery." })
  @ApiBadRequestResponse({ description: "Missing headers or a body that is not JSON.", type: ErrorBody })
  @ApiUnauthorizedResponse({ description: "Signature or timestamp not valid.", type: ErrorBody })
  @ApiPayloadTooLargeResponse({ description: "Body over 256 KiB.", type: ErrorBody })
  @ApiUnprocessableEntityResponse({
    description: "Unknown event, or a note.create the use case refused.",
    type: ErrorBody,
  })
  async receive(@Req() req: RawBodyRequest<Request>): Promise<void> {
    // The bytes the sender signed, kept by the body parser (rawBody: true in
    // main.ts). The parsed req.body is never used for the signature.
    const raw = req.rawBody;
    if (raw === undefined) {
      throw new HttpProblem(
        400,
        "malformed_json",
        "webhook body must be JSON (Content-Type: application/json)",
      );
    }
    if (raw.length > WEBHOOK_MAX_BODY_BYTES) {
      throw new HttpProblem(413, "body_too_large", "request body too large");
    }
    const id = req.header(HEADER_ID);
    const now = Math.floor(Date.now() / 1000);
    const result = verify(
      this.key.value,
      id,
      req.header(HEADER_TIMESTAMP),
      req.header(HEADER_SIGNATURE),
      raw,
      now,
    );
    if (result === "missing_headers") {
      throw new HttpProblem(400, "missing_signature", "webhook headers are missing");
    }
    if (result !== "ok" || id === undefined) {
      this.logger.warn("webhook rejected", { reason: result });
      throw new HttpProblem(401, "invalid_signature", "webhook signature is not valid");
    }
    if (!this.dedup.firstSeen(id, now)) {
      return;
    }
    let event: { type?: unknown; data?: { title?: unknown; body?: unknown } };
    try {
      event = JSON.parse(raw.toString("utf8")) as typeof event;
    } catch {
      this.dedup.forget(id);
      throw new HttpProblem(400, "malformed_json", "webhook body is not JSON");
    }
    switch (event.type) {
      case "ping":
        return;
      case "note.create": {
        const title = event.data?.title;
        const body = event.data?.body;
        if (typeof title !== "string" || (body !== undefined && typeof body !== "string")) {
          this.dedup.forget(id);
          throw new HttpProblem(422, "validation_failed", "invalid input", {
            data: "must have a string title and an optional string body",
          });
        }
        try {
          await this.notes.create({ title, body });
        } catch (error) {
          // Not processed: a retry of this id must be processed, not acknowledged as a duplicate.
          this.dedup.forget(id);
          throw error;
        }
        return;
      }
      default:
        this.dedup.forget(id);
        throw new HttpProblem(422, "unknown_event", "unknown webhook event type");
    }
  }
}

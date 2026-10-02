import type { FastifyInstance } from "fastify";
import { sendError } from "../http/errors.js";
import type { NotesUseCases } from "../notes/notes.js";
import type { Dedup } from "./dedup.js";
import { HEADER_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, verify } from "./signature.js";

/** Bounds an inbound delivery independently of the API body limit. */
const MAX_WEBHOOK_BODY = 256 * 1024;

const header = (v: string | string[] | undefined): string | undefined =>
  Array.isArray(v) ? v[0] : v;

/**
 * Inbound webhook. This plugin is its own encapsulation context, so the raw
 * content-type parsers below apply to this route only: the signature is over
 * the bytes as received, and a parsed-and-re-encoded body would not verify.
 *
 * `note.create` runs the same use case as POST /api/v1/notes; `ping` only
 * proves the signature. A delivery id already accepted is acknowledged again
 * without being processed again.
 */
export async function inboundWebhookRoutes(
  app: FastifyInstance,
  opts: { key: Buffer; dedup: Dedup; notes: NotesUseCases; now?: () => number },
) {
  const now = opts.now ?? (() => Math.floor(Date.now() / 1000));
  const raw = (_req: unknown, body: Buffer, done: (err: Error | null, body?: Buffer) => void) =>
    done(null, body);
  app.addContentTypeParser("application/json", { parseAs: "buffer" }, raw);
  app.addContentTypeParser("*", { parseAs: "buffer" }, raw);

  app.post(
    "/inbound",
    {
      bodyLimit: MAX_WEBHOOK_BODY,
      schema: {
        summary:
          "Receive a signed webhook (Standard Webhooks). Only served when WEBHOOK_INBOUND_SECRET is set.",
        response: {
          204: { type: "null", description: "Accepted, or a duplicate of an accepted delivery." },
          400: { $ref: "Error#" },
          401: { $ref: "Error#" },
          413: { $ref: "Error#" },
          422: { $ref: "Error#" },
        },
      },
    },
    async (request, reply) => {
      const body = Buffer.isBuffer(request.body) ? request.body : Buffer.alloc(0);
      const id = header(request.headers[HEADER_ID]);
      const failure = verify(
        opts.key,
        id,
        header(request.headers[HEADER_TIMESTAMP]),
        header(request.headers[HEADER_SIGNATURE]),
        body,
        now(),
      );
      if (failure === "missing_headers")
        return sendError(request, reply, 400, "missing_signature", "webhook headers are missing");
      if (failure) {
        request.log.warn({ reason: failure }, "webhook rejected");
        return sendError(
          request,
          reply,
          401,
          "invalid_signature",
          "webhook signature is not valid",
        );
      }
      if (!opts.dedup.firstSeen(id!, now())) return reply.code(204).send();

      let event: { type?: unknown; data?: unknown };
      try {
        event = JSON.parse(body.toString("utf8"));
      } catch {
        return sendError(request, reply, 400, "malformed_json", "webhook body is not JSON");
      }
      if (event.type === "ping") return reply.code(204).send();
      if (event.type !== "note.create")
        return sendError(request, reply, 422, "unknown_event", "unknown webhook event type");
      const data = (event.data ?? {}) as { title?: unknown; body?: unknown };
      if (
        typeof data.title !== "string" ||
        (data.body !== undefined && typeof data.body !== "string")
      )
        return sendError(request, reply, 400, "malformed_json", "webhook data is not a note");
      try {
        await opts.notes.create({ title: data.title, body: data.body as string | undefined });
      } catch (err) {
        // Not processed: a retry of this id must be processed, not acknowledged
        // as a duplicate.
        opts.dedup.forget(id!);
        throw err;
      }
      return reply.code(204).send();
    },
  );
}

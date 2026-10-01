import {
  decodeNoteInput,
  errorResponse,
  HttpError,
  parseJsonObject,
  readBody,
  route,
} from "@/lib/http/route";
import { HEADER_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, verify } from "@/lib/server/webhooks/signature";

export const dynamic = "force-dynamic";

/** Bounds an inbound delivery independently of the API limit. */
const MAX_BODY_BYTES = 256 * 1024;

// Accepts a signed delivery (Standard Webhooks). `note.create` runs the same
// use case as POST /api/v1/notes; `ping` only proves the signature. A
// delivery id already accepted is acknowledged again without being processed
// again. The signature is checked over the raw bytes, before any parsing.
export const POST = route(async (request, _context, runtime) => {
  const key = runtime.inboundKey;
  if (!key) return errorResponse(404, "not_found", "no such route");

  const body = await readBody(request, MAX_BODY_BYTES);
  const id = request.headers.get(HEADER_ID);
  const result = verify(
    key,
    {
      id,
      timestamp: request.headers.get(HEADER_TIMESTAMP),
      signature: request.headers.get(HEADER_SIGNATURE),
    },
    body,
    Math.floor(runtime.nowMs() / 1000),
  );
  if (result === "missing_headers" || !id) {
    return errorResponse(400, "missing_signature", "webhook headers are missing");
  }
  if (result !== "ok") {
    runtime.log.warn("webhook rejected", { reason: result });
    return errorResponse(401, "invalid_signature", "webhook signature is not valid");
  }
  if (!runtime.dedup.firstSeen(id, runtime.nowMs())) {
    return new Response(null, { status: 204 });
  }
  let type: unknown;
  try {
    const event = parseJsonObject(body);
    type = event.type;
    switch (type) {
      case "ping":
        break;
      case "note.create":
        await runtime.notes.create(decodeNoteInput(event.data));
        break;
      default:
        throw new HttpError(422, "unknown_event", "unknown webhook event type");
    }
  } catch (err) {
    // Not processed: a retry of this id must be processed, not acknowledged as a duplicate.
    runtime.dedup.forget(id);
    throw err;
  }
  runtime.log.info("webhook accepted", { webhook_id: id, event: String(type) });
  return new Response(null, { status: 204 });
});

/**
 * One error shape for every endpoint:
 *   {"error":{"code","message","fields"?,"request_id"}}
 * and the mapping from framework and use-case errors to it. Nothing here leaks
 * a stack or an internal message to the client.
 */
import type { FastifyError, FastifyInstance, FastifyReply, FastifyRequest } from "fastify";
import { NotFoundError, ValidationError } from "../notes/notes.js";

export const errorSchema = {
  $id: "Error",
  type: "object",
  required: ["error"],
  properties: {
    error: {
      type: "object",
      required: ["code", "message"],
      properties: {
        code: { type: "string" },
        message: { type: "string" },
        fields: { type: "object", additionalProperties: { type: "string" } },
        request_id: { type: "string" },
      },
    },
  },
} as const;

export function sendError(
  request: FastifyRequest,
  reply: FastifyReply,
  status: number,
  code: string,
  message: string,
  fields?: Record<string, string>,
): FastifyReply {
  return reply
    .code(status)
    .send({ error: { code, message, ...(fields ? { fields } : {}), request_id: request.id } });
}

/** Turns Ajv's validation list into {field: message}. */
function validationFields(err: FastifyError): Record<string, string> {
  const fields: Record<string, string> = {};
  for (const v of err.validation ?? []) {
    const params = v.params as { missingProperty?: string; additionalProperty?: string };
    const name =
      params.missingProperty ??
      params.additionalProperty ??
      (v.instancePath.replace(/^\//, "").replaceAll("/", ".") || "body");
    fields[name] =
      v.keyword === "additionalProperties" ? "is not allowed" : (v.message ?? "is invalid");
  }
  return fields;
}

export function registerErrorHandling(app: FastifyInstance): void {
  app.setNotFoundHandler((request, reply) =>
    sendError(request, reply, 404, "not_found", "no such route"),
  );
  app.setErrorHandler((err: FastifyError, request, reply) => {
    if (err instanceof ValidationError)
      return sendError(request, reply, 422, "validation_failed", "invalid input", err.fields);
    if (err instanceof NotFoundError)
      return sendError(request, reply, 404, "not_found", "note not found");
    if (err.validation)
      return sendError(
        request,
        reply,
        422,
        "validation_failed",
        "invalid input",
        validationFields(err),
      );
    if (err.code === "FST_ERR_CTP_BODY_TOO_LARGE")
      return sendError(request, reply, 413, "body_too_large", "request body too large");
    if (err.statusCode === 400 || err.statusCode === 415)
      return sendError(
        request,
        reply,
        400,
        "malformed_json",
        "request body is not the expected JSON",
      );
    request.log.error({ err: err.message }, "request failed");
    return sendError(request, reply, 500, "internal", "internal error");
  });
}

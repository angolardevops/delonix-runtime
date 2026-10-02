// The HTTP transport shared by every Route Handler: request id, access log,
// the one error shape, body limits, and the in-flight count the shutdown
// waits for. It translates HTTP to use-case calls and back; no business rule
// lives here.
import "server-only";
import { isNoteNotFound, isValidationError } from "@/lib/notes/notes";
import { runWithRequest, currentRequest } from "@/lib/server/request-context";
import { getRuntime, type Runtime } from "@/lib/server/runtime";

/** An error a handler raises on purpose; anything else becomes a bare 500. */
export class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly fields?: Record<string, string>,
  ) {
    super(message);
    this.name = "HttpError";
  }
}

export function json(status: number, body: unknown, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", ...headers },
  });
}

/** The one error shape: {"error":{"code","message","fields"?,"request_id"}}. */
export function errorResponse(
  status: number,
  code: string,
  message: string,
  fields?: Record<string, string>,
): Response {
  return json(status, { error: { code, message, fields, request_id: currentRequest()?.requestId } });
}

function toResponse(runtime: Runtime, err: unknown): Response {
  if (err instanceof HttpError) return errorResponse(err.status, err.code, err.message, err.fields);
  if (isValidationError(err)) return errorResponse(422, "validation_failed", "invalid input", err.fields);
  if (isNoteNotFound(err)) return errorResponse(404, "not_found", "note not found");
  // Logged with its message; the client gets no detail and no stack.
  runtime.log.error("request failed", { error: err instanceof Error ? err.message : String(err) });
  return errorResponse(500, "internal", "internal error");
}

// A client-sent X-Request-Id is kept only if it is short and plain; anything
// else is replaced, so a header cannot inject content into the logs.
const VALID_REQUEST_ID = /^[A-Za-z0-9._-]{1,64}$/;

function requestIdOf(request: Request): string {
  const sent = request.headers.get("x-request-id") ?? "";
  return VALID_REQUEST_ID.test(sent) ? sent : globalThis.crypto.randomUUID().replaceAll("-", "").slice(0, 16);
}

export interface RouteOptions {
  /** Probes: logged at debug, and answered even while the process refuses work. */
  probe?: boolean;
}

type Handler<C> = (request: Request, context: C, runtime: Runtime) => Promise<Response> | Response;

/** Wraps a Route Handler. `C` is the context Next.js passes (`{ params }`). */
export function route<C = unknown>(handler: Handler<C>, options: RouteOptions = {}) {
  return async (request: Request, context: C): Promise<Response> => {
    const runtime = getRuntime();
    const requestId = requestIdOf(request);
    return runWithRequest({ requestId }, async () => {
      const started = runtime.nowMs();
      let response: Response;
      if (runtime.lifecycle.isClosing && !options.probe) {
        response = errorResponse(503, "shutting_down", "the service is shutting down");
        response.headers.set("connection", "close");
      } else {
        // Counted until the response has left the process, not just until the
        // handler returned: that is what the shutdown must wait for.
        const done = runtime.lifecycle.begin();
        try {
          response = await handler(request, context, runtime);
        } catch (err) {
          response = toResponse(runtime, err);
        }
        try {
          runtime.afterResponse(async () => done());
        } catch {
          done();
        }
      }
      response.headers.set("x-request-id", requestId);
      // The path only: never the query string, headers or body.
      const fields = {
        method: request.method,
        path: new URL(request.url).pathname,
        status: response.status,
        duration_ms: runtime.nowMs() - started,
      };
      if (options.probe) runtime.log.debug("request", fields);
      else runtime.log.info("request", fields);
      return response;
    });
  };
}

/** Reads the raw body, refusing anything over `maxBytes` with 413 before buffering it all. */
export async function readBody(request: Request, maxBytes: number): Promise<Uint8Array> {
  const tooLarge = () => new HttpError(413, "body_too_large", "request body too large");
  const declared = Number(request.headers.get("content-length") ?? "");
  if (Number.isFinite(declared) && declared > maxBytes) throw tooLarge();
  if (!request.body) return new Uint8Array();
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > maxBytes) {
      await reader.cancel();
      throw tooLarge();
    }
    chunks.push(value);
  }
  const body = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
}

/** Parses a JSON object strictly; anything else is 400 malformed_json. */
export function parseJsonObject(raw: Uint8Array | string): Record<string, unknown> {
  const malformed = () => new HttpError(400, "malformed_json", "request body is not the expected JSON");
  let value: unknown;
  try {
    value = JSON.parse(typeof raw === "string" ? raw : new TextDecoder("utf-8", { fatal: true }).decode(raw));
  } catch {
    throw malformed();
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw malformed();
  return value as Record<string, unknown>;
}

/** The note input of the API and of the `note.create` webhook: unknown fields and wrong types are refused. */
export function decodeNoteInput(value: unknown): { title: string; body?: string } {
  const malformed = () => new HttpError(400, "malformed_json", "request body is not the expected JSON");
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw malformed();
  const { title, body, ...unknown } = value as Record<string, unknown>;
  if (Object.keys(unknown).length > 0) throw malformed();
  if (title !== undefined && typeof title !== "string") throw malformed();
  if (body !== undefined && typeof body !== "string") throw malformed();
  return { title: title ?? "", body };
}

/** An integer query parameter, or the default when absent. */
export function intParam(url: URL, name: string, fallback: number, fields: Record<string, string>): number {
  const raw = url.searchParams.get(name);
  if (raw === null || raw === "") return fallback;
  if (!/^-?\d{1,9}$/.test(raw)) {
    fields[name] = "must be an integer";
    return fallback;
  }
  return Number(raw);
}

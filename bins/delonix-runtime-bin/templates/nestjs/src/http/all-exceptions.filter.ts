import {
  type ArgumentsHost,
  BadRequestException,
  Catch,
  type ExceptionFilter,
  HttpException,
  HttpStatus,
  NotFoundException,
  PayloadTooLargeException,
  type RawBodyRequest,
} from "@nestjs/common";
import type { Request, Response } from "express";

import { AppLogger } from "../logging/app-logger";
import { NoteNotFoundError, NoteValidationError } from "../notes/note";
import { HttpProblem } from "./error-body";
import { requestIdOf } from "./request-id";

interface Problem {
  status: number;
  code: string;
  message: string;
  fields?: Record<string, string>;
}

/**
 * Maps every error to the one shape `{"error":{code,message,fields?,request_id}}`.
 * Use-case errors are translated here, so the use cases never know about HTTP.
 * Anything unexpected is logged and answered as a bare 500, without a stack.
 */
@Catch()
export class AllExceptionsFilter implements ExceptionFilter {
  constructor(private readonly logger: AppLogger) {}

  catch(exception: unknown, host: ArgumentsHost): void {
    const http = host.switchToHttp();
    const req = http.getRequest<Request>();
    const res = http.getResponse<Response>();
    const problem = this.classify(exception, req);
    if (res.headersSent) {
      return;
    }
    res.status(problem.status).json({
      error: {
        code: problem.code,
        message: problem.message,
        ...(problem.fields !== undefined ? { fields: problem.fields } : {}),
        request_id: requestIdOf(req),
      },
    });
  }

  private classify(exception: unknown, req: Request): Problem {
    if (exception instanceof HttpProblem) {
      return exception;
    }
    if (exception instanceof NoteValidationError) {
      return { status: 422, code: "validation_failed", message: "invalid input", fields: exception.fields };
    }
    if (exception instanceof NoteNotFoundError) {
      return { status: 404, code: "not_found", message: "note not found" };
    }
    // The body parser's errors. Too large: its own error (`type`), passed
    // through by Nest. Not JSON: the Express adapter turns the SyntaxError into
    // a BadRequestException, so the kept raw bytes decide whether that is what
    // happened.
    if (
      (exception as { type?: unknown } | null)?.type === "entity.too.large" ||
      exception instanceof PayloadTooLargeException
    ) {
      return { status: 413, code: "body_too_large", message: "request body too large" };
    }
    if (exception instanceof BadRequestException && !isJson((req as RawBodyRequest<Request>).rawBody)) {
      return { status: 400, code: "malformed_json", message: "request body is not the expected JSON" };
    }
    if (exception instanceof NotFoundException) {
      return { status: 404, code: "not_found", message: "no such route" };
    }
    if (exception instanceof HttpException && exception.getStatus() < 500) {
      const status = exception.getStatus();
      return { status, code: codeFor(status), message: messageOf(exception) };
    }
    this.logger.error("request failed", {
      error: exception instanceof Error ? exception.message : String(exception),
      path: req.path,
    });
    return { status: HttpStatus.INTERNAL_SERVER_ERROR, code: "internal", message: "internal error" };
  }
}

/**
 * True when there was no body, or it is what the body parser accepts: a JSON
 * object or array. (`"text"` and `5` are valid JSON, and refused by the parser.)
 */
function isJson(raw: Buffer | undefined): boolean {
  if (raw === undefined || raw.length === 0) {
    return true;
  }
  try {
    const value: unknown = JSON.parse(raw.toString("utf8"));
    return value !== null && typeof value === "object";
  } catch {
    return false;
  }
}

function codeFor(status: number): string {
  switch (status) {
    case 400:
      return "bad_request";
    case 405:
      return "method_not_allowed";
    case 415:
      return "unsupported_media_type";
    default:
      return `http_${status}`;
  }
}

function messageOf(exception: HttpException): string {
  const response = exception.getResponse();
  if (typeof response === "string") {
    return response;
  }
  const message = (response as { message?: unknown }).message;
  return typeof message === "string" ? message : exception.message;
}

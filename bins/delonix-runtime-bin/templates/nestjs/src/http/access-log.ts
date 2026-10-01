import { context } from "@opentelemetry/api";
import type { NextFunction, Request, Response } from "express";

import type { AppLogger } from "../logging/app-logger";
import { requestIdOf } from "./request-id";

/**
 * One line per request, written when the response finishes. It logs the path,
 * never the query string, headers or body. Health probes log at debug. The
 * trace context is captured when the request starts, so the line carries the
 * request's trace_id even though `finish` fires outside the handler.
 */
export function accessLogMiddleware(logger: AppLogger) {
  return (req: Request, res: Response, next: NextFunction): void => {
    const started = process.hrtime.bigint();
    const active = context.active();
    res.on("finish", () => {
      const fields = {
        method: req.method,
        path: req.path,
        status: res.statusCode,
        duration_ms: Number((process.hrtime.bigint() - started) / 1_000_000n),
        request_id: requestIdOf(req),
      };
      context.with(active, () => {
        if (req.path.startsWith("/api/v1/health/")) {
          logger.debug("request", fields);
        } else {
          logger.log("request", fields);
        }
      });
    });
    next();
  };
}

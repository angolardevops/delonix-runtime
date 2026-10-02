import { randomBytes } from "node:crypto";

import type { NextFunction, Request, Response } from "express";

/** A client-sent X-Request-Id is kept only if short and plain, so a header cannot inject content into the logs. */
const VALID_REQUEST_ID = /^[A-Za-z0-9._-]{1,64}$/;

const REQUEST_ID = Symbol("requestId");

type WithRequestId = Request & { [REQUEST_ID]?: string };

/**
 * Express middleware, registered before the body parser so even a request with
 * a malformed body gets an id: sets it on the request and the response.
 */
export function requestIdMiddleware(req: Request, res: Response, next: NextFunction): void {
  const sent = req.header("x-request-id");
  const id = sent !== undefined && VALID_REQUEST_ID.test(sent) ? sent : randomBytes(8).toString("hex");
  (req as WithRequestId)[REQUEST_ID] = id;
  res.setHeader("X-Request-Id", id);
  next();
}

export function requestIdOf(req: Request): string {
  return (req as WithRequestId)[REQUEST_ID] ?? "";
}

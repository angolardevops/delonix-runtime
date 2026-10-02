import { type CallHandler, type ExecutionContext, Injectable, type NestInterceptor } from "@nestjs/common";
import { context } from "@opentelemetry/api";
import { getRPCMetadata, RPCType } from "@opentelemetry/core";
import type { Request } from "express";
import type { Observable } from "rxjs";

/**
 * Tells the HTTP instrumentation which route matched, so the server span is
 * named `POST /api/v1/notes/{id}` and its metrics are labelled by the route
 * template — one series per route, not one per id.
 */
@Injectable()
export class HttpRouteInterceptor implements NestInterceptor {
  intercept(ctx: ExecutionContext, next: CallHandler): Observable<unknown> {
    const req = ctx.switchToHttp().getRequest<Request>();
    const route = (req.route as { path?: unknown } | undefined)?.path;
    const rpc = getRPCMetadata(context.active());
    if (typeof route === "string" && rpc?.type === RPCType.HTTP) {
      rpc.route = route.replace(/:([A-Za-z0-9_]+)/g, "{$1}");
    }
    return next.handle();
  }
}

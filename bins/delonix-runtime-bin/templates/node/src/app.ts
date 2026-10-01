/**
 * Application factory: one Fastify instance with the cross-cutting behaviour
 * (request ids, access log, error shape, body limit, OpenAPI) and one plugin
 * per capability. No business rule lives here.
 */
import { randomBytes } from "node:crypto";
import swagger from "@fastify/swagger";
import { trace } from "@opentelemetry/api";
import Fastify, { LogController, type FastifyBaseLogger, type FastifyInstance } from "fastify";
import type { Readiness } from "./health/readiness.js";
import { healthRoutes } from "./health/routes.js";
import { errorSchema, registerErrorHandling } from "./http/errors.js";
import type { NotesUseCases } from "./notes/notes.js";
import { notesRoutes } from "./notes/routes.js";
import { SERVICE_NAME } from "./service.js";
import type { Dedup } from "./webhooks/dedup.js";
import { inboundWebhookRoutes } from "./webhooks/routes.js";

export interface AppDeps {
  logger: FastifyBaseLogger;
  notes: NotesUseCases;
  readiness: Readiness;
  maxBodyBytes: number;
  /** Enables the inbound webhook route; undefined leaves it unregistered. */
  inboundWebhook?: { key: Buffer; dedup: Dedup; now?: () => number };
}

/** A client-sent X-Request-Id is kept only if short and plain, so it cannot inject into logs. */
const VALID_REQUEST_ID = /^[A-Za-z0-9._-]{1,64}$/;

export async function buildApp(deps: AppDeps): Promise<FastifyInstance> {
  const app = Fastify({
    loggerInstance: deps.logger,
    // The access log below replaces Fastify's: it never logs the query string.
    logController: new LogController({
      disableRequestLogging: true,
      requestIdLogLabel: "request_id",
    }),
    bodyLimit: deps.maxBodyBytes,
    requestIdHeader: false,
    genReqId: (req) => {
      const id = req.headers["x-request-id"];
      return typeof id === "string" && VALID_REQUEST_ID.test(id)
        ? id
        : randomBytes(8).toString("hex");
    },
    ajv: { customOptions: { removeAdditional: false, coerceTypes: true, allErrors: true } },
  });

  app.addSchema(errorSchema);
  registerErrorHandling(app);

  await app.register(swagger, {
    openapi: {
      openapi: "3.1.0",
      info: {
        title: SERVICE_NAME,
        version: "1.0.0",
        description:
          "HTTP contract, generated from the route schemas. api/openapi.json is the checked-in copy; test/openapi.test.ts fails when they differ.",
      },
    },
  });

  app.addHook("onRequest", async (request, reply) => {
    reply.header("x-request-id", request.id);
    // Name the span after the route pattern, not the raw path: /notes/:id is
    // one series, not one per id.
    const route = request.routeOptions.url;
    const span = trace.getActiveSpan();
    if (span && route) {
      span.updateName(`${request.method} ${route}`);
      span.setAttribute("http.route", route);
    }
  });

  app.addHook("onResponse", async (request, reply) => {
    const path = request.url.split("?", 1)[0] ?? request.url;
    const line = {
      method: request.method,
      path,
      status: reply.statusCode,
      duration_ms: Math.round(reply.elapsedTime),
    };
    // Probes every few seconds would drown the lines that matter.
    if (path.startsWith("/api/v1/health/")) request.log.debug(line, "request");
    else request.log.info(line, "request");
  });

  await app.register(healthRoutes, { prefix: "/api/v1/health", readiness: deps.readiness });
  await app.register(notesRoutes, { prefix: "/api/v1/notes", notes: deps.notes });
  if (deps.inboundWebhook) {
    await app.register(inboundWebhookRoutes, {
      prefix: "/api/v1/webhooks",
      notes: deps.notes,
      ...deps.inboundWebhook,
    });
  }
  return app;
}

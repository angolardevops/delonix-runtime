import Fastify, { type FastifyInstance } from "fastify";
import { healthRoutes } from "./routes/health.js";

/** Application factory — wires config, routes, and plugins. */
export function buildApp(): FastifyInstance {
  const app = Fastify({
    logger: { level: process.env.LOG_LEVEL ?? "info" },
  });
  app.register(healthRoutes, { prefix: "/api/v1/health" });
  return app;
}

import type { FastifyInstance } from "fastify";

/** Liveness/readiness probes (used by the Delonixfile HEALTHCHECK). */
export async function healthRoutes(app: FastifyInstance): Promise<void> {
  app.get("/live", async () => ({ status: "alive" }));
  app.get("/ready", async () => ({ status: "ready" }));
}

import type { FastifyInstance } from "fastify";
import type { Readiness } from "./readiness.js";

const statusSchema = {
  type: "object",
  required: ["status"],
  properties: { status: { type: "string" }, dependency: { type: "string" } },
} as const;

/** Liveness (the process answers) and readiness (it should receive traffic). */
export async function healthRoutes(app: FastifyInstance, opts: { readiness: Readiness }) {
  app.get(
    "/live",
    {
      schema: { summary: "Liveness — the process answers.", response: { 200: statusSchema } },
    },
    async () => ({ status: "alive" }),
  );
  app.get(
    "/ready",
    {
      schema: {
        summary: "Readiness — the process should receive traffic.",
        response: { 200: statusSchema, 503: statusSchema },
      },
    },
    async (request, reply) => {
      const { state, failing } = await opts.readiness.evaluate();
      if (state !== "ready") return reply.code(503).send({ status: state });
      if (failing) {
        // The name goes to the client; the reason stays in the logs.
        request.log.warn({ dependency: failing }, "readiness dependency failing");
        return reply.code(503).send({ status: "unavailable", dependency: failing });
      }
      return { status: "ready" };
    },
  );
}

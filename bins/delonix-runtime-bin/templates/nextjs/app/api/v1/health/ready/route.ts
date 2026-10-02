import { json, route } from "@/lib/http/route";

export const dynamic = "force-dynamic";

/** Readiness: 503 `starting` → 200 `ready` → 503 `draining` after SIGTERM. */
export const GET = route(
  async (_request, _context, runtime) => {
    const { state, failing } = await runtime.lifecycle.evaluate();
    if (state !== "ready") return json(503, { status: state });
    if (failing) {
      // The failure detail stays in the logs, never in the response.
      runtime.log.warn("readiness dependency failing", { dependency: failing });
      return json(503, { status: "unavailable", dependency: failing });
    }
    return json(200, { status: "ready" });
  },
  { probe: true },
);

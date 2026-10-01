import { json, route } from "@/lib/http/route";

// Never prerendered at build time: the answer is about the running process.
export const dynamic = "force-dynamic";

/** Liveness: the process answers. Used by the image's HEALTHCHECK. */
export const GET = route(() => json(200, { status: "alive" }), { probe: true });

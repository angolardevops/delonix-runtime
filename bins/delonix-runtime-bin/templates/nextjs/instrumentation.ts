// Next.js calls register() once when a server process starts, before it
// handles a request. Everything here is Node-only, so it is imported behind
// the runtime check (the Edge runtime has no process signals or OTel SDK).
// `next build` loads this file too; a build is not a server, so it is skipped.
export async function register(): Promise<void> {
  if (process.env.NEXT_RUNTIME === "nodejs" && process.env.NEXT_PHASE !== "phase-production-build") {
    const { boot } = await import("./lib/server/boot");
    boot();
  }
}

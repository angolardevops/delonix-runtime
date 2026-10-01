/** Regenerates api/openapi.json from the route schemas: `pnpm openapi:write`. */
import { mkdirSync, writeFileSync } from "node:fs";
import { buildApp } from "../src/app.js";
import { Readiness } from "../src/health/readiness.js";
import { createLogger } from "../src/logging.js";
import { MemoryNoteStore } from "../src/notes/memory-store.js";
import { NotesService } from "../src/notes/notes.js";
import { Dedup } from "../src/webhooks/dedup.js";

const app = await buildApp({
  logger: createLogger({ logLevel: "error", serviceName: "openapi", version: "dev", env: "test" }),
  notes: new NotesService(new MemoryNoteStore()),
  readiness: new Readiness(),
  maxBodyBytes: 1_048_576,
  // The contract documents the webhook route; it is served only when a secret is set.
  inboundWebhook: { key: Buffer.alloc(32), dedup: new Dedup(1) },
});
await app.ready();
mkdirSync("api", { recursive: true });
writeFileSync("api/openapi.json", JSON.stringify(app.swagger(), null, 2) + "\n");
await app.close();

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

/** Module → import specifiers it must never use (a prefix match). */
const RULES: Record<string, string[]> = {
  // The use cases know their two ports and node:crypto — not the transport,
  // not telemetry, not a storage adapter.
  "src/notes/notes.ts": [
    "fastify",
    "@fastify/",
    "@opentelemetry/",
    "pino",
    "node:http",
    "./",
    "../",
  ],
  // Adapters implement ports; they never import the transport.
  "src/notes/memory-store.ts": ["fastify", "../app", "../http/"],
  "src/webhooks/dispatcher.ts": ["fastify", "../app", "../http/", "../notes/memory-store"],
  // The transport reaches the use cases through NotesUseCases, never a store.
  "src/notes/routes.ts": ["./memory-store"],
  "src/config.ts": ["../", "./notes", "./webhooks", "./app", "fastify", "@opentelemetry/"],
};

const imports = (file: string): string[] =>
  [...readFileSync(file, "utf8").matchAll(/^\s*(?:import|export)\s[^;]*?from\s+"([^"]+)"/gms)].map(
    (m) => m[1] ?? "",
  );

test("dependencies point inwards (see ARCHITECTURE.md)", () => {
  for (const [file, forbidden] of Object.entries(RULES)) {
    for (const spec of imports(file)) {
      const hit = forbidden.find((f) => spec === f || spec.startsWith(f));
      assert.equal(hit, undefined, `${file} imports "${spec}" — forbidden by the dependency rules`);
    }
  }
});

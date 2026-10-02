import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { describe, expect, it } from "vitest";

// Dependency direction as a test, not a convention (ARCHITECTURE.md):
//
//   app/ (routes, pages) → lib/http → lib/server (adapters, runtime) → lib/notes (rules)
//
// Imports are read from the source text; nothing is executed.
const ROOT = process.cwd();

function sources(dir: string): string[] {
  return readdirSync(join(ROOT, dir)).flatMap((entry) => {
    const path = join(dir, entry);
    if (statSync(join(ROOT, path)).isDirectory()) return sources(path);
    return /\.tsx?$/.test(entry) ? [path] : [];
  });
}

function imports(file: string): string[] {
  const source = readFileSync(join(ROOT, file), "utf8");
  return [...source.matchAll(/(?:from\s+|import\s+|import\()\s*["']([^"']+)["']/g)].map((m) => m[1]!);
}

const isClientComponent = (file: string) =>
  /^\s*["']use client["']/.test(readFileSync(join(ROOT, file), "utf8"));

/** Every violation as "file imports x", so a failure names what to fix. */
function violations(files: string[], forbidden: (specifier: string, file: string) => boolean): string[] {
  return files.flatMap((file) =>
    imports(file)
      .filter((specifier) => forbidden(specifier, file))
      .map((specifier) => `${relative(".", file)} imports ${specifier}`),
  );
}

describe("architecture", () => {
  it("lib/notes holds the rules and imports nothing but itself and the server-only marker", () => {
    const files = sources("lib/notes");
    expect(files.length).toBeGreaterThan(0);
    expect(violations(files, (s) => s !== "server-only" && !s.startsWith("./"))).toEqual([]);
  });

  it("lib/server (adapters, runtime) never imports the transport or the app", () => {
    expect(
      violations(sources("lib/server"), (s) => s.startsWith("@/app") || s.startsWith("@/lib/http")),
    ).toEqual([]);
  });

  it("only lib/server/boot.ts depends on Next.js inside lib/", () => {
    const files = [...sources("lib/server"), ...sources("lib/notes"), ...sources("lib/http")];
    expect(
      violations(
        files,
        (s, file) => (s === "next" || s.startsWith("next/")) && file !== "lib/server/boot.ts",
      ),
    ).toEqual([]);
  });

  it("lib/server/config.ts imports no other server module", () => {
    expect(violations(["lib/server/config.ts"], (s) => s !== "server-only" && s !== "@/lib/project")).toEqual(
      [],
    );
  });

  it("Route Handlers reach adapters through the runtime, never by constructing them", () => {
    const routes = sources("app").filter((f) => f.endsWith("route.ts"));
    expect(routes.length).toBeGreaterThan(0);
    const adapters = [
      "@/lib/server/memory-note-store",
      "@/lib/server/webhooks/dispatcher",
      "@/lib/server/telemetry",
    ];
    expect(violations(routes, (s) => adapters.includes(s))).toEqual([]);
  });

  it("Client Components import no server module", () => {
    const clients = sources("app").filter(isClientComponent);
    expect(clients.length).toBeGreaterThan(0);
    expect(
      violations(
        clients,
        (s) => s.startsWith("@/lib/server") || s.startsWith("@/lib/notes") || s.startsWith("@/lib/http"),
      ),
    ).toEqual([]);
  });

  it("every module under lib/server, lib/notes and lib/http is marked server-only", () => {
    const files = [...sources("lib/server"), ...sources("lib/notes"), ...sources("lib/http")];
    expect(files.filter((file) => !imports(file).includes("server-only"))).toEqual([]);
  });

  it("no source file reads a NEXT_PUBLIC_ variable: this project has no browser-visible configuration", () => {
    const files = [...sources("app"), ...sources("lib")];
    expect(
      files.filter((file) => /process\.env\.NEXT_PUBLIC_/.test(readFileSync(join(ROOT, file), "utf8"))),
    ).toEqual([]);
  });
});

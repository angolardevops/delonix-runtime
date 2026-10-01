import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { expect, it } from "vitest";
import { parse } from "yaml";

const API_DIR = join(process.cwd(), "app/api");
const METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/** The catch-all that answers 404 in the API's error shape; deliberately not a contract route. */
const NOT_IN_CONTRACT = ["/api/{path}"];

function routeFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((entry) => {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) return routeFiles(path);
    return entry === "route.ts" ? [path] : [];
  });
}

/** app/api/v1/notes/[id]/route.ts → "/api/v1/notes/{id}"; route groups "(x)" do not appear in the URL. */
function urlPath(file: string): string {
  const segments = relative(join(process.cwd(), "app"), file).split("/").slice(0, -1);
  return (
    "/" +
    segments
      .filter((s) => !/^\(.*\)$/.test(s))
      .map((s) => s.replace(/^\[(?:\.\.\.)?(.+)\]$/, "{$1}"))
      .join("/")
  );
}

/** The HTTP methods a route file exports: `export const GET`, `export function GET`, `export { x as GET }`. */
function exportedMethods(file: string): string[] {
  const source = readFileSync(file, "utf8");
  return METHODS.filter((m) =>
    new RegExp(`export\\s+(?:const|async\\s+function|function)\\s+${m}\\b|\\bas\\s+${m}\\b`).test(source),
  );
}

function fromCode(): string[] {
  return routeFiles(API_DIR)
    .flatMap((file) => exportedMethods(file).map((method) => `${method} ${urlPath(file)}`))
    .filter((op) => !NOT_IN_CONTRACT.some((path) => op.endsWith(` ${path}`)))
    .sort();
}

function fromContract(): string[] {
  const doc = parse(readFileSync(join(process.cwd(), "api/openapi.yaml"), "utf8")) as {
    paths: Record<string, Record<string, unknown>>;
  };
  return Object.entries(doc.paths)
    .flatMap(([path, item]) =>
      Object.keys(item)
        .filter((key) => METHODS.includes(key.toUpperCase()))
        .map((method) => `${method.toUpperCase()} ${path}`),
    )
    .sort();
}

it("api/openapi.yaml lists exactly the Route Handlers under app/api", () => {
  const code = fromCode();
  expect(code.length).toBeGreaterThan(0);
  expect(code).toEqual(fromContract());
});

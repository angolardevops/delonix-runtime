import { readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";

/**
 * The dependency rules of ARCHITECTURE.md, as a test: it fails the moment a
 * file imports in the wrong direction. Each rule names the files it covers and
 * either the only imports they may have (`allow`) or the ones they must never
 * have (`forbid`). Patterns are matched against the import specifier.
 */
interface Rule {
  why: string;
  files: RegExp;
  allow?: RegExp[];
  forbid?: RegExp[];
}

export const RULES: Rule[] = [
  {
    why: "the use cases and ports know Nest's DI marker, node:crypto and each other — no transport, telemetry or storage",
    files: /^notes\/(note|notes\.service|note\.repository|note\.publisher)\.ts$/,
    allow: [/^@nestjs\/common$/, /^node:crypto$/, /^\.\/(note|note\.repository|note\.publisher)$/],
  },
  {
    why: "a storage adapter implements a port; it does not call the transport or other adapters",
    files: /^notes\/in-memory-note\.repository\.ts$/,
    forbid: [/^express$/, /platform-express/, /^\.\.\/(http|webhooks|telemetry)\//, /controller/],
  },
  {
    why: "the transport reaches the use cases through NotesService, never a storage adapter",
    files: /^(notes\/notes\.controller|webhooks\/inbound-webhook\.controller)\.ts$/,
    forbid: [/in-memory/, /repository/],
  },
  {
    why: "webhook adapters do not depend on the HTTP transport",
    files: /^webhooks\/(signature|dedup|webhook-dispatcher)\.ts$/,
    forbid: [/^express$/, /platform-express/, /^\.\.\/http\//, /controller/],
  },
  {
    why: "configuration and logging are leaves: they import no feature",
    files: /^(config|logging)\/.*\.ts$/,
    forbid: [/^\.\.\/(notes|webhooks|http|health|telemetry)\//],
  },
  {
    why: "health does not depend on features",
    files: /^health\/.*\.ts$/,
    forbid: [/^\.\.\/(notes|webhooks)\//],
  },
];

const SRC = __dirname;
const IMPORT =
  /(?:^|\n)\s*(?:import|export)\s[^;]*?from\s+["']([^"']+)["']|(?:import|require)\(\s*["']([^"']+)["']\s*\)/g;

function sources(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      return sources(path);
    }
    return entry.name.endsWith(".ts") && !entry.name.endsWith(".spec.ts") ? [path] : [];
  });
}

export function importsOf(source: string): string[] {
  return [...source.matchAll(IMPORT)].map((match) => match[1] ?? match[2]);
}

export function violations(files: { path: string; source: string }[], rules: Rule[] = RULES): string[] {
  const found: string[] = [];
  for (const { path, source } of files) {
    for (const rule of rules.filter((r) => r.files.test(path))) {
      for (const spec of importsOf(source)) {
        const refused =
          (rule.allow !== undefined && !rule.allow.some((p) => p.test(spec))) ||
          (rule.forbid?.some((p) => p.test(spec)) ?? false);
        if (refused) {
          found.push(`src/${path} imports "${spec}" — ${rule.why}`);
        }
      }
    }
  }
  return found;
}

describe("architecture", () => {
  const files = sources(SRC).map((path) => ({
    path: relative(SRC, path).split("\\").join("/"),
    source: readFileSync(path, "utf8"),
  }));

  it("every rule covers at least one file (a renamed file must not escape its rule)", () => {
    for (const rule of RULES) {
      expect(files.some((f) => rule.files.test(f.path))).toBe(true);
    }
  });

  it("no file imports against the dependency direction", () => {
    expect(violations(files)).toEqual([]);
  });

  it("the checker itself catches a wrong import", () => {
    const bad = [{ path: "notes/notes.service.ts", source: 'import { trace } from "@opentelemetry/api";\n' }];
    expect(violations(bad)).toHaveLength(1);
  });
});

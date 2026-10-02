// The composition root: the one place that builds adapters and hands them to
// the use cases. Route Handlers and pages ask for the result with
// getRuntime(); they never construct an adapter themselves.
//
// The instance lives on globalThis because Next.js bundles instrumentation.ts
// and each route separately: a module-level variable would exist once per
// bundle, and the signal handler would drain a different object from the one
// the routes use.
import "server-only";
import { NotesService, type NotesApi } from "@/lib/notes/notes";
import { ConfigError, type Config } from "./config";
import { Lifecycle } from "./lifecycle";
import type { Logger } from "./logger";
import { MemoryNoteStore } from "./memory-note-store";
import type { Telemetry } from "./telemetry";
import { TracedNotes } from "./traced-notes";
import { Dedup } from "./webhooks/dedup";
import { WebhookDispatcher, type Scheduler } from "./webhooks/dispatcher";
import { parseSecret } from "./webhooks/signature";

export interface Runtime {
  config: Config;
  log: Logger;
  notes: NotesApi;
  lifecycle: Lifecycle;
  telemetry: Telemetry;
  /** Runs a task once the current response has been sent (`after` from next/server). */
  afterResponse: Scheduler;
  /** Set when WEBHOOK_INBOUND_SECRET is configured; the inbound endpoint is 404 otherwise. */
  inboundKey?: Buffer;
  dedup: Dedup;
  /** Set when WEBHOOK_TARGET_URL is configured. */
  dispatcher?: WebhookDispatcher;
  nowMs: () => number;
}

export interface RuntimeDeps {
  log: Logger;
  telemetry: Telemetry;
  afterResponse: Scheduler;
  fetch?: typeof fetch;
  nowMs?: () => number;
}

function secret(name: string, value: string): Buffer {
  try {
    return parseSecret(value);
  } catch (err) {
    throw new ConfigError([`${name}: ${err instanceof Error ? err.message : String(err)}`]);
  }
}

export function buildRuntime(config: Config, deps: RuntimeDeps): Runtime {
  const inboundKey = config.webhookInboundSecret
    ? secret("WEBHOOK_INBOUND_SECRET", config.webhookInboundSecret)
    : undefined;
  const dispatcher =
    config.webhookTargetUrl && config.webhookTargetSecret
      ? new WebhookDispatcher({
          url: config.webhookTargetUrl,
          key: secret("WEBHOOK_TARGET_SECRET", config.webhookTargetSecret),
          log: deps.log,
          schedule: deps.afterResponse,
          fetch: deps.fetch,
          nowMs: deps.nowMs,
        })
      : undefined;
  const service = new NotesService(new MemoryNoteStore(), dispatcher);
  return {
    config,
    log: deps.log,
    notes: new TracedNotes(service),
    lifecycle: new Lifecycle(),
    telemetry: deps.telemetry,
    afterResponse: deps.afterResponse,
    inboundKey,
    dedup: new Dedup(),
    dispatcher,
    nowMs: deps.nowMs ?? (() => Date.now()),
  };
}

const KEY = Symbol.for("delonix.init.runtime");
type Holder = { [KEY]?: Runtime };

export function setRuntime(runtime: Runtime | undefined): void {
  (globalThis as Holder)[KEY] = runtime;
}

export function getRuntime(): Runtime {
  const runtime = (globalThis as Holder)[KEY];
  if (!runtime) {
    throw new Error("the runtime is not initialised: instrumentation.ts must run before a request is served");
  }
  return runtime;
}

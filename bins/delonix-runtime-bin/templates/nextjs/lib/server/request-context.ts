// The request id of the request being served, carried by AsyncLocalStorage so
// a log line written anywhere below a Route Handler (use case, adapter) can
// name the request without every function passing it along.
//
// The storage lives on globalThis for the same reason as the runtime
// (lib/server/runtime.ts): the logger is created in the instrumentation
// bundle and the Route Handlers run in theirs; a module-level instance would
// be two instances, and the logger would never see the request.
import "server-only";
import { AsyncLocalStorage } from "node:async_hooks";

export interface RequestContext {
  requestId: string;
}

const KEY = Symbol.for("delonix.init.request-context");
type Holder = { [KEY]?: AsyncLocalStorage<RequestContext> };

function storage(): AsyncLocalStorage<RequestContext> {
  const holder = globalThis as Holder;
  return (holder[KEY] ??= new AsyncLocalStorage<RequestContext>());
}

export function runWithRequest<T>(ctx: RequestContext, fn: () => T): T {
  return storage().run(ctx, fn);
}

export function currentRequest(): RequestContext | undefined {
  return storage().getStore();
}

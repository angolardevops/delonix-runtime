// OpenTelemetry setup. It must run BEFORE Nest (and `http`) are loaded, so the
// HTTP instrumentation can patch the server: src/instrumentation.ts calls
// startTelemetry() and is loaded with `node --require` (see the `start`
// script). main.ts later asks for the handle to flush it at shutdown.
//
// Spans are always recorded in-process, so every log line of a request
// carries its trace_id even with no collector anywhere. Export (OTLP over
// HTTP, traces and metrics) is switched on only when an endpoint is
// configured through the standard variables, and never blocks startup or a
// request: a collector that is down costs dropped batches. OTEL_SDK_DISABLED
// =true turns everything off. Sampling and batching follow
// OTEL_TRACES_SAMPLER / OTEL_BSP_* as the SDK reads them.
import type { IncomingMessage } from "node:http";

import { diag, type DiagLogger, DiagLogLevel } from "@opentelemetry/api";
import { OTLPMetricExporter } from "@opentelemetry/exporter-metrics-otlp-http";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-http";
import { HttpInstrumentation } from "@opentelemetry/instrumentation-http";
import { UndiciInstrumentation } from "@opentelemetry/instrumentation-undici";
import { resourceFromAttributes } from "@opentelemetry/resources";
import { PeriodicExportingMetricReader } from "@opentelemetry/sdk-metrics";
import { NodeSDK, tracing } from "@opentelemetry/sdk-node";

import { DEFAULT_SERVICE_NAME } from "../config/app-config";

export interface TelemetryHandle {
  /** Flushes and stops the providers. */
  shutdown(): Promise<void>;
}

let handle: TelemetryHandle | undefined;

/** The telemetry started by src/instrumentation.ts, if it was loaded. */
export function telemetry(): TelemetryHandle | undefined {
  return handle;
}

/** Probes every few seconds would drown the traces that matter. */
export function isHealthProbe(url: string | undefined): boolean {
  return (url ?? "").startsWith("/api/v1/health/");
}

/** Is an OTLP endpoint configured for this signal? */
function exporting(env: NodeJS.ProcessEnv, signal: "TRACES" | "METRICS"): boolean {
  return Boolean(env.OTEL_EXPORTER_OTLP_ENDPOINT) || Boolean(env[`OTEL_EXPORTER_OTLP_${signal}_ENDPOINT`]);
}

export function startTelemetry(env: NodeJS.ProcessEnv): TelemetryHandle {
  const service = env.SERVICE_NAME?.trim() || DEFAULT_SERVICE_NAME;
  // Export failures (a collector that is down) become JSON warnings on stdout,
  // in the same shape as the application's log lines.
  diag.setLogger(jsonDiagLogger(service), DiagLogLevel.WARN);

  const sdk = new NodeSDK({
    resource: resourceFromAttributes({
      "service.name": service,
      "service.version": env.APP_VERSION?.trim() || "dev",
      "deployment.environment.name": env.APP_ENV?.trim() || "development",
    }),
    // Explicit processors and readers: without them the SDK would default to
    // an OTLP exporter aimed at localhost:4318 for every signal.
    spanProcessors: [
      exporting(env, "TRACES")
        ? new tracing.BatchSpanProcessor(new OTLPTraceExporter())
        : new tracing.NoopSpanProcessor(),
    ],
    metricReaders: exporting(env, "METRICS")
      ? [new PeriodicExportingMetricReader({ exporter: new OTLPMetricExporter() })]
      : [],
    logRecordProcessors: [],
    instrumentations: [
      new HttpInstrumentation({
        ignoreIncomingRequestHook: (req: IncomingMessage) => isHealthProbe(req.url),
      }),
      // Outbound fetch(): a client span per call and the traceparent header.
      new UndiciInstrumentation(),
    ],
  });
  sdk.start();
  handle = { shutdown: () => sdk.shutdown() };
  return handle;
}

function jsonDiagLogger(service: string): DiagLogger {
  const write =
    (level: string) =>
    (message: string, ...args: unknown[]): void => {
      const line = {
        level,
        time: new Date().toISOString(),
        service,
        msg: "telemetry: " + [message, ...args].map(brief).join(" "),
      };
      process.stdout.write(JSON.stringify(line) + "\n");
    };
  return {
    error: write("warn"),
    warn: write("warn"),
    info: write("info"),
    debug: write("debug"),
    verbose: write("debug"),
  };
}

/** The SDK reports errors as objects or as their JSON; keep the message, drop the stack. */
function brief(value: unknown): string {
  if (value instanceof Error) {
    return value.message;
  }
  if (typeof value !== "string") {
    return JSON.stringify(value);
  }
  if (value.startsWith("{")) {
    try {
      const parsed = JSON.parse(value) as { message?: unknown };
      if (typeof parsed.message === "string") {
        return parsed.message;
      }
    } catch {
      // Not JSON: use the text as it is.
    }
  }
  return value;
}

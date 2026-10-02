/**
 * OpenTelemetry, started BEFORE the application is loaded (`node --import`),
 * so the http and fetch instrumentations patch those modules before Fastify
 * and the webhook client use them.
 *
 * Spans are always recorded in-process, so every log line of a request
 * carries its trace_id even with no collector anywhere. Export (OTLP over
 * HTTP) is switched on only when an endpoint is configured through the
 * standard variables, and a collector that is down costs dropped batches, not
 * requests. OTEL_SDK_DISABLED=true turns everything off. Sampling and
 * batching follow OTEL_TRACES_SAMPLER / OTEL_BSP_* as the SDK reads them.
 */
import { OTLPMetricExporter } from "@opentelemetry/exporter-metrics-otlp-http";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-http";
import { HttpInstrumentation } from "@opentelemetry/instrumentation-http";
import { UndiciInstrumentation } from "@opentelemetry/instrumentation-undici";
import { resourceFromAttributes } from "@opentelemetry/resources";
import { PeriodicExportingMetricReader } from "@opentelemetry/sdk-metrics";
import { NodeSDK } from "@opentelemetry/sdk-node";
import { BatchSpanProcessor, NoopSpanProcessor } from "@opentelemetry/sdk-trace-base";
import { SERVICE_NAME } from "./service.js";

const env = process.env;
const exporting = (signal: string): boolean =>
  Boolean(env.OTEL_EXPORTER_OTLP_ENDPOINT || env[`OTEL_EXPORTER_OTLP_${signal}_ENDPOINT`]);

const sdk = new NodeSDK({
  resource: resourceFromAttributes({
    "service.name": env.OTEL_SERVICE_NAME ?? env.SERVICE_NAME ?? SERVICE_NAME,
    "service.version": env.SERVICE_VERSION ?? "dev",
    "deployment.environment.name": env.APP_ENV ?? "development",
  }),
  // Explicit lists: left undefined, the SDK would default to an OTLP exporter
  // aimed at localhost and log a failure every batch. Without an endpoint the
  // spans are still recorded (a processor that drops them), so logs keep
  // their trace ids.
  spanProcessors: [
    exporting("TRACES") ? new BatchSpanProcessor(new OTLPTraceExporter()) : new NoopSpanProcessor(),
  ],
  metricReaders: exporting("METRICS")
    ? [new PeriodicExportingMetricReader({ exporter: new OTLPMetricExporter() })]
    : [],
  logRecordProcessors: [],
  instrumentations: [
    new HttpInstrumentation({
      ignoreIncomingRequestHook: (req) => (req.url ?? "").startsWith("/api/v1/health/"),
    }),
    new UndiciInstrumentation(),
  ],
});

sdk.start();

/** Flushes and stops telemetry within `timeoutMs`; resolves false when it could not. */
export async function shutdownTelemetry(timeoutMs: number): Promise<boolean> {
  let timer: NodeJS.Timeout | undefined;
  const timeout = new Promise<false>((resolve) => {
    timer = setTimeout(() => resolve(false), timeoutMs);
  });
  const done = sdk.shutdown().then(
    () => true,
    () => false,
  );
  const ok = await Promise.race([done, timeout]);
  clearTimeout(timer);
  return ok;
}

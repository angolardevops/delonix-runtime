// OpenTelemetry for the Node.js runtime, started once from instrumentation.ts
// before Next.js serves a request. Next.js creates the request spans itself
// (`GET /api/v1/notes`, `executing api route (app) …`) through the global
// OpenTelemetry API; this module only registers the provider they land in.
//
// The SDK packages are used directly instead of @vercel/otel: this service
// runs on Node only, needs a handle to flush within the shutdown deadline, and
// must export nothing unless an endpoint is configured.
import "server-only";
import { metrics, type Attributes, type Context, type Link, type SpanKind } from "@opentelemetry/api";
import { OTLPMetricExporter } from "@opentelemetry/exporter-metrics-otlp-http";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-http";
import { resourceFromAttributes } from "@opentelemetry/resources";
import { MeterProvider, PeriodicExportingMetricReader } from "@opentelemetry/sdk-metrics";
import {
  BatchSpanProcessor,
  NoopSpanProcessor,
  ParentBasedSampler,
  SamplingDecision,
  TraceIdRatioBasedSampler,
  type Sampler,
  type SamplingResult,
} from "@opentelemetry/sdk-trace-base";
import { NodeTracerProvider } from "@opentelemetry/sdk-trace-node";
import type { Logger } from "./logger";

export interface Telemetry {
  /** Flushes and stops the exporters; never rejects. False when the deadline came first. */
  shutdown(timeoutMs: number): Promise<boolean>;
}

export interface TelemetryOptions {
  serviceName: string;
  version: string;
  environment: string;
  env?: Record<string, string | undefined>;
  log: Logger;
}

const HEALTH_PREFIX = "/api/v1/health/";

/**
 * Drops the probes: every few seconds they would drown the traces that
 * matter. Next.js starts the request span with `http.target` set, which is
 * what a sampler can see.
 */
export class DropHealthProbes implements Sampler {
  constructor(private readonly next: Sampler) {}

  shouldSample(
    context: Context,
    traceId: string,
    spanName: string,
    spanKind: SpanKind,
    attributes: Attributes,
    links: Link[],
  ): SamplingResult {
    const target = attributes["http.target"] ?? attributes["url.path"];
    if (typeof target === "string" && target.startsWith(HEALTH_PREFIX)) {
      return { decision: SamplingDecision.NOT_RECORD };
    }
    return this.next.shouldSample(context, traceId, spanName, spanKind, attributes, links);
  }

  toString(): string {
    return `DropHealthProbes{${this.next.toString()}}`;
  }
}

export function startTelemetry(options: TelemetryOptions): Telemetry {
  const env = options.env ?? process.env;
  const { log } = options;
  if (env.OTEL_SDK_DISABLED === "true") {
    log.info("telemetry disabled by OTEL_SDK_DISABLED");
    return { shutdown: async () => true };
  }
  const exporting = Boolean(env.OTEL_EXPORTER_OTLP_ENDPOINT || env.OTEL_EXPORTER_OTLP_TRACES_ENDPOINT);
  const ratioRaw = Number(env.OTEL_TRACES_SAMPLER_ARG ?? "1");
  const ratio = Number.isFinite(ratioRaw) && ratioRaw >= 0 && ratioRaw <= 1 ? ratioRaw : 1;
  const resource = resourceFromAttributes({
    "service.name": options.serviceName,
    "service.version": options.version,
    "deployment.environment.name": options.environment,
  });

  // Without an endpoint spans are still recorded (so log lines carry a
  // trace_id) but handed to a processor that drops them. The exporters read
  // the standard OTEL_EXPORTER_OTLP_* variables themselves.
  const tracerProvider = new NodeTracerProvider({
    resource,
    sampler: new ParentBasedSampler({ root: new DropHealthProbes(new TraceIdRatioBasedSampler(ratio)) }),
    spanProcessors: [exporting ? new BatchSpanProcessor(new OTLPTraceExporter()) : new NoopSpanProcessor()],
  });
  // Also installs the AsyncLocalStorage context manager and W3C propagation.
  tracerProvider.register();

  const meterProvider = new MeterProvider({
    resource,
    readers: exporting
      ? [
          new PeriodicExportingMetricReader({
            exporter: new OTLPMetricExporter(),
            exportIntervalMillis: 30_000,
          }),
        ]
      : [],
  });
  metrics.setGlobalMeterProvider(meterProvider);
  log.info("telemetry started", { exporting, sample_ratio: ratio });

  return {
    async shutdown(timeoutMs: number): Promise<boolean> {
      let timer: ReturnType<typeof setTimeout> | undefined;
      const deadline = new Promise<false>((resolve) => (timer = setTimeout(() => resolve(false), timeoutMs)));
      // A collector that is down loses the last batch; it does not turn a
      // clean stop into a failed one.
      const flushed = Promise.allSettled([tracerProvider.shutdown(), meterProvider.shutdown()]).then(
        (results) => {
          for (const r of results) {
            if (r.status === "rejected") log.warn("telemetry flush failed", { error: String(r.reason) });
          }
          return true as const;
        },
      );
      const done = await Promise.race([flushed, deadline]);
      clearTimeout(timer);
      if (!done) log.warn("telemetry flush did not finish before the deadline");
      return done;
    },
  };
}

import type { NextConfig } from "next";

// `standalone` makes `next build` emit .next/standalone/server.js with only the
// files the server needs; the Delonixfile ships that and nothing else.
const nextConfig: NextConfig = {
  output: "standalone",
  poweredByHeader: false,
  // Only the server imports these; keeping them out of the bundle avoids
  // bundling the OpenTelemetry SDK (it relies on Node's module loading).
  serverExternalPackages: [
    "@opentelemetry/sdk-trace-node",
    "@opentelemetry/sdk-trace-base",
    "@opentelemetry/sdk-metrics",
    "@opentelemetry/exporter-trace-otlp-http",
    "@opentelemetry/exporter-metrics-otlp-http",
  ],
};

export default nextConfig;

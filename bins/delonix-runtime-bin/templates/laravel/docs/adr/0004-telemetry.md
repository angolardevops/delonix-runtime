# 0004 — OpenTelemetry by hand: SDK, three small classes, traces only

Status: accepted (template default)

## Context
OpenTelemetry PHP offers automatic Laravel instrumentation
(`open-telemetry/opentelemetry-auto-laravel`), which needs the `opentelemetry`
PECL extension in every environment and is published as beta
(1.10.0-beta1 on 2026-09-24). The API and SDK packages are stable.

## Decision
Use the stable SDK directly: a middleware opens the server span
(`TraceRequest`), a decorator opens the use-case span (`TracedNotes`), a
Guzzle middleware opens the client span and injects `traceparent`
(`HttpClientTracing`), and the queued job carries the trace context across
the queue. Export is OTLP over HTTP with JSON encoding (no protobuf
extension), batched, flushed after the response is sent and after each job,
with a 2 s timeout. No metrics.

## Alternatives
- **Auto-instrumentation**: more spans (queries, views) for no code; a C
  extension to build into the image, CI and every laptop, and a beta package.
- **SDK metrics**: PHP shares nothing between requests in classic mode, so a
  counter would restart at every request. Derive request metrics from spans
  in the Collector (`spanmetrics` connector) or adopt Octane first.
- **OTel logs bridge**: logs already go to stdout as JSON with `trace_id`.

## Trade-off
Database queries and framework internals are not traced. A collector that is
down costs up to the export timeout of PHP time after each response (the
client already has its answer), never a failed request.

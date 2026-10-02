"""OpenTelemetry: traces and metrics over OTLP/HTTP.

- Nothing is exported unless OTEL_EXPORTER_OTLP_ENDPOINT is set (the OTLP
  exporters read it, and the other standard OTEL_* variables, themselves).
  Spans are still created, so trace ids appear in the logs.
- Export runs in background batch threads: a collector that is down costs
  dropped batches and an SDK log line, never a failed request.
- Health probes are not traced.
- Logs go through the standard `logging` module (core/logging.py) with trace
  ids, not through the OpenTelemetry logs bridge, which is not yet stable in
  the Python SDK.

`configure` must run in the process that serves (after gunicorn forks) and
before Django builds its middleware chain; config/wsgi.py calls it.
"""

from __future__ import annotations

import logging
import os
from typing import TYPE_CHECKING

from django.conf import settings
from opentelemetry import metrics, trace
from opentelemetry.instrumentation.django import DjangoInstrumentor
from opentelemetry.instrumentation.httpx import HTTPXClientInstrumentor
from opentelemetry.sdk.metrics import MeterProvider
from opentelemetry.sdk.metrics.export import MetricReader, PeriodicExportingMetricReader
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import SpanProcessor, TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor

if TYPE_CHECKING:
    from config.env import Config

log = logging.getLogger("core.telemetry")

_tracer_provider: TracerProvider | None = None
_meter_provider: MeterProvider | None = None


def configure(
    app: Config,
    span_processor: SpanProcessor | None = None,
    metric_reader: MetricReader | None = None,
) -> None:
    """Install the providers and instrument Django and httpx. Tests pass their
    own processor/reader; production gets OTLP exporters when an endpoint is
    configured. Idempotent."""
    global _tracer_provider, _meter_provider
    if _tracer_provider is not None or app.otel_disabled:
        return
    resource = Resource.create(
        {
            "service.name": app.service_name,
            "service.version": app.service_version,
            "deployment.environment.name": app.app_env,
        }
    )
    tracer_provider = TracerProvider(resource=resource)  # sampler from OTEL_TRACES_SAMPLER
    readers: list[MetricReader] = []
    if span_processor is not None:
        tracer_provider.add_span_processor(span_processor)
    elif app.otlp_endpoint:
        from opentelemetry.exporter.otlp.proto.http.metric_exporter import OTLPMetricExporter
        from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter

        tracer_provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter(timeout=5)))
        readers.append(PeriodicExportingMetricReader(OTLPMetricExporter(timeout=5)))
    if metric_reader is not None:
        readers.append(metric_reader)
    meter_provider = MeterProvider(resource=resource, metric_readers=readers)

    trace.set_tracer_provider(tracer_provider)
    metrics.set_meter_provider(meter_provider)
    # The instrumentation takes its place in MIDDLEWARE from this variable
    # only: after the probes and the request id, before the access log.
    os.environ.setdefault("OTEL_PYTHON_DJANGO_MIDDLEWARE_POSITION", str(settings.TRACING_MIDDLEWARE_POSITION))
    DjangoInstrumentor().instrument(
        tracer_provider=tracer_provider,
        meter_provider=meter_provider,
        excluded_urls="api/v1/health/",
    )
    HTTPXClientInstrumentor().instrument(tracer_provider=tracer_provider, meter_provider=meter_provider)
    _tracer_provider, _meter_provider = tracer_provider, meter_provider
    log.info("telemetry configured", extra={"otlp_export": bool(app.otlp_endpoint or span_processor)})


def shutdown(timeout_seconds: float) -> None:
    """Flush and stop the exporters, bounded by `timeout_seconds`."""
    millis = max(int(timeout_seconds * 1000), 1)
    if _tracer_provider is not None:
        _tracer_provider.force_flush(millis)
        _tracer_provider.shutdown()
    if _meter_provider is not None:
        _meter_provider.shutdown(timeout_millis=millis)

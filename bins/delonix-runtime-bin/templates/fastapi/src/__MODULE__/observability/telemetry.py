"""OpenTelemetry traces and metrics.

Spans are always recorded in-process, so every log line of a request carries
its `trace_id` even with no collector anywhere. Export (OTLP over HTTP) is on
only when an endpoint is configured through the standard variables
(`OTEL_EXPORTER_OTLP_ENDPOINT`, or the per-signal `..._TRACES_ENDPOINT` /
`..._METRICS_ENDPOINT`), and never blocks a request: spans and metrics leave
from background threads, so a collector that is down costs dropped batches.
`OTEL_SDK_DISABLED=true` turns everything off. Sampling and batching follow
`OTEL_TRACES_SAMPLER` / `OTEL_BSP_*` as the SDK reads them. Maturity: traces and
metrics are stable in the Python SDK; logs stay on `logging` (the OTel logs
SDK is still experimental in Python).
"""

from __future__ import annotations

import logging
import os
import threading
from dataclasses import dataclass

from opentelemetry import metrics, trace
from opentelemetry.metrics import Meter, MeterProvider, NoOpMeterProvider
from opentelemetry.sdk.metrics import MeterProvider as SdkMeterProvider
from opentelemetry.sdk.metrics.export import PeriodicExportingMetricReader
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider as SdkTracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from opentelemetry.trace import NoOpTracerProvider, Tracer, TracerProvider

log = logging.getLogger(__name__)


def _exporting(signal: str) -> bool:
    return bool(
        os.environ.get("OTEL_EXPORTER_OTLP_ENDPOINT")
        or os.environ.get(f"OTEL_EXPORTER_OTLP_{signal}_ENDPOINT")
    )


@dataclass
class Telemetry:
    tracer_provider: TracerProvider
    meter_provider: MeterProvider

    def tracer(self, name: str) -> Tracer:
        return self.tracer_provider.get_tracer(name)

    def meter(self, name: str) -> Meter:
        return self.meter_provider.get_meter(name)

    def shutdown(self, timeout: float) -> None:
        """Flush what is buffered, within `timeout` seconds. A collector that
        is down loses the last batch; it does not hold the process: the SDK's
        own shutdown retries an export for up to its exporter timeout, so it
        runs on a daemon thread that is abandoned at the deadline."""
        timeout_ms = max(1, int(timeout * 1000))
        tp, mp = self.tracer_provider, self.meter_provider

        def close() -> None:
            if isinstance(tp, SdkTracerProvider):
                tp.force_flush(timeout_millis=timeout_ms)
                tp.shutdown()
            if isinstance(mp, SdkMeterProvider):
                mp.shutdown(timeout_millis=timeout_ms)

        worker = threading.Thread(target=close, name="telemetry-shutdown", daemon=True)
        worker.start()
        worker.join(timeout)
        if worker.is_alive():
            log.warning("telemetry not flushed before the deadline; the last batch is lost")


def setup_telemetry(
    service: str, version: str, environment: str, *, install_globals: bool = True
) -> Telemetry:
    """Build the providers. `install_globals=False` is for tests, which pass
    their own providers to `create_app` instead."""
    if os.environ.get("OTEL_SDK_DISABLED", "").strip().lower() == "true":
        return Telemetry(NoOpTracerProvider(), NoOpMeterProvider())
    resource = Resource.create(
        {
            "service.name": service,
            "service.version": version,
            "deployment.environment.name": environment,
        }
    )
    # shutdown_on_exit=False: `Telemetry.shutdown` is the one, bounded, shutdown.
    tracer_provider = SdkTracerProvider(resource=resource, shutdown_on_exit=False)
    if _exporting("TRACES"):
        from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter

        tracer_provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))
    readers = []
    if _exporting("METRICS"):
        from opentelemetry.exporter.otlp.proto.http.metric_exporter import OTLPMetricExporter

        readers.append(PeriodicExportingMetricReader(OTLPMetricExporter()))
    meter_provider = SdkMeterProvider(
        resource=resource, metric_readers=readers, shutdown_on_exit=False
    )
    if install_globals:
        trace.set_tracer_provider(tracer_provider)
        metrics.set_meter_provider(meter_provider)
    return Telemetry(tracer_provider, meter_provider)

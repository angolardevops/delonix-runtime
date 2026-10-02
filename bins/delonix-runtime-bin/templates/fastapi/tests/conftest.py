"""Shared fixtures. Every test builds its own app with its own telemetry
providers, so no test depends on global OpenTelemetry state.

Imports from the package are written in the parenthesised form on purpose:
the formatter keeps it for any project name, so `make fmt-check` does not
depend on how long the name is."""

from __future__ import annotations

import base64
import json
import logging
import threading
from collections.abc import Iterator
from typing import Any

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient
from opentelemetry.sdk.metrics import MeterProvider
from opentelemetry.sdk.metrics.export import InMemoryMetricReader
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter

from __MODULE__.app import (
    create_app,
)
from __MODULE__.config import (
    Settings,
)
from __MODULE__.observability.logging import (
    JsonFormatter,
)
from __MODULE__.observability.telemetry import (
    Telemetry,
)

SECRET = "whsec_" + base64.b64encode(b"k" * 32).decode()
KEY = b"k" * 32


@pytest.fixture
def anyio_backend() -> str:
    return "asyncio"


def make_settings(**overrides: Any) -> Settings:
    """Settings from arguments only: no process environment, no .env file."""
    values: dict[str, Any] = {"app_env": "test", **overrides}
    return Settings(_env_file=None, **values)


class Spans:
    def __init__(self) -> None:
        self.exporter = InMemorySpanExporter()
        self.tracer_provider = TracerProvider()
        self.tracer_provider.add_span_processor(SimpleSpanProcessor(self.exporter))
        self.metric_reader = InMemoryMetricReader()
        self.meter_provider = MeterProvider(metric_readers=[self.metric_reader])

    @property
    def telemetry(self) -> Telemetry:
        return Telemetry(self.tracer_provider, self.meter_provider)

    def finished(self) -> list[Any]:
        return list(self.exporter.get_finished_spans())


@pytest.fixture
def spans() -> Spans:
    return Spans()


class JsonLines(logging.Handler):
    """Collects log lines exactly as production formats them."""

    def __init__(self) -> None:
        super().__init__(logging.DEBUG)
        self.setFormatter(JsonFormatter("svc", "test", "test"))
        self.lines: list[dict[str, Any]] = []
        self._lock = threading.Lock()

    def emit(self, record: logging.LogRecord) -> None:
        line = json.loads(self.format(record))
        with self._lock:
            self.lines.append(line)


@pytest.fixture
def logs() -> Iterator[JsonLines]:
    handler = JsonLines()
    root = logging.getLogger()
    previous = root.level
    root.addHandler(handler)
    root.setLevel(logging.DEBUG)
    yield handler
    root.removeHandler(handler)
    root.setLevel(previous)


@pytest.fixture
def app(spans: Spans) -> FastAPI:
    return create_app(make_settings(webhook_inbound_secret=SECRET), spans.telemetry)


@pytest.fixture
def client(app: FastAPI) -> Iterator[TestClient]:
    # raise_server_exceptions=False: assert on the 500 a client gets.
    with TestClient(app, raise_server_exceptions=False) as c:
        yield c

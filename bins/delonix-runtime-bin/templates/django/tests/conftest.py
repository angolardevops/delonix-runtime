"""Shared fixtures. Tests run with APP_ENV=test (in-memory SQLite through
pytest-django) and with telemetry recording into memory, so a test can read
the spans a request produced."""

from __future__ import annotations

import dataclasses
import os

os.environ.setdefault("APP_ENV", "test")

import pytest
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter

SPANS = InMemorySpanExporter()


@pytest.fixture(scope="session", autouse=True)
def _telemetry(django_db_setup):
    from django.conf import settings

    from core import telemetry

    telemetry.configure(settings.APP, span_processor=SimpleSpanProcessor(SPANS))


@pytest.fixture
def spans() -> InMemorySpanExporter:
    SPANS.clear()
    return SPANS


@pytest.fixture(autouse=True)
def _ready():
    """Each test starts with a process that finished starting."""
    from core import health

    health.state.reset()
    health.state.mark_ready()
    yield
    health.state.reset()


@pytest.fixture
def app_config(settings):
    """Change the parsed configuration for one test:
    `app_config(webhook_inbound_key=b"...")`."""

    def change(**values):
        settings.APP = dataclasses.replace(settings.APP, **values)
        return settings.APP

    return change

"""The HTTP contract: `python -m __MODULE__.contract` writes api/openapi.json.

The document is generated from a default app (no environment, no .env), so it
does not change with where it is generated."""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

from opentelemetry.metrics import NoOpMeterProvider
from opentelemetry.trace import NoOpTracerProvider

from .app import create_app
from .config import Settings
from .observability.telemetry import Telemetry

CONTRACT_PATH = Path(__file__).resolve().parents[2] / "api" / "openapi.json"


def generate() -> dict[str, Any]:
    settings = Settings(_env_file=None, app_env="test")
    app = create_app(settings, Telemetry(NoOpTracerProvider(), NoOpMeterProvider()))
    return app.openapi()


def render() -> str:
    return json.dumps(generate(), indent=2, sort_keys=True) + "\n"


if __name__ == "__main__":
    sys.stdout.write(render())

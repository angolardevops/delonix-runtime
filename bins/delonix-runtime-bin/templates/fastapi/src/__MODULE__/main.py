"""Application factory — wires config, routes, and lifecycle."""

from __future__ import annotations

from fastapi import FastAPI

from __MODULE__.api.router import api_router
from __MODULE__.config import get_settings


def create_app() -> FastAPI:
    settings = get_settings()
    app = FastAPI(title=settings.app_name, version="0.1.0")
    app.include_router(api_router, prefix="/api/v1")
    return app


app = create_app()

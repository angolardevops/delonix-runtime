"""API v1 router — aggregates the feature routers."""

from __future__ import annotations

from fastapi import APIRouter

from __MODULE__.api import health

api_router = APIRouter()
api_router.include_router(health.router, prefix="/health", tags=["health"])

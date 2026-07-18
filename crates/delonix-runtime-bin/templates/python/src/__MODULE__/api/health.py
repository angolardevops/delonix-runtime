"""Liveness and readiness probes (used by the Delonixfile HEALTHCHECK)."""

from __future__ import annotations

from fastapi import APIRouter

router = APIRouter()


@router.get("/live")
def live() -> dict[str, str]:
    """The process is up."""
    return {"status": "alive"}


@router.get("/ready")
def ready() -> dict[str, str]:
    """The app is ready to serve (extend with DB/cache checks)."""
    return {"status": "ready"}

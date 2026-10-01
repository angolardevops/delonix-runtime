"""Liveness and readiness probes. Not traced, logged at debug."""

from __future__ import annotations

from fastapi import APIRouter, Request, Response
from pydantic import BaseModel

from ..lifecycle import Readiness


class Status(BaseModel):
    status: str
    dependency: str | None = None


router = APIRouter(prefix="/api/v1/health", tags=["health"])


@router.get("/live", summary="Liveness: the process answers", response_model_exclude_none=True)
async def live() -> Status:
    return Status(status="alive")


@router.get(
    "/ready",
    summary="Readiness: the process should receive traffic",
    responses={503: {"model": Status, "description": "Starting, draining, or a dependency down."}},
    response_model_exclude_none=True,
)
async def ready(request: Request, response: Response) -> Status:
    readiness: Readiness = request.app.state.readiness
    if readiness.state != "ready":
        response.status_code = 503
        return Status(status=readiness.state)
    failing = await readiness.failing_dependency()
    if failing is not None:
        response.status_code = 503
        return Status(status="unavailable", dependency=failing)
    return Status(status="ready")

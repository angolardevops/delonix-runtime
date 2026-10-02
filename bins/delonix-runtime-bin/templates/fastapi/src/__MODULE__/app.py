"""Composition root: builds every adapter, hands them to the use cases and the
routers, and owns the application lifespan (start the webhook worker, mark
ready, drain on shutdown). The only module that knows every other one."""

from __future__ import annotations

import logging
import time
from collections.abc import AsyncIterator, Callable
from contextlib import asynccontextmanager

import httpx
from fastapi import FastAPI
from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor
from opentelemetry.instrumentation.httpx import HTTPXClientInstrumentor

from .config import Settings
from .lifecycle import Readiness
from .notes import api as notes_api
from .notes.memory import InMemoryNoteStore
from .notes.service import NoteService
from .observability.telemetry import Telemetry
from .observability.traced import TracedNotes
from .web import health
from .web.errors import install_error_handlers
from .web.middleware import HEALTH_PREFIX, BodyLimitMiddleware, RequestContextMiddleware
from .webhooks import api as webhooks_api
from .webhooks.dedup import Dedup
from .webhooks.dispatcher import WebhookDispatcher

log = logging.getLogger(__name__)

#: The HTTP contract version (api/openapi.json), not the package version.
API_VERSION = "1.0.0"
TITLE = "__NAME__"


def create_app(
    settings: Settings,
    telemetry: Telemetry,
    *,
    clock: Callable[[], float] = time.time,
    webhook_client: httpx.AsyncClient | None = None,
) -> FastAPI:
    readiness = Readiness()
    client: httpx.AsyncClient | None = None
    dispatcher: WebhookDispatcher | None = None
    target_key = settings.target_key
    if settings.webhook_target_url and target_key is not None:
        client = webhook_client or httpx.AsyncClient()
        # A client span per attempt, and `traceparent` on the outbound request.
        HTTPXClientInstrumentor.instrument_client(client, tracer_provider=telemetry.tracer_provider)
        dispatcher = WebhookDispatcher(
            url=settings.webhook_target_url,
            key=target_key,
            client=client,
            tracer=telemetry.tracer(f"{__package__}.webhooks"),
            meter=telemetry.meter(f"{__package__}.webhooks"),
        )
    notes = TracedNotes(
        NoteService(InMemoryNoteStore(), dispatcher), telemetry.tracer(f"{__package__}.notes")
    )

    @asynccontextmanager
    async def lifespan(_: FastAPI) -> AsyncIterator[None]:
        if dispatcher is not None:
            dispatcher.start()
        readiness.set_ready()
        log.info("ready")
        yield
        # Uvicorn runs this after in-flight requests finished (or were cut).
        # Whatever time the shutdown deadline leaves goes to the webhook queue.
        readiness.begin_drain(settings.shutdown_timeout)
        if dispatcher is not None and not await dispatcher.close(
            readiness.remaining(settings.shutdown_timeout)
        ):
            readiness.mark_interrupted()
        if client is not None:
            await client.aclose()

    docs = settings.api_docs
    app = FastAPI(
        title=TITLE,
        version=API_VERSION,
        lifespan=lifespan,
        docs_url="/docs" if docs else None,
        redoc_url="/redoc" if docs else None,
        openapi_url="/openapi.json" if docs else None,
        # FastAPI >= 0.142 has its own OpenTelemetry integration, on by default,
        # which would also attach an OTLP exporter from the environment to our
        # provider (every span exported twice). This service instruments with
        # the contrib package, which works on every accepted FastAPI version,
        # so the native one is switched off. FastAPI < 0.142 stores the
        # argument in `app.extra` and ignores it.
        telemetry={"tracing": False, "metrics": False, "logs": False, "auto_configure": False},
    )
    app.state.readiness = readiness
    app.state.notes = notes
    app.state.inbound_webhook_key = settings.inbound_key
    app.state.dedup = Dedup()
    app.state.clock = clock

    install_error_handlers(app)
    app.include_router(health.router)
    app.include_router(notes_api.router)
    app.include_router(webhooks_api.router)

    # add_middleware puts each one OUTSIDE the previous: the request context
    # wraps the body limit, so a 413 is logged and carries a request id.
    app.add_middleware(BodyLimitMiddleware, max_bytes=settings.http_max_body_bytes)
    app.add_middleware(RequestContextMiddleware, readiness=readiness)
    FastAPIInstrumentor.instrument_app(
        app,
        tracer_provider=telemetry.tracer_provider,
        meter_provider=telemetry.meter_provider,
        # Probes every few seconds would drown the traces that matter.
        excluded_urls=HEALTH_PREFIX,
        # One server span per request, not one more per ASGI message.
        exclude_spans=["receive", "send"],
    )
    return app

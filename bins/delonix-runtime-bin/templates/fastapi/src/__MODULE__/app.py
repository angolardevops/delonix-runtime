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

from .cache.cached_notes import CachedNotes
from .cache.memory import InMemoryCache
from .cache.ports import Cache
from .cache.redis import RedisCache
from .config import Settings
from .lifecycle import Readiness
from .messaging.consumer import RabbitConsumer
from .messaging.publisher import CompositePublisher, Opener, RabbitPublisher, aio_pika_opener
from .notes import api as notes_api
from .notes.memory import InMemoryNoteStore
from .notes.ports import NotePublisher
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


def _combine(publishers: list[NotePublisher]) -> NotePublisher | None:
    """The one publisher, the composite of several, or None for zero."""
    if not publishers:
        return None
    if len(publishers) == 1:
        return publishers[0]
    return CompositePublisher(publishers)


def create_app(
    settings: Settings,
    telemetry: Telemetry,
    *,
    clock: Callable[[], float] = time.time,
    webhook_client: httpx.AsyncClient | None = None,
    cache: Cache | None = None,
    rabbit_opener: Opener = aio_pika_opener,
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

    # Every new note goes to each configured publisher: the webhook dispatcher
    # (signed HTTP) and RabbitMQ (AMQP). Both are non-blocking and bounded.
    publishers: list[NotePublisher] = []
    if dispatcher is not None:
        publishers.append(dispatcher)
    rabbit_publisher: RabbitPublisher | None = None
    consumer: RabbitConsumer | None = None
    if settings.rabbitmq_url:
        meter = telemetry.meter(f"{__package__}.messaging")
        rabbit_publisher = RabbitPublisher(
            url=settings.rabbitmq_url, meter=meter, opener=rabbit_opener
        )
        publishers.append(rabbit_publisher)
        consumer = RabbitConsumer(url=settings.rabbitmq_url, meter=meter)
    publisher: NotePublisher | None = _combine(publishers)

    # Redis when configured, an in-process cache otherwise (so tests and local
    # runs need no Redis). Cache-aside wraps the use cases from the outside.
    cache_backend: Cache = cache or (
        RedisCache.from_url(settings.redis_url) if settings.redis_url else InMemoryCache()
    )
    notes = CachedNotes(
        TracedNotes(
            NoteService(InMemoryNoteStore(), publisher), telemetry.tracer(f"{__package__}.notes")
        ),
        cache_backend,
        ttl_seconds=settings.cache_ttl,
    )

    @asynccontextmanager
    async def lifespan(_: FastAPI) -> AsyncIterator[None]:
        if dispatcher is not None:
            dispatcher.start()
        if rabbit_publisher is not None:
            rabbit_publisher.start()
        if consumer is not None:
            consumer.start()
        readiness.set_ready()
        log.info("ready")
        yield
        # Uvicorn runs this after in-flight requests finished (or were cut).
        # Whatever time the shutdown deadline leaves goes to the delivery queues.
        readiness.begin_drain(settings.shutdown_timeout)
        if consumer is not None:
            await consumer.close()
        if rabbit_publisher is not None and not await rabbit_publisher.close(
            readiness.remaining(settings.shutdown_timeout)
        ):
            readiness.mark_interrupted()
        if dispatcher is not None and not await dispatcher.close(
            readiness.remaining(settings.shutdown_timeout)
        ):
            readiness.mark_interrupted()
        await cache_backend.aclose()
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

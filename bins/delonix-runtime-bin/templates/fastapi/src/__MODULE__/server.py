"""Process entry point (`python -m __MODULE__`): configuration, logging,
telemetry, the uvicorn server, signals and the exit code.

Shutdown, all within `SHUTDOWN_TIMEOUT` from the first SIGTERM/SIGINT:
readiness 503 `draining` → `DRAIN_DELAY` (still serving) → uvicorn stops
accepting and waits for in-flight requests (cancelled when the deadline
passes) → lifespan shutdown drains the webhook queue → telemetry flush.
Exit 0 when that completed, 1 when the deadline cut it. A second SIGINT skips
the wait.

Uvicorn's own handler would re-raise the signal after a graceful stop (the
process then dies by SIGTERM, exit 143); this subclass handles it instead, so
the exit code says whether the stop was complete.
"""

from __future__ import annotations

import asyncio
import logging
import signal
import socket
import sys
from types import FrameType

import uvicorn
from fastapi import FastAPI
from pydantic import ValidationError

from . import __version__
from .app import create_app
from .config import Settings
from .lifecycle import Readiness
from .observability.logging import configure_logging
from .observability.telemetry import setup_telemetry

log = logging.getLogger(__name__)

#: Upper bound, in seconds, of the final telemetry flush.
TELEMETRY_FLUSH_MAX = 5.0


class GracefulServer(uvicorn.Server):
    def __init__(self, config: uvicorn.Config, readiness: Readiness, settings: Settings) -> None:
        super().__init__(config)
        self._readiness = readiness
        self._settings = settings
        self._loop: asyncio.AbstractEventLoop | None = None
        self._stopping = False

    async def serve(self, sockets: list[socket.socket] | None = None) -> None:
        self._loop = asyncio.get_running_loop()
        await super().serve(sockets)

    def handle_exit(self, sig: int, frame: FrameType | None) -> None:
        if self._stopping:
            # A second SIGINT (Ctrl-C twice) stops waiting, as in uvicorn. A
            # repeated SIGTERM is ignored: wrappers (`uv run`, `make`, an init
            # process) often deliver the same SIGTERM twice.
            if sig == signal.SIGINT:
                self._readiness.mark_interrupted()
                self.force_exit = True
                self.should_exit = True
            return
        self._stopping = True
        # Readiness flips now, from the signal handler (plain attribute writes);
        # the log line and the delayed stop run on the event loop.
        self._readiness.begin_drain(self._settings.shutdown_timeout)
        if self._loop is None:
            self.should_exit = True
            return
        self._loop.call_soon_threadsafe(self._begin_shutdown, sig)

    def _begin_shutdown(self, sig: int) -> None:
        log.info(
            "shutting down",
            extra={
                "signal": sig,
                "timeout_s": self._settings.shutdown_timeout,
                "drain_delay_s": self._settings.drain_delay,
            },
        )
        assert self._loop is not None  # noqa: S101 — set before any signal can arrive
        self._loop.call_later(self._settings.drain_delay, self._stop_serving)

    def _stop_serving(self) -> None:
        self.should_exit = True


def _load_settings() -> Settings:
    try:
        return Settings()
    except ValidationError as exc:
        lines = []
        for err in exc.errors():
            where = ".".join(str(p) for p in err["loc"]).upper() or "settings"
            lines.append(f"  {where}: {err['msg']}")
        sys.stderr.write("configuration:\n" + "\n".join(lines) + "\n")
        raise SystemExit(1) from None


def dev_app() -> FastAPI:
    """App factory for `uvicorn --factory --reload` in development (`make dev`).
    Uvicorn's reloader owns the process there, so the drain steps of `main`
    do not apply."""
    settings = _load_settings()
    configure_logging(settings.log_level, settings.service_name, __version__, settings.app_env)
    telemetry = setup_telemetry(settings.service_name, __version__, settings.app_env)
    return create_app(settings, telemetry)


def main() -> None:
    settings = _load_settings()
    configure_logging(settings.log_level, settings.service_name, __version__, settings.app_env)
    telemetry = setup_telemetry(settings.service_name, __version__, settings.app_env)
    app = create_app(settings, telemetry)
    readiness: Readiness = app.state.readiness
    config = uvicorn.Config(
        app,
        host=settings.http_host,
        port=settings.port,
        log_config=None,  # logging is configured above (JSON on stdout)
        access_log=False,  # the access log is web.middleware's
        server_header=False,
        timeout_keep_alive=max(1, int(settings.http_keepalive_timeout)),
        # In-flight requests get what the deadline leaves after the drain delay.
        timeout_graceful_shutdown=max(1, int(settings.shutdown_timeout - settings.drain_delay)),
    )
    server = GracefulServer(config, readiness, settings)
    server.run()
    if not server.started:
        # The lifespan startup failed; uvicorn logged why.
        telemetry.shutdown(timeout=2.0)
        raise SystemExit(1)
    # What the deadline leaves, at most TELEMETRY_FLUSH_MAX: a collector that
    # is down must not hold the process until the deadline.
    left = readiness.remaining(settings.shutdown_timeout)
    telemetry.shutdown(timeout=min(TELEMETRY_FLUSH_MAX, max(1.0, left)))
    if readiness.interrupted:
        log.error("stopped before in-flight work finished")
        raise SystemExit(1)
    log.info("stopped")

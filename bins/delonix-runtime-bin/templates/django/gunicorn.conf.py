"""gunicorn configuration, read automatically from the working directory.

Lifecycle, measured with this file (README "Lifecycle"):
- SIGTERM to the master (what `delonix container stop` sends): the master
  closes its listening socket and sends SIGTERM to every worker. Each worker
  turns its readiness to 503 `draining`, keeps serving for DRAIN_DELAY, then
  stops accepting, finishes its in-flight requests, drains the outbound
  webhook queue and flushes telemetry. The master kills whatever is still
  running SHUTDOWN_TIMEOUT after the signal (`graceful_timeout`).
- SIGINT (Ctrl-C) and SIGQUIT are gunicorn's *fast* shutdown: workers exit at
  once, without draining. Use SIGTERM for an orderly stop.
"""

from __future__ import annotations

import math
import signal
import threading
import time

from config.env import ConfigError, load

try:
    _app = load()
except ConfigError as exc:
    # One line naming every invalid variable, exit status 1, no stack.
    raise SystemExit(str(exc)) from None

bind = f"{_app.host}:{_app.port}"
workers = _app.web_concurrency
# Threads per worker: requests block on the database, not on CPU.
worker_class = "gthread"
threads = _app.threads
timeout = 30
keepalive = 5
# gunicorn takes whole seconds; the master kills what is left after this.
graceful_timeout = max(1, math.ceil(_app.shutdown_timeout))
# Telemetry and the webhook thread are started per worker, after the fork
# (config/wsgi.py); preloading would start them in the master instead.
preload_app = False
# X-Forwarded-* is believed only from the proxy named in TRUSTED_PROXY.
forwarded_allow_ips = ",".join(_app.trusted_proxy) or "127.0.0.1,::1"
# gunicorn 25+ opens a local control socket by default; nothing here uses it,
# and a read-only container has nowhere to put it.
control_socket_disable = True
# The access log is the application's JSON "request" line; gunicorn's own
# messages go to stdout as JSON too.
accesslog = None
logconfig_dict = {
    "version": 1,
    "disable_existing_loggers": False,
    "filters": {
        "context": {
            "()": "core.logging.ContextFilter",
            "service": _app.service_name,
            "version": _app.service_version,
            "environment": _app.app_env,
        }
    },
    "formatters": {"json": {"()": "core.logging.JsonFormatter"}},
    "handlers": {
        "stdout": {
            "class": "logging.StreamHandler",
            "stream": "ext://sys.stdout",
            "filters": ["context"],
            "formatter": "json",
        }
    },
    "root": {"handlers": ["stdout"], "level": "INFO"},
    "loggers": {
        "gunicorn.error": {"handlers": ["stdout"], "level": "INFO", "propagate": False},
        "gunicorn.access": {"handlers": [], "propagate": False},
    },
}

_shutdown_started: list[float] = []


def post_worker_init(worker):
    """Replace the worker's SIGTERM handler: drain first, then let gunicorn's
    own graceful exit run."""
    from core import health

    graceful_exit = worker.handle_exit

    def on_sigterm(signum, frame):
        if _shutdown_started:
            return
        _shutdown_started.append(time.monotonic())
        health.state.begin_draining()
        worker.log.info("draining: readiness is 503, still serving for %ss", _app.drain_delay)
        if _app.drain_delay > 0:
            threading.Timer(_app.drain_delay, graceful_exit, args=(signum, None)).start()
        else:
            graceful_exit(signum, frame)

    signal.signal(signal.SIGTERM, on_sigterm)
    signal.siginterrupt(signal.SIGTERM, False)  # as gunicorn had it


def worker_exit(server, worker):
    """In the worker, after it stopped serving: drain the webhook queue and
    flush telemetry within what is left of SHUTDOWN_TIMEOUT."""
    from core import telemetry
    from webhooks import receivers

    started = _shutdown_started[0] if _shutdown_started else time.monotonic()
    # Keep a margin so the master's SIGKILL does not cut the flush.
    remaining = max(_app.shutdown_timeout - (time.monotonic() - started) - 0.5, 0.1)
    receivers.close(remaining * 0.7)
    left = max(_app.shutdown_timeout - (time.monotonic() - started) - 0.5, 0.1)
    telemetry.shutdown(left)

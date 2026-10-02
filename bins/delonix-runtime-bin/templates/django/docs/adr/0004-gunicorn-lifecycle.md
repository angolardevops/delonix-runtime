# 0004 — Readiness and shutdown under gunicorn

Status: accepted (template default)

## Context
The contract asks for readiness that turns 503 `draining` on SIGTERM while
in-flight requests finish, a shutdown bounded by `SHUTDOWN_TIMEOUT`, and
exporters flushed. gunicorn owns the processes and the signals: a master,
and N worker processes that each load Django.

## What gunicorn does (read in its source and documentation, then measured)
- SIGTERM to the master: it closes its copy of the listening socket, sends
  SIGTERM to every worker and waits up to `graceful_timeout`; then SIGKILL.
  A `gthread` worker on SIGTERM stops accepting and finishes in-flight
  requests.
- SIGINT/SIGQUIT to the master: quick shutdown (workers exit at once).
- The master exits 0 after a graceful stop, including when it had to kill a
  worker at the deadline.

## Decision
- `graceful_timeout = SHUTDOWN_TIMEOUT` (whole seconds, rounded up).
- `post_worker_init` replaces the worker's SIGTERM handler: readiness →
  `draining`, keep serving for `DRAIN_DELAY` (the worker still holds the
  listening socket), then call gunicorn's own graceful exit.
- `worker_exit` (runs in the worker) drains the outbound webhook queue and
  flushes telemetry within what is left of the budget, minus a 0.5 s margin.
- `preload_app = False`: Django, the telemetry SDK and the webhook thread
  are created per worker after the fork (`config/wsgi.py`). Batch exporters
  and threads do not survive a fork.
- Health probes are answered by the first middleware, so they work whatever
  the Host header and scheme.

## Measured (2026-10-01; Django 5.2/6.0/6.1, gunicorn 26.2, Python 3.13)
- `DRAIN_DELAY=3s`, 2 workers: after SIGTERM readiness answered 503
  `draining` and the API kept answering 200 for 3 s; then connections were
  refused; the master exited 0, 3.2 s after the signal.
- `DRAIN_DELAY=0s`: the listener closes almost at once, so a 503 `draining`
  is rarely observable — set a drain delay behind a load balancer that polls
  readiness.
- A webhook receiver that never answers, `SHUTDOWN_TIMEOUT=4s`: the worker
  gave the queue 2.5 s, logged `webhook queue not drained at shutdown`,
  flushed telemetry and exited; the master exited 0, 2.8 s after the signal.
- Invalid configuration: gunicorn exits 1 before listening, printing one
  line that names each variable.
Not measured here: a request still running at the deadline (the master's
SIGKILL after `graceful_timeout` is gunicorn's documented behaviour).

## Consequences
- The exit code does not say whether the deadline cut the shutdown; the logs
  do. SIGINT does not drain.
- Readiness is per worker. During `starting`, a worker is not yet accepting,
  so `starting` is practically only visible in tests and under `runserver`.
- `manage.py runserver` (development) has none of this.

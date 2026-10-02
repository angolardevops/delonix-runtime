"""WSGI entry point, loaded once per gunicorn worker (after the fork, because
`preload_app` is off) and by `manage.py runserver`.

Telemetry is configured here, before Django builds its middleware chain,
because the Django instrumentation inserts its own middleware. Doing it after
the fork matters: the SDK's batch exporters own threads, and threads do not
survive a fork.
"""

from __future__ import annotations

import os

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "config.settings")

from django.conf import settings
from django.core.wsgi import get_wsgi_application

from core import health, telemetry

telemetry.configure(settings.APP)
application = get_wsgi_application()
health.state.mark_ready()

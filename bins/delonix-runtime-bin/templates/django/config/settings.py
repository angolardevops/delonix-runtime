"""Django settings, entirely from the environment (see config/env.py).

One settings module for every environment: `APP_ENV` selects the defaults,
and production refuses the development shortcuts (a guessable SECRET_KEY, an
empty or wildcard ALLOWED_HOSTS, no DATABASE_URL). Invalid configuration
stops the process before it serves anything, with every problem listed.
"""

from __future__ import annotations

import sys
from pathlib import Path

import dj_database_url
from django.core.exceptions import ImproperlyConfigured

from config.env import ConfigError, load

BASE_DIR = Path(__file__).resolve().parent.parent

try:
    APP = load(base_dir=BASE_DIR)
except ConfigError as exc:
    # ImproperlyConfigured is what Django reports for broken settings; the
    # message names each variable.
    raise ImproperlyConfigured(str(exc)) from None

for _warning in APP.warnings:
    print(f"configuration warning: {_warning}", file=sys.stderr)

SECRET_KEY = APP.secret_key
# DEBUG stays off everywhere: this is a JSON API, errors are answered in the
# API's error shape, and Django's debug pages would leak settings.
DEBUG = False
ALLOWED_HOSTS = list(APP.allowed_hosts)

INSTALLED_APPS = [
    "notes.apps.NotesConfig",
    "webhooks.apps.WebhooksConfig",
]

MIDDLEWARE = [
    # Probes first: answered before tracing, host validation and the https
    # redirect, because a runtime probes over plain HTTP by address.
    "core.middleware.health_probes",
    "core.middleware.request_id",
    # core.telemetry.configure() inserts the OpenTelemetry middleware here
    # (TRACING_MIDDLEWARE_POSITION), so everything below runs inside the span.
    "core.middleware.access_log",
    "django.middleware.security.SecurityMiddleware",
    "django.middleware.common.CommonMiddleware",
    # Kept for any browser-facing view added later; the JSON API views are
    # csrf_exempt on purpose (see ARCHITECTURE.md, "CSRF").
    "django.middleware.csrf.CsrfViewMiddleware",
    "django.middleware.clickjacking.XFrameOptionsMiddleware",
    "core.middleware.json_errors",
]
TRACING_MIDDLEWARE_POSITION = MIDDLEWARE.index("core.middleware.access_log")

ROOT_URLCONF = "config.urls"
WSGI_APPLICATION = "config.wsgi.application"
APPEND_SLASH = False

DATABASES = {"default": dj_database_url.parse(APP.database_url, conn_max_age=60, conn_health_checks=True)}
DEFAULT_AUTO_FIELD = "django.db.models.BigAutoField"

# Request bodies above this size answer 413 body_too_large.
DATA_UPLOAD_MAX_MEMORY_SIZE = APP.max_body_bytes

USE_TZ = True
TIME_ZONE = "UTC"
USE_I18N = False

# --- Transport security -----------------------------------------------------
# Production defaults: redirect plain HTTP to https and send HSTS. Behind a
# TLS-terminating proxy, set TRUSTED_PROXY to its address(es) so the
# X-Forwarded-Proto it sets is believed; without it every request looks like
# plain HTTP and is redirected. Health probes are never redirected.
SECURE_SSL_REDIRECT = APP.ssl_redirect
SECURE_HSTS_SECONDS = APP.hsts_seconds
SECURE_HSTS_INCLUDE_SUBDOMAINS = APP.hsts_include_subdomains
SECURE_HSTS_PRELOAD = APP.hsts_preload
if APP.trusted_proxy:
    SECURE_PROXY_SSL_HEADER = ("HTTP_X_FORWARDED_PROTO", "https")
SECURE_CONTENT_TYPE_NOSNIFF = True
SECURE_REFERRER_POLICY = "same-origin"
X_FRAME_OPTIONS = "DENY"
# No session or CSRF cookie is ever set by this API; if a browser-facing view
# is added, these keep its cookies off plain HTTP.
SESSION_COOKIE_SECURE = APP.production
CSRF_COOKIE_SECURE = APP.production
# HSTS preload is a per-domain registration decision (hstspreload.org), so it
# is off by default and its deploy warning is silenced here, on purpose.
# Set SECURE_HSTS_PRELOAD=true once the domain is ready to be preloaded.
SILENCED_SYSTEM_CHECKS = ["security.W021"]

# --- Logging ----------------------------------------------------------------
# JSON lines on stdout; see core/logging.py for the fields and redaction.
LOGGING = {
    "version": 1,
    "disable_existing_loggers": False,
    "filters": {
        "context": {
            "()": "core.logging.ContextFilter",
            "service": APP.service_name,
            "version": APP.service_version,
            "environment": APP.app_env,
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
    "root": {"handlers": ["stdout"], "level": APP.log_level.upper()},
    "loggers": {
        # A refused Host header is answered 400 and logged as a "request"
        # line; Django's own ERROR with a stack per scanner hit is noise.
        "django.security.DisallowedHost": {"level": "CRITICAL"},
        "django.server": {"level": "WARNING"},
        "django.db.backends": {"level": "INFO"},
        # One ERROR per batch that could not be exported is the signal; the
        # exporter's WARNING per retry attempt is not.
        "opentelemetry.exporter": {"level": "ERROR"},
    },
}

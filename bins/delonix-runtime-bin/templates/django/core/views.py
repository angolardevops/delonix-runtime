"""Health probes and the error handlers Django calls (handler400/404/500)."""

from __future__ import annotations

from django.http import HttpRequest, JsonResponse

from core import health, http


def live(request: HttpRequest) -> JsonResponse:
    """The process answers."""
    return JsonResponse({"status": "alive"})


def ready(request: HttpRequest) -> JsonResponse:
    """The process should receive traffic: 503 while starting, while
    draining, or when the database or its migrations are not in place."""
    status, body = health.state.evaluate()
    return JsonResponse(body, status=status)


# The probes answer only GET/HEAD (see core.middleware.health_probes); the
# contract test reads this attribute as it does for api_view views.
live.allowed_methods = ["GET"]  # type: ignore[attr-defined]
ready.allowed_methods = ["GET"]  # type: ignore[attr-defined]


def bad_request(request: HttpRequest, exception: Exception | None = None) -> JsonResponse:
    # Django routes DisallowedHost and similar SuspiciousOperations here.
    return http.error(400, "bad_request", "bad request")


def not_found(request: HttpRequest, exception: Exception | None = None) -> JsonResponse:
    return http.error(404, "not_found", "no such route")


def server_error(request: HttpRequest) -> JsonResponse:
    return http.error(500, "internal", "internal error")

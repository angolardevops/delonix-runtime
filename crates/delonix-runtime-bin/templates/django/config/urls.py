"""URL routes — health probes (used by the Delonixfile HEALTHCHECK)."""
from __future__ import annotations

from django.http import JsonResponse
from django.urls import path


def live(_request):
    return JsonResponse({"status": "alive"})


def ready(_request):
    return JsonResponse({"status": "ready"})


urlpatterns = [
    path("api/v1/health/live", live),
    path("api/v1/health/ready", ready),
]

"""The route table. `api/openapi.yaml` must list exactly these routes;
tests/test_openapi.py fails on any drift."""

from __future__ import annotations

from django.urls import include, path

from core import views as core_views

urlpatterns = [
    path("api/v1/health/live", core_views.live, name="health-live"),
    path("api/v1/health/ready", core_views.ready, name="health-ready"),
    path("api/v1/notes", include("notes.urls")),
    path("api/v1/webhooks/", include("webhooks.urls")),
]

# Unknown routes and unexpected errors answer in the API's error shape.
handler400 = "core.views.bad_request"
handler404 = "core.views.not_found"
handler500 = "core.views.server_error"

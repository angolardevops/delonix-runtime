from django.urls import path

from core.http import api_view
from webhooks import views

urlpatterns = [
    path("inbound", api_view(POST=views.receive), name="webhook-inbound"),
]

from django.urls import path

from core.http import api_view
from notes import views

urlpatterns = [
    path("", api_view(GET=views.list_, POST=views.create), name="notes"),
    path("/<str:note_id>", api_view(GET=views.detail), name="note"),
]

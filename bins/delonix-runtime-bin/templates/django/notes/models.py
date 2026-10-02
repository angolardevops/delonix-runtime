from __future__ import annotations

import uuid
from typing import Any

from django.db import models


class Note(models.Model):
    """One stored note. Limits are enforced by notes.services, not here: the
    database column sizes are only a backstop."""

    id = models.UUIDField(primary_key=True, default=uuid.uuid4, editable=False)
    title = models.CharField(max_length=200)
    body = models.TextField(blank=True, default="")
    created_at = models.DateTimeField(db_index=True)

    class Meta:
        ordering = ("-created_at", "-id")

    def __str__(self) -> str:
        return self.title

    def as_dict(self) -> dict[str, Any]:
        """The public representation (API responses and webhook payloads)."""
        return {
            "id": self.id.hex,
            "title": self.title,
            "body": self.body,
            "created_at": self.created_at.isoformat().replace("+00:00", "Z"),
        }

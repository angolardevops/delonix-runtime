"""Use cases of the notes capability.

The Django ORM is the persistence here, used directly: wrapping it in a
repository interface would copy the ORM's API for no second implementation
(ARCHITECTURE.md, "The ORM in the use cases"). What this module owns is the
rules — validation, limits, paging, ordering — and the event it announces.
It imports nothing about HTTP; views call it, and so does the inbound
webhook.
"""

from __future__ import annotations

import logging
import uuid
from dataclasses import dataclass

from django.core.cache import cache
from django.db import transaction
from django.utils import timezone
from opentelemetry import trace

from notes.models import Note
from notes.signals import note_created

log = logging.getLogger("notes.services")

MAX_TITLE_LEN = 200
MAX_BODY_LEN = 10_000
MAX_PAGE_SIZE = 100
DEFAULT_PAGE_SIZE = 20

tracer = trace.get_tracer("__NAME__.notes")

# Cache key for a single note. A note is immutable once created, so a cached
# entry is never stale; the only expiry is the cache TIMEOUT (CACHE_TTL).
_CACHE_VERSION = "v1"


def _cache_key(note_id: str) -> str:
    return f"note:{_CACHE_VERSION}:{note_id}"


def _cache_get(note_id: str) -> Note | None:
    """Read a note from the cache, treating ANY backend error as a miss. The
    cache is a slow path, never a failure path: django-redis is configured to
    swallow its own errors (IGNORE_EXCEPTIONS), and this guard makes the same
    true for every backend and for an unpickleable/old-shape entry."""
    try:
        cached = cache.get(_cache_key(note_id))
    except Exception:  # a cache outage must not fail the lookup
        log.warning("cache get failed, reading the store", extra={"note_id": note_id})
        return None
    return cached if isinstance(cached, Note) else None


def _cache_set(note: Note) -> None:
    try:
        cache.set(_cache_key(note.id.hex), note)
    except Exception:  # a cache outage must not fail the write
        log.warning("cache set failed", extra={"note_id": note.id.hex})


class ValidationFailed(Exception):
    """Every invalid field at once, so a client fixes a request in one round
    trip."""

    def __init__(self, fields: dict[str, str]) -> None:
        super().__init__("invalid note: " + "; ".join(f"{k}: {v}" for k, v in fields.items()))
        self.fields = fields


class NoteNotFound(Exception):
    pass


@dataclass(frozen=True)
class Page:
    items: list[Note]
    total: int
    offset: int
    limit: int


def create_note(*, title: object, body: object = "") -> Note:
    """Validate and store a note; announce it once the transaction commits."""
    with tracer.start_as_current_span("notes.create"):
        fields: dict[str, str] = {}
        clean_title = title.strip() if isinstance(title, str) else ""
        if not isinstance(title, str):
            fields["title"] = "is required" if title is None else "must be a string"
        elif clean_title == "":
            fields["title"] = "is required"
        elif len(clean_title) > MAX_TITLE_LEN:
            fields["title"] = f"must be at most {MAX_TITLE_LEN} characters"
        if body is None:
            body = ""
        if not isinstance(body, str):
            fields["body"] = "must be a string"
        elif len(body) > MAX_BODY_LEN:
            fields["body"] = f"must be at most {MAX_BODY_LEN} characters"
        if fields:
            raise ValidationFailed(fields)
        note = Note.objects.create(title=clean_title, body=body, created_at=timezone.now())
        # Warm the cache so the first read after a create is a hit (the note is
        # immutable, so this entry stays valid for its whole TTL).
        _cache_set(note)
        # Announced only if the note is really stored: inside an outer
        # transaction (the inbound webhook) this waits for its commit.
        transaction.on_commit(lambda: note_created.send(sender=Note, note=note))
        return note


def get_note(note_id: str) -> Note:
    """Read a note, through the cache. A hit skips the database; a miss reads
    the store and warms the cache. A cache outage only makes every read a
    miss (see `_cache_get`)."""
    with tracer.start_as_current_span("notes.get"):
        try:
            key = uuid.UUID(note_id)
        except ValueError:
            raise NoteNotFound(note_id) from None
        cached = _cache_get(key.hex)
        if cached is not None:
            return cached
        try:
            note = Note.objects.get(pk=key)
        except Note.DoesNotExist:
            raise NoteNotFound(note_id) from None
        _cache_set(note)
        return note


def list_notes(*, offset: int = 0, limit: int = DEFAULT_PAGE_SIZE) -> Page:
    """One page, newest first, with the total count."""
    with tracer.start_as_current_span("notes.list"):
        if not 1 <= limit <= MAX_PAGE_SIZE:
            raise ValidationFailed({"limit": f"must be between 1 and {MAX_PAGE_SIZE}"})
        if offset < 0:
            raise ValidationFailed({"offset": "must not be negative"})
        query = Note.objects.order_by("-created_at", "-id")
        return Page(items=list(query[offset : offset + limit]), total=query.count(), offset=offset, limit=limit)

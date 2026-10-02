"""Remembers accepted delivery ids for twice the tolerance window, so a
sender's retry of a message already handled is acknowledged without being
handled twice.

In memory: it does NOT survive a restart and is not shared between replicas.
Enough against the retries of one sender to one process; a deployment that
must never process a message twice needs a unique key in a database instead.
"""

from __future__ import annotations

from collections import OrderedDict

from .signature import TOLERANCE_SECONDS


class Dedup:
    def __init__(self, max_ids: int = 4096) -> None:
        self._seen: OrderedDict[str, float] = OrderedDict()  # oldest first
        self._max = max_ids

    def first_seen(self, msg_id: str, now: float) -> bool:
        """Record `msg_id`; True when it was new."""
        while self._seen:
            oldest, at = next(iter(self._seen.items()))
            if now - at <= 2 * TOLERANCE_SECONDS:
                break
            del self._seen[oldest]
        if msg_id in self._seen:
            return False
        if len(self._seen) >= self._max:
            self._seen.popitem(last=False)  # full: evict the oldest, never refuse traffic
        self._seen[msg_id] = now
        return True

    def forget(self, msg_id: str) -> None:
        """Processing failed after `first_seen` accepted the id: a retry must be
        processed, not acknowledged as a duplicate."""
        self._seen.pop(msg_id, None)

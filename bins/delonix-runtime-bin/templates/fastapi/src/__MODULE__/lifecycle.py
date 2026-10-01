"""Whether the process should receive traffic, and how its shutdown went.

Liveness only says the process answers. Readiness moves through three states:
`starting` (the lifespan has not finished starting), `ready`, and `draining`
(a shutdown signal arrived: finish what is in flight, take nothing new). A
load balancer that polls readiness stops routing here before the listener
closes (see `DRAIN_DELAY`).
"""

from __future__ import annotations

import time
from collections.abc import Awaitable, Callable
from typing import Literal

State = Literal["starting", "ready", "draining"]

# A dependency the service cannot serve without. The example has none (the
# in-memory store cannot be down); add one when a real database is configured.
Check = Callable[[], Awaitable[bool]]


class Readiness:
    def __init__(self, checks: dict[str, Check] | None = None) -> None:
        self.state: State = "starting"
        self.checks = checks or {}
        self._deadline: float | None = None
        self.interrupted = False

    def set_ready(self) -> None:
        if self.state == "starting":
            self.state = "ready"

    def begin_drain(self, timeout: float) -> None:
        """One-way. Fixes the shutdown deadline the first time it is called."""
        self.state = "draining"
        if self._deadline is None:
            self._deadline = time.monotonic() + timeout

    def remaining(self, default: float) -> float:
        """Seconds left until the shutdown deadline (`default` if none was set)."""
        if self._deadline is None:
            return default
        return max(0.0, self._deadline - time.monotonic())

    def mark_interrupted(self) -> None:
        self.interrupted = True

    async def failing_dependency(self) -> str | None:
        for name, check in self.checks.items():
            if not await check():
                return name
        return None

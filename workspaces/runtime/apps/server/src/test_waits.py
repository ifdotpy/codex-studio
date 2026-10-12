"""Shared condition waits for server tests."""

from __future__ import annotations

from collections.abc import Callable
import time
from typing import TypeVar


HANG_GUARD_SECONDS = 60.0
POLL_INTERVAL_SECONDS = 0.05

T = TypeVar("T")


def wait_for_condition(
    condition: Callable[[], T | None],
    description: str = "condition",
    *,
    retry_exceptions: tuple[type[Exception], ...] = (),
) -> T:
    """Return a truthy condition result, failing only if it hangs for 60 seconds."""
    deadline = time.monotonic() + HANG_GUARD_SECONDS
    last_error: Exception | None = None
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            detail = f"; last error={last_error!r}" if last_error is not None else ""
            raise AssertionError(
                f"Condition did not become true within the shared "
                f"{HANG_GUARD_SECONDS:g}s hang guard: {description}{detail}"
            )

        try:
            value = condition()
        except retry_exceptions as error:
            last_error = error
            value = None
        if value:
            return value

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            continue
        time.sleep(min(POLL_INTERVAL_SECONDS, remaining))

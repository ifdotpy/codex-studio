"""Shared condition waits for server tests."""

from __future__ import annotations

from contextvars import ContextVar, Token
from collections.abc import Callable
import time
import threading
from typing import TypeVar


HANG_GUARD_SECONDS = 60.0
POLL_INTERVAL_SECONDS = 0.05

T = TypeVar("T")
_CONDITION_DEADLINE: ContextVar[float | None] = ContextVar(
    "server_test_condition_deadline", default=None,
)


def current_condition_deadline() -> float | None:
    """Return the active shared deadline for a nested request, if any."""
    return _CONDITION_DEADLINE.get()


def wait_for_condition(
    condition: Callable[[], T | None],
    description: str = "condition",
    *,
    wake_event: threading.Event | None = None,
    retry_exceptions: tuple[type[Exception], ...] = (),
    timeout_exception: type[Exception] = AssertionError,
) -> T:
    """Return a truthy condition result, failing only if it hangs for 60 seconds."""
    deadline = time.monotonic() + HANG_GUARD_SECONDS
    last_error: Exception | None = None
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            detail = f"; last error={last_error!r}" if last_error is not None else ""
            raise timeout_exception(
                f"Condition did not become true within the shared "
                f"{HANG_GUARD_SECONDS:g}s hang guard: {description}{detail}"
            )

        token: Token[float | None] = _CONDITION_DEADLINE.set(deadline)
        try:
            value = condition()
        except retry_exceptions as error:
            last_error = error
            value = None
        finally:
            _CONDITION_DEADLINE.reset(token)
        if value:
            return value

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            continue
        if wake_event is None:
            time.sleep(min(POLL_INTERVAL_SECONDS, remaining))
        else:
            wake_event.wait(min(POLL_INTERVAL_SECONDS, remaining))
            wake_event.clear()

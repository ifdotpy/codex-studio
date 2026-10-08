"""Keep immediate resource publication separate from work reconciliation."""

import threading
from collections.abc import Mapping


class SchedulerWake(threading.Event):
    """One atomic wake carries a work bit without losing a concurrent set."""

    def __init__(self) -> None:
        super().__init__()
        self._work_lock = threading.Lock()
        self._work_requested = False

    def set(self) -> None:
        with self._work_lock:
            self._work_requested = True
            super().set()

    def set_resources(self) -> None:
        with self._work_lock:
            super().set()

    def consume_dispatch(self) -> bool:
        with self._work_lock:
            work = self._work_requested
            self._work_requested = False
            super().clear()
            return work

    def clear(self) -> None:
        self.consume_dispatch()


def record_needs_dispatch(
    table: str, previous: Mapping[str, object] | None, current: Mapping[str, object],
) -> bool:
    """Unknown changes retain immediate reconciliation; only live output is exempt."""
    ignored: frozenset[str] = frozenset()
    if previous is not None and previous.get("status") == current.get("status") == "running":
        if table == "agents" and previous.get("inFlight") and current.get("inFlight"):
            # Activity advances the silence deadline; it cannot release a turn or input.
            ignored = frozenset(("events", "lastEvent", "activity", "tail", "lastAnswer"))
        elif table == "tasks":
            ignored = frozenset(("tail", "outputTruncated"))
        elif table == "monitors":
            # The monitor phase checks the current activity deadline independently.
            ignored = frozenset(("tail", "activityAt", "activityGeneration"))
    return previous is None or any(
        previous.get(field) != current.get(field) for field in (previous.keys() | current.keys()) - ignored
    )

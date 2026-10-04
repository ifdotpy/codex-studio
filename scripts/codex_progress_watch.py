"""Watch subscribed agents' exact PROGRESS.md files using native OS events."""

from __future__ import annotations

import logging
import os
from pathlib import Path
import sys
import threading
from typing import Callable, Protocol, cast
import uuid

from codex_progress import _directory, progress_path, read_progress


_logger = logging.getLogger(__name__)


WATCHDOG_JOIN_TIMEOUT_SECONDS = 5
_FILE_EVENTS = frozenset({"created", "modified", "deleted", "moved"})


def _observer_class():
    """Select a native observer explicitly; never fall back to polling."""
    if sys.platform.startswith("linux"):
        from watchdog.observers.inotify import InotifyObserver

        return InotifyObserver
    if sys.platform == "darwin":
        from watchdog.observers.fsevents import FSEventsObserver

        return FSEventsObserver
    raise RuntimeError("Progress file watching requires native Linux inotify or macOS FSEvents")


class _NativeObserver(Protocol):
    def schedule(self, event_handler: object, path: str, recursive: bool = False) -> object: ...
    def start(self) -> None: ...
    def unschedule(self, watch: object) -> None: ...
    def stop(self) -> None: ...
    def join(self, timeout: float | None = None) -> None: ...
    def is_alive(self) -> bool: ...


class _ProgressEventHandler:
    """Small watchdog event handler bound to one validated agent directory."""

    def __init__(self, watcher: ProgressFileWatchdog, agent_id: str, directory: Path):
        self._watcher = watcher
        self._agent_id = agent_id
        self._directory = directory

    def dispatch(self, event: object) -> None:
        if getattr(event, "is_directory", False) or getattr(event, "event_type", None) not in _FILE_EVENTS:
            return
        paths = (getattr(event, "src_path", ""), getattr(event, "dest_path", ""))
        expected = os.path.abspath(self._directory / "PROGRESS.md")
        if any(isinstance(path, str) and path and os.path.abspath(path) == expected for path in paths):
            self._watcher._changed(self._agent_id)


class ProgressFileWatchdog:
    """Reference-count native watches to active panel subscriptions."""

    def __init__(self, state_dir: str | Path):
        self._state_dir = Path(state_dir).absolute()
        self._condition = threading.Condition(threading.RLock())
        self._observer: _NativeObserver | None = None
        self._watches: dict[str, tuple[object, _ProgressEventHandler]] = {}
        self._subscribers: dict[str, dict[str, Callable[[], None]]] = {}
        self._states: dict[str, tuple[str | None, str | None, bool]] = {}
        self._stopping = False
        self._closed = False

    def subscribe(
        self,
        agent_id: str,
        on_change: Callable[[], None],
    ) -> Callable[[], None]:
        """Watch before reading a baseline, then publish each new file revision."""
        if not callable(on_change):
            raise TypeError("Progress watcher callback must be callable")
        path = progress_path(self._state_dir, agent_id)
        directory = path.parent
        with _directory(self._state_dir, agent_id):
            pass

        token = uuid.uuid4().hex
        with self._condition:
            while self._stopping:
                self._condition.wait(timeout=WATCHDOG_JOIN_TIMEOUT_SECONDS)
                if self._stopping:
                    raise RuntimeError("Timed out waiting for progress watcher shutdown")
            if self._closed:
                raise RuntimeError("Progress file watcher is closed")
            first = agent_id not in self._watches
            if first:
                self._attach_directory(agent_id, directory)
            self._subscribers.setdefault(agent_id, {})[token] = on_change
            if first:
                # Establish the baseline only after the native watch is live.
                baseline = read_progress(self._state_dir, agent_id)
                self._states[agent_id] = self._signature(baseline)

        detached = False
        detach_lock = threading.Lock()

        def detach() -> None:
            nonlocal detached
            with detach_lock:
                if detached:
                    return
                detached = True
            self._detach(agent_id, token)

        return detach

    def _attach_directory(self, agent_id: str, directory: Path) -> None:
        observer = self._observer
        started_here = observer is None
        if started_here:
            observer = cast(_NativeObserver, _observer_class()())
        handler = _ProgressEventHandler(self, agent_id, directory)
        watch = observer.schedule(handler, str(directory), recursive=False)
        try:
            if started_here:
                observer.start()
        except BaseException:
            observer.unschedule(watch)
            if started_here and observer.is_alive():
                observer.stop()
                observer.join(WATCHDOG_JOIN_TIMEOUT_SECONDS)
            raise
        self._observer = observer
        self._watches[agent_id] = (watch, handler)

    @staticmethod
    def _signature(current: dict) -> tuple[str | None, str | None, bool]:
        return current["revision"], current["error"], current["exists"]

    def _changed(self, agent_id: str) -> None:
        current = read_progress(self._state_dir, agent_id)
        signature = self._signature(current)
        with self._condition:
            if agent_id not in self._watches or self._states.get(agent_id) == signature:
                return
            self._states[agent_id] = signature
            callbacks = tuple(self._subscribers.get(agent_id, {}).values())
        for callback in callbacks:
            try:
                callback()
            except Exception:
                _logger.exception("Progress watcher callback failed for agent %s", agent_id)

    def _detach(self, agent_id: str, token: str) -> None:
        observer_to_update: _NativeObserver | None = None
        watch: object | None = None
        stop_observer = False
        with self._condition:
            subscribers = self._subscribers.get(agent_id)
            if subscribers is None:
                return
            subscribers.pop(token, None)
            if subscribers:
                return
            self._subscribers.pop(agent_id, None)
            watch, _ = self._watches.pop(agent_id)
            self._states.pop(agent_id, None)
            observer = self._observer
            observer_to_update = observer
            if not self._watches and observer is not None:
                self._observer = None
                self._stopping = True
                stop_observer = True
        if observer_to_update is None or watch is None:
            return
        try:
            observer_to_update.unschedule(watch)
            if stop_observer:
                observer_to_update.stop()
                observer_to_update.join(WATCHDOG_JOIN_TIMEOUT_SECONDS)
                if observer_to_update.is_alive():
                    raise RuntimeError("Progress file watcher did not stop before its deadline")
        finally:
            if stop_observer:
                with self._condition:
                    self._stopping = False
                    self._condition.notify_all()

    def close(self) -> None:
        with self._condition:
            if self._closed:
                return
            self._closed = True
            callbacks = [
                (agent_id, token)
                for agent_id, subscribers in self._subscribers.items()
                for token in subscribers
            ]
        for agent_id, token in callbacks:
            self._detach(agent_id, token)

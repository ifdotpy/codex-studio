"""Free space of the state volume: the desktop warning and the disk-full state.

macOS grows swap files into the last free space. When the volume fills, VM disks, Docker and
SQLite writes fail, so Studio warns before that point. Below the disk-full threshold Studio
starts no new turn, and an agent whose turn failed for lack of space waits (a capacity retry
with the cause ``diskFull``). The thresholds are owned by ``codex_shared_values``.
"""
from __future__ import annotations

import errno
from pathlib import Path
import re
import shutil
import sqlite3
import time
from typing import TYPE_CHECKING, Any, Literal, TypedDict

from codex_shared_values import (
    DISK_FULL_ENTER_FREE_BYTES,
    DISK_FULL_ERROR_PATTERN,
    DISK_FULL_EXIT_FREE_BYTES,
    DISK_FULL_RECHECK_SECONDS,
    DISK_SPACE_CRITICAL_FREE_BYTES,
    DISK_SPACE_WARNING_FREE_BYTES,
)

if TYPE_CHECKING:
    from codex_runtime import Runtime

GIB = 1024**3
MIB = 1024**2
WARNING_FREE_BYTES = DISK_SPACE_WARNING_FREE_BYTES
CRITICAL_FREE_BYTES = DISK_SPACE_CRITICAL_FREE_BYTES
CHECK_SECONDS = DISK_FULL_RECHECK_SECONDS
# The capacity-retry cause and the error kind of a turn that failed because the disk was full.
DISK_FULL_KIND = 'diskFull'
# A free-space sample is reused for this many seconds; dispatch can run many times a second.
SAMPLE_SECONDS = 1.0

Level = Literal['warning', 'critical', 'full']
_PATTERN = re.compile(DISK_FULL_ERROR_PATTERN, re.IGNORECASE)


class DiskSpaceStatus(TypedDict):
    level: Level | None
    freeBytes: int | None
    message: str | None


class DiskFullError(ValueError):
    """A start or a continuation waits because the disk is full."""

    def __init__(self, free: int | None) -> None:
        self.free = free
        super().__init__(held_reason(free))


def _mib(free: int | None) -> str:
    return 'unknown' if free is None else f'{max(0, free) // MIB} MiB'


def held_reason(free: int | None) -> str:
    """The reason shown for an agent that waits for free space."""
    return f'Disk is full: {_mib(free)} free. Studio resumes this agent when free space returns.'


def admission_message(free: int | None) -> str:
    """The one desktop notice while the disk is full."""
    return (f'Disk is full: {_mib(free)} free. Studio starts no new turns and resumes held agents '
            f'when {DISK_FULL_EXIT_FREE_BYTES // GIB} GiB is free. Free disk space now.')


ERROR_MESSAGE = 'Disk is full. Studio holds this agent and resumes it when free space returns.'


def is_disk_full_error(error: Any) -> bool:
    """True when an error (exception, provider error object or text) means the disk is full."""
    if isinstance(error, OSError) and error.errno in (errno.ENOSPC, errno.EDQUOT):
        return True
    if isinstance(error, sqlite3.Error) and getattr(error, 'sqlite_errorcode', None) == sqlite3.SQLITE_FULL:
        return True
    if isinstance(error, BaseException):
        cause = error.__cause__ or error.__context__
        if cause is not None and cause is not error and is_disk_full_error(cause):
            return True
        data = getattr(error, 'data', None)
        if isinstance(data, dict) and data.get('diskFull') is True:
            return True
        return bool(_PATTERN.search(str(error)))
    if isinstance(error, dict):
        info = error.get('codexErrorInfo')
        if info == DISK_FULL_KIND:
            return True
        data = error.get('data')
        if isinstance(data, dict) and data.get('diskFull') is True:
            return True
        message = error.get('message')
        return isinstance(message, str) and bool(_PATTERN.search(message))
    return isinstance(error, str) and bool(_PATTERN.search(error))


def level(free: int, full: bool = False) -> Level | None:
    if full or free < DISK_FULL_ENTER_FREE_BYTES:
        return 'full'
    if free < CRITICAL_FREE_BYTES:
        return 'critical'
    if free < WARNING_FREE_BYTES:
        return 'warning'
    return None


def message(free: int, current: Level | None) -> str | None:
    if current is None:
        return None
    if current == 'full':
        return admission_message(free)
    amount = f'{free / GIB:.1f} GB'
    if current == 'critical':
        return (f'The Mac disk has only {amount} free. VMs, Docker and Studio can fail to write. '
                'Free disk space now.')
    return f'The Mac disk has only {amount} free. Free disk space soon.'


def free_bytes(root: Path) -> int | None:
    try:
        return shutil.disk_usage(root).free
    except OSError:
        return None


def _state(rt: Any) -> dict[str, Any]:
    state: dict[str, Any] = rt.__dict__.setdefault(
        '_disk_full_state', {'full': False, 'observedUntil': 0.0, 'sampledAt': None, 'free': None})
    return state


def full_free_bytes(rt: Any, now: float | None = None) -> int | None:
    """Return the free bytes while the disk is full, else None.

    The full state starts below DISK_FULL_ENTER or after an observed disk-full error, and ends at
    or above DISK_FULL_EXIT. An unreadable volume does not block work."""
    now = time.monotonic() if now is None else now
    state = _state(rt)
    if state['sampledAt'] is None or now - state['sampledAt'] >= SAMPLE_SECONDS:
        state['free'] = free_bytes(Path(rt.root))
        state['sampledAt'] = now
    free = state['free']
    if free is None:
        return None
    if free < DISK_FULL_ENTER_FREE_BYTES:
        state['full'] = True
    elif free >= DISK_FULL_EXIT_FREE_BYTES and now >= state['observedUntil']:
        state['full'] = False
    return free if state['full'] or now < state['observedUntil'] else None


def observe_full(rt: Any, now: float | None = None) -> None:
    """Record a write that failed because the disk is full."""
    now = time.monotonic() if now is None else now
    state = _state(rt)
    state.update(full=True, observedUntil=now + DISK_FULL_RECHECK_SECONDS, sampledAt=None)


def status(root: Path, rt: Any = None, now: float | None = None) -> DiskSpaceStatus:
    if rt is not None:
        full = full_free_bytes(rt, now)
        free = _state(rt)['free']
    else:
        full, free = None, free_bytes(root)
    if free is None:
        return {'level': None, 'freeBytes': None, 'message': None}
    current = level(free, full is not None)
    return {'level': current, 'freeBytes': free, 'message': message(free, current)}


def tick(rt: "Runtime", now: float | None = None) -> None:
    """Publish the desktop resource when the warning level changes.

    /api/desktop reads the level on each request, so the first check only records it: a
    window that loads after startup already shows the current level."""
    now = time.monotonic() if now is None else now
    if now - rt.__dict__.get('_disk_space_checked', -CHECK_SECONDS) < CHECK_SECONDS:
        return
    rt.__dict__['_disk_space_checked'] = now
    current = status(Path(rt.root), rt, now)['level']
    if '_disk_space_level' not in rt.__dict__:
        rt.__dict__['_disk_space_level'] = current
        return
    if current != rt.__dict__['_disk_space_level']:
        rt.__dict__['_disk_space_level'] = current
        rt._publish_desktop_resource()


def _proven_not_applied(error: Any) -> bool:
    """The provider states that it never handed this input to the model."""
    data = getattr(error, 'data', None)
    return (isinstance(data, dict) and data.get('turnStartOutcome') == 'not_applied'
            and data.get('inputSubmitted') is not True)


def defer_start(runtime: "Runtime", agent_id: str, attempt_id: str, error: BaseException,
                *, unknown: bool = False) -> bool:
    """Return an unsent start that failed because the disk is full to the queue.

    The exact input events go back to pending with their identities. Dispatch starts them again
    once the disk-full state ends. A start that may have reached the model is not changed."""
    if unknown or not is_disk_full_error(error):
        return False
    with runtime.lock, runtime.db() as db:
        agent = runtime.agent(agent_id, db)
        attempt = agent.get('startAttempt') or {}
        if (attempt.get('id') != attempt_id or attempt.get('action')
                or (attempt.get('submitted') and not _proven_not_applied(error))
                or attempt.get('activeAtReservation')
                or attempt.get('turnId') or attempt.get('observedTurnId')
                or attempt.get('accountKey', agent.get('accountKey', 'default')) != agent.get('accountKey', 'default')
                or attempt.get('epoch') != agent['epoch'] or not agent['autoWake']
                or agent.get('deletedAt')):
            return False
        events = attempt.get('events', [])
        rows = [db.execute('SELECT status, turn_id FROM runtime_events WHERE id=? AND agent=? AND epoch=?',
                           (event_id, agent_id, agent['epoch'])).fetchone() for event_id in events]
        if any(row is None or row[0] not in {'pending', 'reserved', 'dispatching', 'uncertain'} or row[1]
               for row in rows):
            return False
        for event_id in events:
            db.execute("UPDATE runtime_events SET status='pending',error=NULL "
                       "WHERE id=? AND agent=? AND epoch=? AND status IN ('reserved','dispatching','uncertain')",
                       (event_id, agent_id, agent['epoch']))
        agent.update(status='queued', inFlight=False, error=None)  # type: ignore[call-arg]  # typed-update
        agent.pop('startAttempt', None)
        runtime.put(db, 'agents', agent)
    observe_full(runtime)
    runtime.changed.set()
    return True

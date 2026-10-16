"""Warn in the window when the volume of the state directory is almost full.

macOS grows swap files into the last free space. When the volume fills, VM disks, Docker and
SQLite writes fail, so Studio warns before that point.
"""
from __future__ import annotations

from pathlib import Path
import shutil
import time
from typing import TYPE_CHECKING, Literal, TypedDict

if TYPE_CHECKING:
    from codex_runtime import Runtime

GIB = 1024**3
WARNING_FREE_BYTES = 15 * GIB
CRITICAL_FREE_BYTES = 5 * GIB
CHECK_SECONDS = 60

Level = Literal['warning', 'critical']


class DiskSpaceStatus(TypedDict):
    level: Level | None
    freeBytes: int | None
    message: str | None


def level(free: int) -> Level | None:
    if free < CRITICAL_FREE_BYTES:
        return 'critical'
    if free < WARNING_FREE_BYTES:
        return 'warning'
    return None


def message(free: int, current: Level | None) -> str | None:
    if current is None:
        return None
    amount = f'{free / GIB:.1f} GB'
    if current == 'critical':
        return (f'The Mac disk has only {amount} free. VMs, Docker and Studio can fail to write. '
                'Free disk space now.')
    return f'The Mac disk has only {amount} free. Free disk space soon.'


def status(root: Path) -> DiskSpaceStatus:
    try:
        free = shutil.disk_usage(root).free
    except OSError:
        return {'level': None, 'freeBytes': None, 'message': None}
    current = level(free)
    return {'level': current, 'freeBytes': free, 'message': message(free, current)}


def tick(rt: "Runtime", now: float | None = None) -> None:
    """Publish the desktop resource when the warning level changes.

    /api/desktop reads the level on each request, so the first check only records it: a
    window that loads after startup already shows the current level."""
    now = time.monotonic() if now is None else now
    if now - rt.__dict__.get('_disk_space_checked', -CHECK_SECONDS) < CHECK_SECONDS:
        return
    rt.__dict__['_disk_space_checked'] = now
    current = status(Path(rt.root))['level']
    if '_disk_space_level' not in rt.__dict__:
        rt.__dict__['_disk_space_level'] = current
        return
    if current != rt.__dict__['_disk_space_level']:
        rt.__dict__['_disk_space_level'] = current
        rt._publish_desktop_resource()

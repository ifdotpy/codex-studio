"""Bounded native notifications for exact rollout files, never checkpoint proof.

One history worker owns this object. Unsupported systems and failed watches use
its existing periodic importer. No observer thread or directory scan is needed.
"""

from __future__ import annotations

import ctypes
import os
from pathlib import Path
import select
import struct
import sys
from typing import Any


MAX_HISTORY_FILE_WATCHES = 128
_INOTIFY_MASK = 0x2 | 0x4 | 0x8 | 0x400 | 0x800  # modify, attrib, close, delete, move
_INOTIFY_IGNORED = 0x8000
_INOTIFY_OVERFLOW = 0x4000


class HistoryFileNotifications:
    def __init__(self, limit: int = MAX_HISTORY_FILE_WATCHES):
        self.limit = limit
        self._queue: Any = None
        self._fd = -1
        self._libc: Any = None
        self._watches: dict[str, tuple[str, int]] = {}
        self._owners: dict[int, set[str]] = {}
        self._closed = False
        try:
            if sys.platform == "darwin":
                self._queue = select.kqueue()
            elif sys.platform.startswith("linux"):
                self._libc = ctypes.CDLL(None, use_errno=True)
                self._libc.inotify_init1.argtypes = [ctypes.c_int]
                self._libc.inotify_init1.restype = ctypes.c_int
                self._libc.inotify_add_watch.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
                self._libc.inotify_add_watch.restype = ctypes.c_int
                self._libc.inotify_rm_watch.argtypes = [ctypes.c_int, ctypes.c_int]
                self._libc.inotify_rm_watch.restype = ctypes.c_int
                self._fd = self._libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
        except (AttributeError, OSError):
            self.close()

    @property
    def enabled(self) -> bool:
        return not self._closed and (self._queue is not None or self._fd >= 0)

    def contains(self, actor: str) -> bool:
        return actor in self._watches

    def watch(self, actor: str, path: str) -> bool:
        """Attach before the next import. Return whether this is a new watch."""
        previous = self._watches.get(actor)
        if previous and previous[0] == path:
            return False
        self._remove(actor)
        if self._closed or len(self._watches) >= self.limit:
            return False
        try:
            if sys.platform == "darwin" and self._queue is not None:
                descriptor = os.open(Path(path), os.O_RDONLY | os.O_CLOEXEC)
                try:
                    flags = (select.KQ_NOTE_WRITE | select.KQ_NOTE_EXTEND | select.KQ_NOTE_ATTRIB
                             | select.KQ_NOTE_DELETE | select.KQ_NOTE_RENAME | select.KQ_NOTE_REVOKE)
                    self._queue.control([select.kevent(descriptor, filter=select.KQ_FILTER_VNODE,
                        flags=select.KQ_EV_ADD | select.KQ_EV_CLEAR, fflags=flags)], 0, 0)
                except BaseException:
                    os.close(descriptor)
                    raise
            elif self._fd >= 0:
                descriptor = self._libc.inotify_add_watch(self._fd, os.fsencode(path), _INOTIFY_MASK)
                if descriptor < 0:
                    return False
            else:
                return False
        except OSError:
            return False
        self._watches[actor] = (path, descriptor)
        self._owners.setdefault(descriptor, set()).add(actor)
        return True

    def retain(self, actors: set[str]) -> None:
        for actor in self._watches.keys() - actors:
            self._remove(actor)

    def _remove(self, actor: str) -> None:
        previous = self._watches.pop(actor, None)
        if previous is None:
            return
        descriptor = previous[1]
        owners = self._owners[descriptor]
        owners.discard(actor)
        if owners:
            return
        del self._owners[descriptor]
        if sys.platform == "darwin" and self._queue is not None:
            # Closing the owned file descriptor also removes its kqueue filter.
            os.close(descriptor)
        elif self._fd >= 0:
            self._libc.inotify_rm_watch(self._fd, descriptor)

    def collect(self, timeout: float = 0) -> set[str]:
        """Coalesce hints. Overflow or observer failure makes all watches due."""
        changed: set[str] = set()
        try:
            if sys.platform == "darwin" and self._queue is not None:
                events = self._queue.control(None, self.limit, timeout)
                for event in events:
                    actors = set(self._owners.get(event.ident, ()))
                    changed.update(actors)
                    if event.flags & select.KQ_EV_ERROR or event.fflags & (
                            select.KQ_NOTE_DELETE | select.KQ_NOTE_RENAME | select.KQ_NOTE_REVOKE):
                        for actor in actors:
                            self._remove(actor)
            elif self._fd >= 0:
                if not select.select([self._fd], [], [], timeout)[0]:
                    return changed
                # A single bounded drain is enough; remaining events keep the fd ready.
                data = os.read(self._fd, 64 * 1024)
                offset = 0
                while offset + 16 <= len(data):
                    descriptor, mask, _cookie, length = struct.unpack_from("iIII", data, offset)
                    offset += 16 + length
                    if mask & _INOTIFY_OVERFLOW:
                        changed.update(self._watches)
                    actors = set(self._owners.get(descriptor, ()))
                    changed.update(actors)
                    if mask & (_INOTIFY_IGNORED | 0x400 | 0x800):
                        for actor in actors:
                            self._remove(actor)
        except (OSError, ValueError):
            changed.update(self._watches)
            self.close()
        return changed

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        self.retain(set())
        if self._queue is not None:
            self._queue.close()
            self._queue = None
        if self._fd >= 0:
            os.close(self._fd)
            self._fd = -1

"""Size bounded app-server log writer with a fixed forensic backup window."""
from __future__ import annotations

import os
from pathlib import Path
import threading


class RotatingLog:
    def __init__(self, path, *, max_bytes=50 * 1024 * 1024, backups=4):
        self.path = Path(path)
        self.max_bytes = max(1, int(max_bytes))
        self.backups = max(0, int(backups))
        self.lock = threading.RLock()
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._trim_existing()
        self.stream = self.path.open("ab", buffering=0)
        self.size = self.path.stat().st_size

    def _backup(self, index):
        return self.path.with_name(self.path.name + "." + str(index))

    def _trim_existing(self):
        """Move an oversized old log to the first backup. A rename copies and deletes nothing."""
        if not self.path.exists() or self.path.stat().st_size <= self.max_bytes:
            return
        if not self.backups:
            with self.path.open("wb"):
                pass
            return
        self._roll_backups()
        os.replace(self.path, self._backup(1))

    def _roll_backups(self):
        try:
            self._backup(self.backups).unlink()
        except FileNotFoundError:
            pass
        for index in range(self.backups - 1, 0, -1):
            old, new = self._backup(index), self._backup(index + 1)
            if old.exists():
                os.replace(old, new)

    def _rotate(self):
        self.stream.close()
        if self.backups:
            self._roll_backups()
            if self.path.exists():
                os.replace(self.path, self._backup(1))
        else:
            self.path.unlink(missing_ok=True)
        self.stream = self.path.open("ab", buffering=0)
        self.size = 0

    def write(self, data):
        if isinstance(data, str):
            data = data.encode("utf-8", errors="replace")
        data = memoryview(data)
        written = 0
        with self.lock:
            while written < len(data):
                if self.size >= self.max_bytes:
                    self._rotate()
                count = min(len(data) - written, self.max_bytes - self.size)
                end = written + count
                while written < end:
                    amount = self.stream.write(data[written:end])
                    if not amount:
                        raise OSError("Rotating log write made no progress")
                    self.size += amount
                    written += amount
        return written

    def flush(self):
        with self.lock:
            self.stream.flush()

    def close(self):
        with self.lock:
            if not self.stream.closed:
                self.stream.close()

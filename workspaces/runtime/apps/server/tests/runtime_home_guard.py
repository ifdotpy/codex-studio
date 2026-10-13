"""Fail closed before Python tests access installed provider state or programs.

CPython does not emit audit events for stat/lstat/access: wrappers cover those
metadata APIs in addition to the audit hook. Native child internals are outside
this guard; test children must also receive the harness's isolated environment.
"""
from __future__ import annotations

import atexit
import functools
import json
import ipaddress
import socket
import os
import sys
import threading


def password_home() -> str:
    import pwd
    return pwd.getpwuid(os.getuid()).pw_dir


class HomeAccessGuard:
    def __init__(self, home_resolver=password_home, extra_roots=()):
        home = os.path.abspath(home_resolver())
        self.roots = tuple(os.path.normcase(os.path.abspath(p)) for p in (
            os.path.join(home, ".codex"), os.path.join(home, ".claude"), *extra_roots,
        ))
        self.counters = {"file_checks": 0, "exec_checks": 0, "socket_checks": 0, "blocked": 0}
        self._local = threading.local()

    def _blocked(self, path):
        return any(path == root or path.startswith(root + os.sep) for root in self.roots)

    def check_path(self, value, event="file"):
        if isinstance(value, int):
            try:
                value = os.readlink(f"/proc/self/fd/{value}")
            except OSError:
                return
        if not isinstance(value, (str, bytes, os.PathLike)):
            return
        if getattr(self._local, "resolving", False):
            # realpath calls these wrappers with absolute normalized components.
            # Check each component before its original lstat/readlink executes.
            if self._blocked(os.path.normcase(os.fsdecode(value))):
                self.counters["blocked"] += 1
                raise PermissionError(f"test isolation blocked {event}: {value}")
            return
        path = os.path.normcase(os.path.abspath(os.fsdecode(value)))
        self.counters["exec_checks" if event == "exec" else "file_checks"] += 1
        if self._blocked(path):
            self.counters["blocked"] += 1
            raise PermissionError(f"test isolation blocked {event}: {path}")
        self._local.resolving = True
        try:
            resolved = os.path.normcase(os.path.realpath(path))
        finally:
            self._local.resolving = False
        if self._blocked(resolved):
            self.counters["blocked"] += 1
            raise PermissionError(f"test isolation blocked {event}: {resolved}")

    def executable(self, executable, environment=None, cwd=None):
        if not isinstance(executable, (str, bytes, os.PathLike)):
            return
        value = os.fsdecode(executable)
        if os.path.dirname(value):
            self.check_path(os.path.join(cwd or os.getcwd(), value), "exec")
        else:
            # Check every candidate before lookup can inspect its metadata.
            for entry in (environment or os.environ).get("PATH", os.defpath).split(os.pathsep):
                self.check_path(os.path.join(cwd or os.getcwd(), entry, value), "exec")

    def audit(self, event, args):
        if event in {"open", "os.listdir", "os.scandir", "sqlite3.connect"}:
            self.check_path(args[0] if args else os.getcwd())
        elif event == "socket.connect":
            connection, address = args
            if connection.family in {socket.AF_INET, socket.AF_INET6}:
                self.counters["socket_checks"] += 1
                host = address[0]
                try:
                    allowed = ipaddress.ip_address(host).is_loopback
                except ValueError:
                    allowed = host == "localhost"
                if not allowed:
                    self.counters["blocked"] += 1
                    raise PermissionError("test isolation blocked non-loopback socket connect")
        elif event == "subprocess.Popen":
            executable, _argv, cwd, environment = args
            self.executable(executable, environment, cwd)
        elif event in {"os.exec", "os.posix_spawn"}:
            self.executable(args[0], args[2] if len(args) > 2 else None)

    def install(self):
        # No broad home exemption: interpreters and site-packages elsewhere in
        # the home work normally. An interpreter or import path beneath either
        # protected tree must be moved to an isolated environment before tests.
        for path in (sys.executable, *sys.path):
            if path and self._blocked(os.path.abspath(path)):
                raise RuntimeError("Test interpreter/site-packages is inside protected provider state; use an isolated interpreter")
        sys.addaudithook(self.audit)
        for name in ("stat", "lstat", "fstat", "access", "readlink", "open"):
            original = getattr(os, name)

            @functools.wraps(original)
            def guarded(path, *args, _original=original, **kwargs):
                if isinstance(path, (str, bytes, os.PathLike)) and kwargs.get("dir_fd") is not None:
                    base = os.readlink(f"/proc/self/fd/{kwargs['dir_fd']}")
                    self.check_path(os.path.join(base, os.fsdecode(path)))
                else:
                    self.check_path(path)
                return _original(path, *args, **kwargs)

            setattr(os, name, guarded)
        return self


_guard = None


def install_home_guard():
    global _guard
    if _guard is None:
        extra = tuple(filter(None, os.environ.get("STUDIO_TEST_FORBIDDEN_ROOTS", "").split(os.pathsep)))
        _guard = HomeAccessGuard(extra_roots=extra).install()
        report = os.environ.get("STUDIO_TEST_GUARD_REPORT")
        def write_report():
            if report:
                with open(report, "a", encoding="utf-8") as output:
                    output.write(json.dumps({"pid": os.getpid(), **_guard.counters}) + "\n")
            # A library can swallow PermissionError (e.g. Path.exists). Such an
            # attempted access must still fail the suite, after fixture cleanup.
            if _guard.counters["blocked"]:
                print("Test isolation: protected provider access was attempted", file=sys.stderr, flush=True)
                os._exit(1)
        atexit.register(write_report)
    return _guard

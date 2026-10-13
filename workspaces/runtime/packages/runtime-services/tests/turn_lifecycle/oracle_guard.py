"""Shared Python-oracle isolation, installed before application imports.

Lexical checks precede all canonicalization and metadata access, including each
recursive stat/lstat performed while resolving symlinks. Self-tests touch only a
synthetic forbidden directory beneath the caller's disposable temporary root.
"""
import os
from pathlib import Path
import pwd
import sys
import tempfile
import threading


class IsolationViolation(BaseException):
    pass


class OracleGuard:
    def __init__(self, *, block_processes=False):
        real_home = Path(pwd.getpwuid(os.getuid()).pw_dir)
        self.forbidden = {real_home / name for name in (".codex", ".claude")}
        self.block_processes = block_processes
        self.local = threading.local()
        self.stat, self.lstat = os.stat, os.lstat

    def reject_lexical(self, path):
        if isinstance(path, (str, bytes, os.PathLike)):
            absolute = Path(os.path.abspath(os.fsdecode(path)))
            if any(absolute == root or root in absolute.parents for root in self.forbidden):
                raise IsolationViolation("oracle refused access to a forbidden provider home")

    def audit(self, event, args):
        if event in {"socket.connect", "socket.bind"}:
            raise IsolationViolation("network is forbidden in oracle")
        if self.block_processes and event in {"subprocess.Popen", "os.system", "os.exec", "os.posix_spawn"}:
            raise IsolationViolation("process launch is forbidden in oracle")
        if event in {"open", "os.listdir", "os.scandir", "os.stat", "os.lstat", "os.fstatat"}:
            path = args[0] if args else None
            if isinstance(path, (str, bytes, os.PathLike)):
                self.reject_lexical(path)
                previous = getattr(self.local, "resolving", False)
                self.local.resolving = True
                try:
                    resolved = Path(os.fsdecode(path)).resolve()
                finally:
                    self.local.resolving = previous
                self.reject_lexical(resolved)

    def metadata(self, original, path, *args, **kwargs):
        self.reject_lexical(path)
        if not getattr(self.local, "resolving", False):
            self.audit("os.stat", (path,))
        return original(path, *args, **kwargs)

    def self_test(self):
        with tempfile.TemporaryDirectory(prefix="oracle-guard-") as directory:
            root = Path(directory)
            forbidden = root / "synthetic-provider-home"
            alias = root / "alias"
            alias.symlink_to(forbidden, target_is_directory=True)
            self.forbidden.add(forbidden)
            try:
                for path in (forbidden / "test", alias / "test"):
                    for operation in (open, os.stat, os.lstat, os.listdir, os.scandir):
                        try:
                            operation(path)
                        except IsolationViolation:
                            pass
                        else:
                            raise AssertionError("synthetic provider-home guard failed")
            finally:
                self.forbidden.remove(forbidden)
        print("guard refused synthetic provider-home content and metadata access")


def install_guard(*, block_processes=False):
    guard = OracleGuard(block_processes=block_processes)
    sys.addaudithook(guard.audit)
    # CPython emits no audit event for these metadata calls.
    os.stat = lambda path, *a, **kw: guard.metadata(guard.stat, path, *a, **kw)
    os.lstat = lambda path, *a, **kw: guard.metadata(guard.lstat, path, *a, **kw)
    return guard

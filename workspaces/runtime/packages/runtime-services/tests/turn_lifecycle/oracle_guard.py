"""Shared Python-oracle isolation, installed before application imports.

Lexical checks precede all canonicalization and metadata access, including each
recursive stat/lstat performed while resolving symlinks. Self-tests touch only a
synthetic forbidden directory beneath the caller's disposable temporary root.
"""
import os
from pathlib import Path
import pwd
import shutil
import sys
import tempfile
import threading


class IsolationViolation(BaseException):
    pass


def assert_isolated_path(path, variable):
    root = Path(os.environ["STUDIO_TEST_ISOLATION_ROOT"]).resolve()
    resolved = Path(path).resolve()
    if not resolved.is_relative_to(root):
        raise IsolationViolation(f"{variable} must stay under the oracle isolation root")
    return resolved


def _validate_provider_home_isolation():
    for variable in ("HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"):
        assert_isolated_path(os.environ[variable], variable)


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
    # Give every oracle process private provider homes and scratch space even
    # when the CI parent intentionally supplies no application environment.
    isolated = tempfile.TemporaryDirectory(prefix="runtime-oracle-")
    root = Path(isolated.name)
    os.environ["STUDIO_TEST_ISOLATION_ROOT"] = str(root)
    for variable, directory in (
        ("HOME", "home"),
        ("CODEX_HOME", "codex-home"),
        ("CLAUDE_CONFIG_DIR", "claude-config"),
        ("TMPDIR", "tmp"),
    ):
        path = root / directory
        path.mkdir()
        os.environ[variable] = str(path)
    _validate_provider_home_isolation()
    # A parent harness may deliberately pin a Node executable before it
    # narrows PATH (for example, the HTTP backend keeps only its replay tools
    # and system directories visible). Preserve that selected executable;
    # resolve from PATH only when no explicit selection was inherited.
    node = os.environ.get("CODEX_NODE") or shutil.which("node")
    if node:
        os.environ["CODEX_NODE"] = node
    else:
        os.environ.pop("CODEX_NODE", None)
    # Retain the TemporaryDirectory for the full lifetime of this oracle.
    global _ISOLATED_ORACLE_ENV
    _ISOLATED_ORACLE_ENV = isolated

    # Initialize the runtime guard's physical forbidden roots before the oracle
    # rejects even metadata resolution beneath those roots. Later imports reuse it.
    tests = Path(__file__).resolve().parents[4] / "apps/server/tests"
    sys.path.insert(0, str(tests))
    from runtime_home_guard import install_home_guard
    install_home_guard()
    guard = OracleGuard(block_processes=block_processes)
    # Executable discovery must not probe provider homes in the inherited PATH.
    os.environ["PATH"] = os.pathsep.join(
        entry for entry in os.environ.get("PATH", "").split(os.pathsep)
        if not any(Path(os.path.abspath(entry)).is_relative_to(root)
                   for root in guard.forbidden)
    )
    sys.addaudithook(guard.audit)
    # CPython emits no audit event for these metadata calls.
    os.stat = lambda path, *a, **kw: guard.metadata(guard.stat, path, *a, **kw)
    os.lstat = lambda path, *a, **kw: guard.metadata(guard.lstat, path, *a, **kw)
    return guard

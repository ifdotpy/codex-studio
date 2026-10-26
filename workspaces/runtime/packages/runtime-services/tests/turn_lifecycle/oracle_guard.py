"""Shared Python-oracle isolation, installed before application imports.

Lexical checks precede all canonicalization and metadata access, including each
recursive stat/lstat performed while resolving symlinks. Self-tests touch only a
synthetic forbidden directory beneath the caller's disposable temporary root.
"""
import ipaddress
import os
from pathlib import Path
import socket
import shutil
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps/server/tests"))
from runtime_home_guard import RESOLVING, install_home_guard


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


def _forms(root):
    # Resolved paths are physical (macOS /var is /private/var): block both forms.
    return {Path(os.path.abspath(root)), Path(os.path.realpath(root))}


class OracleGuard:
    def __init__(self, *, block_processes=False, allow_loopback=False):
        # Reuse both lexical and physical roots without resolving protected
        # paths through an already-installed home guard's metadata wrappers.
        self.forbidden = {Path(root) for root in install_home_guard().roots}
        self.block_processes = block_processes
        self.allow_loopback = allow_loopback
        self.stat, self.lstat = os.stat, os.lstat

    def reject_lexical(self, path):
        if isinstance(path, (str, bytes, os.PathLike)):
            absolute = os.path.abspath(os.fsdecode(path))
            # POSIX Path ancestry is exact equality or a separator-bounded prefix.
            # Avoid allocating every ancestor for every recursive metadata probe.
            for root in self.forbidden:
                prefix = os.fspath(root)
                if absolute == prefix or absolute.startswith(prefix + os.sep):
                    raise IsolationViolation("oracle refused access to a forbidden provider home")

    def audit(self, event, args):
        if event in {"socket.connect", "socket.bind"} and self.allow_loopback:
            connection, address = args
            if connection.family in {socket.AF_INET, socket.AF_INET6}:
                host = address[0]
                try:
                    loopback = ipaddress.ip_address(host).is_loopback
                except (TypeError, ValueError):
                    loopback = False
                if loopback:
                    return
        if event in {"socket.connect", "socket.bind"}:
            raise IsolationViolation("network is forbidden in oracle")
        if self.block_processes and event in {"subprocess.Popen", "os.system", "os.exec", "os.posix_spawn"}:
            raise IsolationViolation("process launch is forbidden in oracle")
        if event in {"open", "os.listdir", "os.scandir", "os.stat", "os.lstat", "os.fstatat"}:
            path = args[0] if args else None
            if isinstance(path, (str, bytes, os.PathLike)):
                self.reject_lexical(path)
                previous = getattr(RESOLVING, "active", False)
                RESOLVING.active = True
                try:
                    resolved = Path(os.fsdecode(path)).resolve()
                finally:
                    RESOLVING.active = previous
                self.reject_lexical(resolved)

    def metadata(self, original, path, *args, **kwargs):
        self.reject_lexical(path)
        if not getattr(RESOLVING, "active", False):
            self.audit("os.stat", (path,))
        return original(path, *args, **kwargs)

    def self_test(self):
        with tempfile.TemporaryDirectory(prefix="oracle-guard-") as directory:
            root = Path(directory)
            forbidden = root / "synthetic-provider-home"
            alias = root / "alias"
            alias.symlink_to(forbidden, target_is_directory=True)
            synthetic = _forms(forbidden)
            self.forbidden |= synthetic
            try:
                # A sibling sharing the name prefix is outside the forbidden tree.
                self.reject_lexical(root / "synthetic-provider-home-neighbor" / "test")
                for lexical in (forbidden, forbidden / "test", os.fsencode(forbidden / "test"),
                                root / "neighbor" / ".." / forbidden.name / "test"):
                    try:
                        self.reject_lexical(lexical)
                    except IsolationViolation:
                        pass
                    else:
                        raise AssertionError("synthetic lexical boundary guard failed")
                for path in (forbidden / "test", alias / "test"):
                    for operation in (open, os.stat, os.lstat, os.listdir, os.scandir):
                        try:
                            operation(path)
                        except IsolationViolation:
                            pass
                        else:
                            raise AssertionError("synthetic provider-home guard failed")
            finally:
                self.forbidden -= synthetic
        if self.allow_loopback:
            class LoopbackSocket:
                family = socket.AF_INET

            self.audit("socket.connect", (LoopbackSocket(), ("127.0.0.1", 1)))
            self.audit("socket.bind", (LoopbackSocket(), ("127.0.0.1", 0)))
            try:
                self.audit("socket.connect", (LoopbackSocket(), ("203.0.113.1", 1)))
            except IsolationViolation:
                pass
            else:
                raise AssertionError("oracle guard allowed a non-loopback connection")
        print("guard refused synthetic provider-home content and metadata access")


_INSTALLED_GUARD = None
_ISOLATED_ORACLE_ENV = None


def install_guard(*, block_processes=False, allow_loopback=False):
    # One guard per process, like install_home_guard. An oracle that loads
    # another oracle module installs twice; a second guard would resolve the
    # real provider homes through the first guard's patched os.lstat and be
    # refused. The stricter request wins.
    global _INSTALLED_GUARD
    if _INSTALLED_GUARD is not None:
        _INSTALLED_GUARD.block_processes |= block_processes
        _INSTALLED_GUARD.allow_loopback &= allow_loopback
        return _INSTALLED_GUARD
    # Give every oracle process private provider homes and scratch space even
    # when the CI parent intentionally supplies no application environment.
    # Keep nested HTTP fixture sockets within the macOS AF_UNIX path limit.
    isolated = tempfile.TemporaryDirectory(prefix="o-")
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
    # The home guard owns canonical roots; OracleGuard reuses them even when
    # a suite entry point installed it before importing this oracle.
    guard = OracleGuard(
        block_processes=block_processes,
        allow_loopback=allow_loopback,
    )
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
    _INSTALLED_GUARD = guard
    return guard


def close_environment():
    """Release the pre-exec interpreter's isolated homes before it is replaced."""
    if _ISOLATED_ORACLE_ENV is not None:
        _ISOLATED_ORACLE_ENV.cleanup()

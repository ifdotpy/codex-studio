#!/usr/bin/env python3
"""Discover and run isolated server-side Python suites."""

import argparse
import ast
from contextlib import contextmanager
from concurrent.futures import ThreadPoolExecutor, as_completed
import ctypes
import fnmatch
import hashlib
import json
import math
import os
from pathlib import Path
import re
import select
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time

SERVER_APP_BOOTSTRAP = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SERVER_APP_BOOTSTRAP / "src"))
from codex_layout import (  # noqa: E402
    REPOSITORY_ROOT,
    SERVER_SOURCE_ROOT,
    SERVER_TESTS_ROOT,
)

ROOT = REPOSITORY_ROOT
TESTS = SERVER_TESTS_ROOT
TESTS_REL = TESTS.relative_to(ROOT).as_posix()
SOURCE_REL = SERVER_SOURCE_ROOT.relative_to(ROOT).as_posix()
sys.path.insert(0, str(TESTS))
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

TERMINALS_SUITE = f"{TESTS_REL}/terminals-contract.py"
CODEX_BIN_SUITES = frozenset({TERMINALS_SUITE, f"{TESTS_REL}/test_isolation_contract.py"})
COMPONENT_ROOTS = (
    f"{SOURCE_REL}/analytics/tests",
    f"{SOURCE_REL}/sync/tests",
    f"{SOURCE_REL}/native_notifications/tests",
)
SCENARIO_BENCHMARK_ROOTS = (
    f"{SOURCE_REL}/sync/benchmarks/message_delivery",
    f"{SOURCE_REL}/studio_api/benchmarks/runtime_load",
)
STUDIO_API_COMPONENT_ROOT = SERVER_SOURCE_ROOT / "studio_api"
LEGACY_SERVER_JS = {
    f"{TESTS_REL}/portable-smoke.mjs": "safe",
    f"{TESTS_REL}/state-contract-smoke.mjs": "safe",
    f"{SOURCE_REL}/studio_api/benchmarks/runtime_load/test_http_outcomes.mjs": "expensive",
}
EXPLICIT_SUITES = (
    "runtime-live.py", "time-awareness.py", "tool-parity.py",
    "workspace-protocol.py", "workspace-races.py", "workspace-native-turn.py",
)
OPT_IN = {"automated", "native", "live", "expensive", "browser", "vm", "heavy"}
FAST_SUITE_BUDGET_SECONDS = 10.0
FOCUSED_SUITE_TEST_SLICES = 4
FOCUSED_MIN_TESTS_FOR_SLICING = 5
DEFAULT_TIMEOUT_SECONDS = 120
EXPENSIVE_TIMEOUT_SECONDS = 900
TEST_TMP_ROOT = Path(os.environ.get(
    "CODEX_SERVER_TEST_TMP_ROOT", Path.home() / ".cache" / "cs" / "st",
)).expanduser()
RUNNER_LOCK_PATH = Path.home() / ".cache" / "cs" / "st" / "runner-live.lock"
REPOSITORY_KEY = hashlib.sha256(str(ROOT.resolve()).encode("utf-8")).hexdigest()[:12]
PROFILE_PATH = RUNNER_LOCK_PATH.parent / f"runner-profile-{REPOSITORY_KEY}.json"
MEMORY_RESERVE_BYTES = 4 * 1024**3
FOCUSED_SUITE_LIMIT = 1
FOCUSED_SLOT_CAP = 4
NEVER_SLICE_SUITES = frozenset({
    f"{TESTS_REL}/server/test_run.py",
})
BASELINE_PATH = Path(__file__).with_name("timing-baseline.json")
TMP_ROOT_OVERRIDE = os.environ.get("CODEX_SERVER_TEST_TMP_ROOT")
MAX_SHORT_TMP_ROOT_BYTES = 42
MAX_UNIX_SOCKET_PATH_BYTES = 103
SCRATCH_PROBE_MIN_BYTES = 64 * 1024 * 1024
SCRATCH_BYTES_PER_WORKER = 256 * 1024 * 1024
SCRATCH_PROBE_CHUNK_BYTES = 8 * 1024 * 1024
SCRATCH_OWNER_FILE = ".codex-server-test-owner"
SCRATCH_LOCK_FILE = ".codex-server-test-owner.lock"
LEGACY_SCRATCH_MAX_AGE_SECONDS = 6 * 60 * 60
SCRATCH_MAINTENANCE_LOCK_FILE = ".csst-maintenance.lock"
OWNED_SCRATCH_LOCKS = {}

try:
    import resource
except ImportError:  # pragma: no cover - Windows
    resource = None

ACTIVE_SUITE_LOCK = threading.Lock()
ACTIVE_SUITE_INTERRUPTERS = set()
SUITE_OUTCOME_LOCK = threading.Lock()
EXPECTED_FAILURES = []
UNEXPECTED_SUCCESSES = []
TEST_OUTCOME_COUNTS = {}
SUITE_DURATIONS = {}
HOME_AUDIT_BLOCKED_PATHS = []
HOME_AUDIT_ISOLATED_ENV_EVENTS = []
EXPECTED_FAILURES_PATH = TESTS / "server" / "expected_failures.txt"


def resolve_codex_executable():
    """Resolve the single app-server executable used by terminal integration tests."""
    explicit = os.environ.get("CODEX_BIN")
    if explicit:
        candidate = Path(explicit).expanduser()
    else:
        # General test discovery never searches a caller's provider PATH.
        # Native suites can still select an executable explicitly with CODEX_BIN.
        located = shutil.which("codex", path=os.defpath)
        if not located:
            return None
        candidate = Path(located)
    try:
        resolved = candidate.resolve(strict=True)
        if not resolved.is_file() or not os.access(resolved, os.X_OK):
            return None
    except OSError:
        return None
    return str(resolved)


RESOLVED_CODEX_BIN = resolve_codex_executable()

NATIVE_SUITES = frozenset({
    f"{TESTS_REL}/account-transfer-native.py", f"{TESTS_REL}/agent-review-native.py",
    f"{TESTS_REL}/claude-monitor-native.py", f"{TESTS_REL}/context-repair-native.py",
    f"{TESTS_REL}/monitor-completion-native.py", f"{TESTS_REL}/monitor-continuation-native.py",
    f"{TESTS_REL}/monitor-shell-native.py", f"{TESTS_REL}/native-primitives-integration.py",
    f"{TESTS_REL}/native-safety-integration.py", f"{TESTS_REL}/native-tools-native.py",
    f"{TESTS_REL}/portable-history-native.py", f"{TESTS_REL}/session-names-native.py",
    f"{TESTS_REL}/spawn-recovery-native.py", f"{TESTS_REL}/time-awareness-native.py",
    f"{TESTS_REL}/tool-parity.py", f"{TESTS_REL}/workspace-native-turn.py",
    f"{TESTS_REL}/workspace-protocol.py",
})
VM_SUITES = frozenset({
    f"{TESTS_REL}/linux-vm-auth-native.py", f"{TESTS_REL}/linux-vm-studio-native.py",
})
BROWSER_SUITES = frozenset({
    f"{TESTS_REL}/browser-backend-smoke.py", f"{TESTS_REL}/browser-lock-contract.py",
    f"{TESTS_REL}/browser-native-contract.py", f"{TESTS_REL}/browser-recovery-contract.py",
})
LIVE_SUITES = frozenset({f"{TESTS_REL}/runtime-live.py"})
EXPENSIVE_SUITES = frozenset({
    # Real APFS images: about 118 s alone, so any parallel load passes the 120 s deadline.
    f"{TESTS_REL}/workspace-images-macos.py",
    f"{TESTS_REL}/account-costs-contract.py", f"{TESTS_REL}/analytics-memory-contract.py",
    f"{TESTS_REL}/analytics-usage-memory-contract.py",
    f"{TESTS_REL}/notification-load-contract.py",
    # The live scheduler storm is timing-sensitive and belongs in the load tier.
    f"{TESTS_REL}/notification-scheduler-contract.py",
    f"{TESTS_REL}/peer-conversion-scale-contract.py",
    f"{TESTS_REL}/session-cost-memory-contract.py",
    f"{TESTS_REL}/supervisor-stream-phase-load-contract.py",
    f"{TESTS_REL}/sync-read-latency-contract.py",
    f"{TESTS_REL}/transcript-latency-contract.py",
    f"{SOURCE_REL}/sync/benchmarks/message_delivery/test_benchmark.py",
    f"{SOURCE_REL}/studio_api/benchmarks/runtime_load/test_runtime_load.py",
})
AUTOMATED_SUITES = {
    # These short fixture-only suites add under two seconds apiece to defaults.
    f"{TESTS_REL}/payload-storage-contract.py": "safe",
    f"{TESTS_REL}/preparation-unload-contract.py": "safe",
    f"{TESTS_REL}/scheduler-disk-full-contract.py": "safe",
    f"{TESTS_REL}/session-cost-refresh-contract.py": "safe",
    f"{TESTS_REL}/session-cost-scan-contract.py": "safe",
    f"{TESTS_REL}/transcript-streaming-write-volume-contract.py": "safe",
    f"{TESTS_REL}/native-action-context-repair-contract.py": "safe",
    f"{TESTS_REL}/native-action-settings-contract.py": "safe",
    f"{TESTS_REL}/native-binary-contract.py": "safe",
    f"{TESTS_REL}/native-command-controls-contract.py": "safe",
    f"{TESTS_REL}/native-notification-lookup-contract.py": "safe",
    f"{TESTS_REL}/native-runtime-updates-contract.py": "safe",
    f"{TESTS_REL}/native-tools-contract.py": "safe",
    f"{TESTS_REL}/native-tools-shutdown-contract.py": "safe",
    f"{TESTS_REL}/native-voice-admission-contract.py": "safe",
    f"{TESTS_REL}/native-voice-contract.py": "safe",
    # The remaining opt-in suites are selected by CI without duplicating names.
    f"{TESTS_REL}/cost-scanner-contract.py": "automated",
    f"{TESTS_REL}/costs-contract.py": "automated",
    f"{TESTS_REL}/limit-payloads-contract.py": "automated",
    f"{TESTS_REL}/pricing-session-cost-contract.py": "automated",
    f"{TESTS_REL}/provider-replay-runner.py": "automated",
    f"{TESTS_REL}/native-action-receipts-contract.py": "automated",
    f"{TESTS_REL}/native-error-contract.py": "automated",
    f"{TESTS_REL}/native-release-contract.py": "automated",
    f"{TESTS_REL}/native-safety-contract.py": "automated",
    f"{TESTS_REL}/portable-history-import-http-contract.py": "automated",
    f"{TESTS_REL}/swarm-retry-contract.mjs": "automated",
}
HEAVY_SUITES = {
    f"{TESTS_REL}/agent-move-signed-integration.py":
        ("signed-in multi-server lifecycle; measured 44.13 s", 44.13),
    f"{TESTS_REL}/server-exec-signed-integration.py":
        ("signed-in server execution flow; measured 38.16 s", 38.16),
    f"{TESTS_REL}/process-supervisor-contract.py":
        ("supervisor replay wait required by contract; measured 36.05 s", 36.05),
}
if set(HEAVY_SUITES).intersection(AUTOMATED_SUITES):
    raise AssertionError("heavy suites must not also be in AUTOMATED_SUITES")
NON_TESTS = {
    f"{TESTS_REL}/test_isolation.py": "shared fixture helper",
    f"{TESTS_REL}/vm-native-fixture.py": "shared VM provider fixture helper",
    f"{TESTS_REL}/host-exec-native.py": "manual live VM proof that needs --state, --agent and --cwd",
    f"{TESTS_REL}/host-exec-transfer-bench.py": "manual host_exec transfer benchmark",
    f"{TESTS_REL}/mobile-startup-fixture.py": "browser fixture helper",
    f"{TESTS_REL}/mobile-usability-fixture.py": "browser fixture helper",
    f"{TESTS_REL}/runtime-read-lock-fixture.py": "runtime fixture helper",
    f"{TESTS_REL}/sidebar-drag-fixture.py": "browser fixture helper",
    f"{TESTS_REL}/simple-ui-fixture.py": "browser fixture helper",
    f"{TESTS_REL}/native-action-ui-fixture.py": "browser fixture helper",
    f"{TESTS_REL}/provider-replay-server.py": "provider subprocess fixture, not a test entrypoint",
    f"{TESTS_REL}/server/rpc_replay_contract.py": "shared fixture RPC allowlist",
    f"{TESTS_REL}/server/suite_entry.py": "server suite runner entrypoint",
    f"{TESTS_REL}/fixtures/current_cleanup_receipts.py": "test fixture data",
    f"{TESTS_REL}/fixtures/current_cleanup_state.py": "test fixture data",
    f"{SOURCE_REL}/sync/benchmarks/message_delivery/benchmark.py": "manual benchmark entrypoint",
    f"{SOURCE_REL}/studio_api/benchmarks/runtime_load/server.py": "load-test fixture server",
    f"{SOURCE_REL}/studio_api/benchmarks/runtime_load/run.mjs": "manual load-test entrypoint",
    f"{SOURCE_REL}/studio_api/benchmarks/runtime_load/http_outcomes.mjs": "load-test helper module",
    f"{SOURCE_REL}/studio_api/benchmarks/runtime_load/identity.mjs": "load-test helper module",
}


def category(path):
    """Return the reviewed execution class for a discovered suite path."""
    relative = path.relative_to(ROOT).as_posix()
    if relative in BROWSER_SUITES:
        return "browser"
    if relative in VM_SUITES:
        return "vm"
    if relative in NATIVE_SUITES:
        return "native"
    if relative in LIVE_SUITES:
        return "live"
    if relative in AUTOMATED_SUITES:
        return AUTOMATED_SUITES[relative]
    if relative in HEAVY_SUITES:
        return "heavy"
    if relative in EXPENSIVE_SUITES:
        return "expensive"
    return "safe"


def is_unittest_suite(path):
    """Discover unittest suites by syntax, without importing or executing them."""
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except (OSError, SyntaxError, UnicodeDecodeError):
        return False
    aliases = {"unittest"}
    testcase_names = {"TestCase"}
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            aliases.update(alias.asname or alias.name for alias in node.names
                           if alias.name == "unittest")
        elif isinstance(node, ast.ImportFrom) and node.module == "unittest":
            testcase_names.update(alias.asname or alias.name for alias in node.names
                                  if alias.name == "TestCase")
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef):
            if any(isinstance(base, ast.Name) and base.id in testcase_names
                   or isinstance(base, ast.Attribute) and base.attr == "TestCase"
                   and isinstance(base.value, ast.Name) and base.value.id in aliases
                   for base in node.bases):
                return True
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            if (node.func.attr in {"main", "TestSuite"}
                    and isinstance(node.func.value, ast.Name)
                    and node.func.value.id in aliases):
                return True
    return False


def has_module_level_mutable_state(path):
    """Avoid splitting suites with obvious module-owned mutable test state."""
    try:
        body = ast.parse(path.read_text(encoding="utf-8"), filename=str(path)).body
    except (OSError, SyntaxError, UnicodeDecodeError):
        return True
    mutable_calls = {"list", "dict", "set", "defaultdict", "deque", "Counter", "Lock",
                     "RLock", "Event", "Condition", "Queue", "Mock", "MagicMock"}
    nodes = [*body, *(child for item in body if isinstance(item, ast.ClassDef) for child in item.body)]
    for node in nodes:
        value = getattr(node, "value", None)
        if isinstance(value, (ast.List, ast.Dict, ast.Set, ast.ListComp, ast.DictComp, ast.SetComp)):
            return True
        if isinstance(value, ast.Call):
            name = getattr(value.func, "id", getattr(value.func, "attr", ""))
            if name in mutable_calls and isinstance(node, (ast.Assign, ast.AnnAssign)):
                return True
    return False


def has_custom_unittest_suite(path):
    """Avoid slicing entrypoints that manually build a suite and ignore CLI IDs."""
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except (OSError, SyntaxError, UnicodeDecodeError):
        return True
    aliases = {"unittest"}
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            aliases.update(alias.asname or alias.name for alias in node.names
                           if alias.name == "unittest")
        elif isinstance(node, ast.ImportFrom) and node.module == "unittest":
            aliases.update(alias.asname or alias.name for alias in node.names
                           if alias.name == "TestSuite")
    return any(
        isinstance(node, ast.Call)
        and isinstance(node.func, ast.Attribute)
        and node.func.attr == "TestSuite"
        and isinstance(node.func.value, ast.Name)
        and node.func.value.id in aliases
        for node in ast.walk(tree)
    )


def inventory():
    entries = {}
    for path in TESTS.rglob("*.py"):
        relative = path.relative_to(ROOT).as_posix()
        if relative in NON_TESTS or f"{TESTS_REL}/fixtures/" in relative:
            continue
        if is_unittest_suite(path):
            entries[relative] = "component" if "/tests/" in relative and relative.startswith(f"{SOURCE_REL}/") else category(path)
    # Standalone assertion scripts do not necessarily use unittest. These
    # test-file suffixes are test-entrypoint conventions; safety is decided by
    # the explicit execution-class sets above, never by the filename itself.
    for pattern in ("*-contract.py", "*_contract.py", "*-native.py", "*-integration.py", "*-smoke.py"):
        for path in TESTS.glob(pattern):
            relative = path.relative_to(ROOT).as_posix()
            if relative not in NON_TESTS:
                entries[relative] = category(path)
    for name in EXPLICIT_SUITES:
        path = TESTS / name
        if path.is_file():
            entries[path.relative_to(ROOT).as_posix()] = category(path)
    for path in (TESTS / "server").glob("test_*.py"):
        entries[path.relative_to(ROOT).as_posix()] = "component"
    for path in (SERVER_SOURCE_ROOT).glob("test_codex_*.py"):
        if is_unittest_suite(path):
            entries[path.relative_to(ROOT).as_posix()] = "component"
    for path, kind in LEGACY_SERVER_JS.items():
        if (ROOT / path).is_file():
            entries[path] = kind
    for path in AUTOMATED_SUITES:
        if (ROOT / path).is_file():
            entries[path] = category(ROOT / path)
    for path in HEAVY_SUITES:
        if (ROOT / path).is_file():
            entries[path] = category(ROOT / path)
    for directory in COMPONENT_ROOTS:
        for path in (ROOT / directory).glob("test_*.py"):
            entries[path.relative_to(ROOT).as_posix()] = "component"
    for directory in SCENARIO_BENCHMARK_ROOTS:
        for path in (ROOT / directory).glob("test_*.py"):
            entries[path.relative_to(ROOT).as_posix()] = category(path)
    # FastAPI domain tests live beside their router/model component. Discover
    # recursively so nested domains and the application verification package
    # cannot silently fall out of the default component suite.
    if STUDIO_API_COMPONENT_ROOT.is_dir():
        for path in STUDIO_API_COMPONENT_ROOT.rglob("test_*.py"):
            if path.is_file() and not path.is_symlink():
                relative = path.relative_to(ROOT).as_posix()
                entries[relative] = (
                    category(path) if relative in EXPENSIVE_SUITES else "component"
                )
    return sorted(entries.items())


def exact_entry(path):
    """Classify one known suite path without walking or parsing its neighbors."""
    try:
        relative = path.relative_to(ROOT).as_posix()
    except ValueError:
        return None
    if relative in NON_TESTS or f"{TESTS_REL}/fixtures/" in relative:
        return None
    if relative in LEGACY_SERVER_JS:
        return relative, LEGACY_SERVER_JS[relative]
    if relative in AUTOMATED_SUITES:
        return relative, category(path)
    if relative in HEAVY_SUITES:
        return relative, category(path)
    if path.parent == TESTS and path.name in EXPLICIT_SUITES:
        return relative, category(path)
    if path.suffix == ".py":
        in_tests = relative.startswith(f"{TESTS_REL}/")
        in_server_tests = relative.startswith(f"{TESTS_REL}/server/")
        if in_server_tests and path.name.startswith("test_"):
            return relative, "component"
        if in_tests and is_unittest_suite(path):
            return relative, category(path)
        if path.parent == TESTS and any(
            fnmatch.fnmatch(path.name, pattern)
            for pattern in ("*-contract.py", "*_contract.py", "*-native.py",
                            "*-integration.py", "*-smoke.py")
        ):
            return relative, category(path)
        if relative.startswith(f"{SOURCE_REL}/") and path.name.startswith("test_"):
            if relative.startswith(f"{SOURCE_REL}/studio_api/"):
                return relative, category(path) if relative in EXPENSIVE_SUITES else "component"
            if any(relative.startswith(f"{directory}/") for directory in COMPONENT_ROOTS):
                return relative, "component"
            if any(relative.startswith(f"{directory}/") for directory in SCENARIO_BENCHMARK_ROOTS):
                return relative, category(path)
            if path.parent == SERVER_SOURCE_ROOT and path.name.startswith("test_codex_") and is_unittest_suite(path):
                return relative, "component"
    return None


def select_entries(pattern):
    """Use the exact path or matching filenames before falling back to full discovery."""
    if not pattern:
        return inventory()
    has_glob = any(character in pattern for character in "*?[")
    if not has_glob and ("/" in pattern or "\\" in pattern):
        candidate = Path(pattern)
        if not candidate.is_absolute():
            candidate = ROOT / candidate
        try:
            candidate = candidate.resolve(strict=True)
        except OSError:
            pass
        else:
            if candidate.is_file():
                entry = exact_entry(candidate)
                if entry:
                    return [entry]

    query = pattern.lower()
    matches = {}
    for root in (TESTS, SERVER_SOURCE_ROOT):
        for directory, _subdirectories, filenames in os.walk(root):
            for filename in filenames:
                path = Path(directory) / filename
                relative = path.relative_to(ROOT).as_posix()
                if query not in relative.lower() and not fnmatch.fnmatch(relative.lower(), query):
                    continue
                entry = exact_entry(path)
                if entry:
                    matches[entry[0]] = entry[1]
    if matches:
        return sorted(matches.items())
    return selected(inventory(), pattern)


def selected(entries, pattern):
    if not pattern:
        return entries
    query = pattern.lower()
    return [(path, kind) for path, kind in entries
            if query in path.lower() or fnmatch.fnmatch(path.lower(), query)]


def only_category(entries, kind):
    """Select only suites assigned to one reviewed execution class."""
    if kind == "automated":
        return [(path, category) for path, category in entries if path in AUTOMATED_SUITES]
    return [(path, category) for path, category in entries if category == kind]


def exact_filter_opt_ins(pattern, entries):
    """Allow exact file-path selection of automated suites without category flags."""
    if not pattern or any(character in pattern for character in "*?["):
        return set()
    if "/" not in pattern and "\\" not in pattern:
        return set()
    candidate = Path(pattern)
    if not candidate.is_absolute():
        candidate = ROOT / candidate
    try:
        relative = candidate.resolve(strict=True).relative_to(ROOT).as_posix()
    except (OSError, ValueError):
        return set()
    if len(entries) != 1 or entries[0][0] != relative:
        return set()
    kind = entries[0][1]
    return {kind} if kind in {"automated", "heavy"} else set()


def run_process(command, cwd, timeout, environment):
    """Run one suite in its own process group and reap it on every exit path."""
    if os.name != "nt" and not _supports_waitid_nowait():
        result = _run_process_with_group_supervisor(command, cwd, timeout, environment)
        return _with_structured_outcomes(result, environment)
    options = {"start_new_session": os.name != "nt"}
    job_handle = None
    if os.name == "nt":
        # Keep the root suspended until it is assigned to the job. This closes
        # the window in which it could create children outside the job.
        options["creationflags"] = (subprocess.CREATE_NEW_PROCESS_GROUP | 0x00000004)
    process = subprocess.Popen(command, cwd=cwd, env=environment,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               **options)

    if os.name == "nt":
        job_handle = _assign_windows_job(process)

    def terminate_group():
        nonlocal job_handle
        try:
            if os.name != "nt":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                with job_lock:
                    owned_job = job_handle
                    job_handle = None
                if owned_job:
                    _close_windows_job(owned_job)
        except ProcessLookupError:
            pass
        except (OSError, subprocess.TimeoutExpired):
            try:
                process.kill()
            except OSError:
                pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    job_lock = threading.Lock()

    def interrupt_group():
        nonlocal job_handle
        try:
            if os.name != "nt":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                with job_lock:
                    owned_job = job_handle
                    job_handle = None
                if owned_job:
                    _close_windows_job(owned_job)
                else:
                    process.kill()
        except ProcessLookupError:
            pass
        except OSError:
            try:
                process.kill()
            except OSError:
                pass

    with ACTIVE_SUITE_LOCK:
        ACTIVE_SUITE_INTERRUPTERS.add(interrupt_group)

    relay_thread = _relay_suite_output(process)

    def finish(returncode, error):
        relay_thread.join()
        return _with_structured_outcomes((returncode, error, [], []), environment)

    try:
        if os.name == "nt":
            _resume_windows_process(process)
        if os.name != "nt" and _supports_waitid_nowait():
            deadline = time.monotonic() + timeout
            while True:
                # Keep the leader as a zombie until its process group is
                # cleaned, preventing its PID/PGID from being reused first.
                exited = os.waitid(os.P_PID, process.pid,
                                   os.WEXITED | os.WNOHANG | os.WNOWAIT)
                if exited and exited.si_pid:
                    terminate_group()
                    return finish(process.returncode, None)
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    terminate_group()
                    return finish(None, f"timed out after {timeout:g}s")
                time.sleep(min(.01, remaining))
        try:
            returncode = process.wait(timeout=timeout)
            terminate_group()
            return finish(returncode, None)
        except subprocess.TimeoutExpired:
            terminate_group()
            return finish(None, f"timed out after {timeout:g}s")
    except BaseException:
        terminate_group()
        relay_thread.join()
        raise
    finally:
        with ACTIVE_SUITE_LOCK:
            ACTIVE_SUITE_INTERRUPTERS.discard(interrupt_group)
        if job_handle:
            _close_windows_job(job_handle)


def _relay_suite_output(process):
    """Relay human-readable suite output; machine outcomes use a separate file."""
    def relay():
        output = getattr(process, "stdout", None)
        if output is None:
            return
        try:
            for line in iter(output.readline, b""):
                try:
                    stream = sys.stdout
                    if hasattr(stream, "buffer"):
                        stream.buffer.write(line)
                        stream.buffer.flush()
                    else:
                        stream.write(line.decode(errors="replace"))
                        stream.flush()
                except (BrokenPipeError, OSError):
                    pass
        finally:
            output.close()

    thread = threading.Thread(target=relay, name="server-suite-output", daemon=True)
    thread.start()
    return thread


def _parse_test_outcomes(records):
    """Parse structured unittest records; display output is deliberately ignored."""
    outcomes = {"expected": [], "unexpected": []}
    counts = {name: 0 for name in ("passed", "failed", "error", "skipped",
                                   "expected_failure", "unexpected_success")}
    for line_number, line in enumerate(records.splitlines(), start=1):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise ValueError(f"invalid unittest outcome JSON on line {line_number}: {error}") from error
        if not isinstance(record, dict) or not isinstance(record.get("id"), str):
            raise ValueError(f"invalid unittest outcome record on line {line_number}")
        outcome = record.get("outcome")
        if outcome not in {"passed", "failed", "error", "skipped",
                           "expected_failure", "unexpected_success"}:
            raise ValueError(f"invalid unittest outcome on line {line_number}: {outcome!r}")
        counts[outcome] += 1
        identity = ".".join(record["id"].split(".")[-2:])
        if outcome == "expected_failure":
            outcomes["expected"].append(identity)
        elif outcome == "unexpected_success":
            outcomes["unexpected"].append(identity)
    return outcomes["expected"], outcomes["unexpected"], counts


def _with_structured_outcomes(result, environment):
    returncode, error, _expected, _unexpected = result
    outcome_file = environment.get("CODEX_SERVER_TEST_RESULT_FILE")
    if not outcome_file:
        return result
    try:
        records = Path(outcome_file).read_text(encoding="utf-8")
        expected, unexpected, counts = _parse_test_outcomes(records)
    except (OSError, ValueError) as outcome_error:
        if error is None:
            error = f"could not read structured unittest outcomes: {outcome_error}"
        return returncode, error, [], []
    with SUITE_OUTCOME_LOCK:
        for name, count in counts.items():
            TEST_OUTCOME_COUNTS[name] = TEST_OUTCOME_COUNTS.get(name, 0) + count
    return returncode, error, expected, unexpected


def _pinned_expected_failure_ids():
    return {line.strip() for line in EXPECTED_FAILURES_PATH.read_text(encoding="utf-8").splitlines()
            if line.strip() and not line.lstrip().startswith("#")}


def _supports_waitid_nowait():
    return hasattr(os, "waitid") and hasattr(os, "WNOWAIT")


def _run_process_with_group_supervisor(command, cwd, timeout, environment):
    """Fallback that keeps a group anchor alive until its children are killed."""
    supervisor = (
        "import os, signal, subprocess, sys\n"
        "status_fd = int(sys.argv[1])\n"
        "os.environ.pop('CODEX_SERVER_GROUP_KILL_HOOK', None)\n"
        "try:\n"
        "    result = subprocess.call(sys.argv[2:])\n"
        "except BaseException:\n"
        "    result = 127\n"
        "with os.fdopen(status_fd, 'w') as status:\n"
        "    status.write(str(result))\n"
        "os.killpg(os.getpgrp(), signal.SIGKILL)\n"
    )
    read_fd, write_fd = os.pipe()
    process = None
    try:
        process = subprocess.Popen(
            [sys.executable, "-c", supervisor, str(write_fd), *command],
            cwd=cwd, env=environment, start_new_session=True, pass_fds=(write_fd,),
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    except BaseException:
        os.close(read_fd)
        raise
    finally:
        os.close(write_fd)
    relay_thread = _relay_suite_output(process)

    def finish(returncode, error):
        relay_thread.join()
        return _with_structured_outcomes((returncode, error, [], []), environment)

    def terminate_group():
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    try:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                terminate_group()
                return finish(None, f"timed out after {timeout:g}s")
            try:
                ready, _, _ = select.select([read_fd], [], [], min(remaining, 1.0))
            except InterruptedError:
                continue
            if ready:
                break
        payload = os.read(read_fd, 64)
        if not payload:
            terminate_group()
            return finish(None, "suite process-group supervisor exited without status")
        returncode = int(payload.decode("ascii"))
        process.wait(timeout=5)
        return finish(returncode, None)
    except BaseException:
        terminate_group()
        relay_thread.join()
        raise
    finally:
        os.close(read_fd)


def _windows_kernel32():
    return ctypes.WinDLL("kernel32", use_last_error=True)


def _assign_windows_job(process):
    """Assign a suspended suite process to a kill-on-close Windows job."""
    from ctypes import wintypes

    class BasicLimitInformation(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_longlong),
            ("PerJobUserTimeLimit", ctypes.c_longlong),
            ("LimitFlags", wintypes.DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", wintypes.DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", wintypes.DWORD),
            ("SchedulingClass", wintypes.DWORD),
        ]

    class IoCounters(ctypes.Structure):
        _fields_ = [(name, ctypes.c_ulonglong) for name in (
            "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
            "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]

    class ExtendedLimitInformation(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", BasicLimitInformation),
            ("IoInfo", IoCounters),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]

    kernel32 = _windows_kernel32()
    kernel32.CreateJobObjectW.argtypes = [wintypes.LPVOID, wintypes.LPCWSTR]
    kernel32.CreateJobObjectW.restype = wintypes.HANDLE
    kernel32.SetInformationJobObject.argtypes = [
        wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD]
    kernel32.SetInformationJobObject.restype = wintypes.BOOL
    kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel32.AssignProcessToJobObject.restype = wintypes.BOOL

    job = None
    try:
        job = kernel32.CreateJobObjectW(None, None)
        if not job:
            raise ctypes.WinError(ctypes.get_last_error())
        limits = ExtendedLimitInformation()
        limits.BasicLimitInformation.LimitFlags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not kernel32.SetInformationJobObject(
                job, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            raise ctypes.WinError(ctypes.get_last_error())
        if not kernel32.AssignProcessToJobObject(job, wintypes.HANDLE(process._handle)):
            raise ctypes.WinError(ctypes.get_last_error())
        return job
    except BaseException:
        # The process was created suspended. Kill it even if job creation or
        # assignment failed, so no orphaned suspended process remains.
        if job:
            try:
                _close_windows_job(job)
            except OSError:
                pass
        try:
            process.kill()
        except OSError:
            pass
        process.wait()
        raise


def _resume_windows_process(process):
    ntdll = ctypes.WinDLL("ntdll")
    ntdll.NtResumeProcess.argtypes = [ctypes.c_void_p]
    ntdll.NtResumeProcess.restype = ctypes.c_long
    status = ntdll.NtResumeProcess(ctypes.c_void_p(process._handle))
    if status != 0:
        process.kill()
        process.wait()
        raise OSError(f"NtResumeProcess failed with NTSTATUS {status:#x}")


def _close_windows_job(job):
    if job:
        from ctypes import wintypes

        kernel32 = _windows_kernel32()
        kernel32.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
        kernel32.TerminateJobObject.restype = wintypes.BOOL
        kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel32.CloseHandle.restype = wintypes.BOOL
        handle = wintypes.HANDLE(job)
        kernel32.TerminateJobObject(handle, 1)
        closed = kernel32.CloseHandle(handle)
        close_error = None if closed else ctypes.WinError(ctypes.get_last_error())
        if close_error:
            raise close_error
        # Closing a kill-on-close job is itself the required cleanup. A failed
        # explicit TerminateJobObject call does not invalidate that guarantee.


def _suite_command(relative, root):
    if relative.endswith(".mjs"):
        return ["node", str(root / relative)]
    entry = root / f"{TESTS_REL}/server/suite_entry.py"
    if relative.startswith(f"{SOURCE_REL}/studio_api/") and relative.endswith(".py"):
        module_name = relative[len(f"{SOURCE_REL}/"):-3].replace("/", ".")
        return [sys.executable, "-B", str(entry), "module", module_name, "-v"]
    path = root / relative
    if is_unittest_suite(path):
        return [sys.executable, "-B", str(entry), "path", str(path), "-v"]
    command = [sys.executable, "-B", str(path)]
    return command


def _suite_test_ids(relative, root, temporary, deadline):
    """Return unittest IDs for an isolated suite, or None when discovery fails."""
    path = root / relative
    if not relative.endswith(".py") or not is_unittest_suite(path):
        return None
    environment = _suite_environment(root, temporary, relative=relative)
    if relative.startswith(f"{SOURCE_REL}/studio_api/"):
        kind = "module"
        target = relative[len(f"{SOURCE_REL}/"):-3].replace("/", ".")
    else:
        kind, target = "path", str(path)
    command = [sys.executable, "-B", str(root / f"{TESTS_REL}/server/suite_entry.py"), "list", kind, target]
    try:
        result = subprocess.run(command, cwd=root, env=environment, capture_output=True,
                                text=True, timeout=deadline, check=False)
        ids = json.loads(next(line[9:] for line in result.stdout.splitlines()
                              if line.startswith("TEST_IDS:")))
    except (OSError, subprocess.SubprocessError, StopIteration, json.JSONDecodeError):
        return None
    return ids if result.returncode == 0 and ids and len(ids) == len(set(ids)) else None


def _slice_test_ids(test_ids, count):
    """Split IDs round-robin so every slice gets a similar number of cases."""
    return [test_ids[index::count] for index in range(count) if test_ids[index::count]]


def _focused_suite_slice_count(relative, test_count, workers):
    """Bound focused test slices by the selected suite and focused worker plan."""
    if (relative in NEVER_SLICE_SUITES or is_http_boundary_suite(relative)
            or test_count < FOCUSED_MIN_TESTS_FOR_SLICING or workers <= 1):
        return 1
    return min(FOCUSED_SUITE_TEST_SLICES, workers, test_count)


def is_http_boundary_suite(relative):
    """Keep backend-process contracts together without a maintained suite list."""
    name = Path(relative).name
    return name == "http-boundary-contract.py" or name.endswith("-http-contract.py")


def _suite_environment(root, temp_root, audit_home=False, relative=None):
    environment = os.environ.copy()
    isolated_home = temp_root / "home"
    isolated_home.mkdir(parents=True, exist_ok=True)
    environment["HOME"] = str(isolated_home)
    environment["USERPROFILE"] = str(isolated_home)
    environment["TMPDIR"] = str(temp_root)
    environment["XDG_CACHE_HOME"] = str(temp_root / "cache")
    environment["XDG_STATE_HOME"] = str(temp_root / "state")
    environment["XDG_DATA_HOME"] = str(temp_root / "data")
    environment["XDG_CONFIG_HOME"] = str(temp_root / "config")
    environment["CODEX_HOME"] = str(temp_root / "codex-home")
    environment["CODEX_AGENTS_PYTHON"] = sys.executable
    # Prevent host Git settings and background index maintenance from changing
    # fixture repositories while another suite copies or fingerprints them.
    isolated_git_config = isolated_home / ".gitconfig"
    isolated_git_config.touch(exist_ok=True)
    environment["GIT_CONFIG_NOSYSTEM"] = "1"
    environment["GIT_CONFIG_GLOBAL"] = str(isolated_git_config)
    git_overrides = {
        "gc.auto": "0",
        "maintenance.auto": "false",
        "core.fsmonitor": "false",
        "core.untrackedCache": "false",
    }
    environment["GIT_CONFIG_COUNT"] = str(len(git_overrides))
    for index, (key, value) in enumerate(git_overrides.items()):
        environment[f"GIT_CONFIG_KEY_{index}"] = key
        environment[f"GIT_CONFIG_VALUE_{index}"] = value
    # Ordinary suites cannot discover inherited provider programs or selectors.
    # Pin the crypto helper before dropping the caller's tool-cache PATH.
    node = environment.get("CODEX_NODE") or shutil.which("node")
    path_entries = [str(Path(sys.executable).parent), os.defpath]
    if node:
        environment["CODEX_NODE"] = node
        # JS suites and bridge probes launch "node" by name. Expose only that
        # executable, never its caller-owned directory (which may hold providers).
        tools = temp_root / "tools"
        tools.mkdir()
        (tools / "node").symlink_to(Path(node).absolute())
        path_entries.insert(0, str(tools))
    environment["PATH"] = os.pathsep.join(path_entries)
    for key in tuple(environment):
        if key in {"CODEX_BIN", "CODEX_AGENTS_NATIVE_RUNTIME_PIN", "CLAUDE_BIN",
                   "STUDIO_CLAUDE_BIN", "STUDIO_NODE_BIN", "OPENAI_API_KEY",
                   "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"}:
            environment.pop(key)
    if relative in CODEX_BIN_SUITES and RESOLVED_CODEX_BIN:
        environment["CODEX_BIN"] = RESOLVED_CODEX_BIN
    environment["CLAUDE_CONFIG_DIR"] = str(temp_root / "claude-home")
    workspace_store = temp_root / "studio-test-workspaces-store"
    workspace_store.mkdir()
    # test_isolation.py accepts this path only when it is an existing child of
    # the suite's TMPDIR, which keeps every worker's image store independent.
    environment["CODEX_AGENTS_TEST_WORKSPACE_STORE"] = str(workspace_store)
    environment["CODEX_WORKSPACE_STORE"] = str(workspace_store)
    source_path = str(root / SOURCE_REL)
    tests_path = str(root / TESTS_REL)
    python_paths = [source_path, tests_path]
    if audit_home:
        audit_path = str(Path(root) / TESTS_REL / "server" / "audit-home")
        python_paths.insert(0, audit_path)
        environment["CODEX_SERVER_TEST_AUDIT_HOME"] = "1"
        environment["CODEX_SERVER_TEST_REAL_HOME"] = str(Path.home())
        environment["CODEX_SERVER_TEST_AUDIT_LOG"] = str(
            _runner_artifact_root(temp_root) / "audit-home.log")
        if relative in CODEX_BIN_SUITES and RESOLVED_CODEX_BIN:
            environment["CODEX_SERVER_TEST_AUDIT_ALLOW_EXECUTABLE"] = RESOLVED_CODEX_BIN
    inherited_pythonpath = environment.get("PYTHONPATH", "")
    if inherited_pythonpath:
        python_paths.append(inherited_pythonpath)
    environment["PYTHONPATH"] = os.pathsep.join(python_paths)
    return environment


def _runner_artifact_root(temp_root):
    """Keep runner-owned files beside fixture workspaces, not inside them."""
    artifact_root = Path(temp_root) / "_runner"
    artifact_root.mkdir(exist_ok=True)
    return artifact_root


def _cleanup_suite_scratch(scratch):
    """Remove one suite scratch folder; describe a failure instead of raising it."""
    try:
        scratch.cleanup()
    except OSError as error:
        return f"scratch cleanup failed: {error}; {_scratch_holder_report(scratch.name)}"
    return None


def _scratch_holder_report(path):
    """Name live processes whose working directory or open files are inside path."""
    target = os.path.realpath(path)
    holders = {}
    proc = Path("/proc")
    if proc.is_dir():
        for entry in proc.iterdir():
            if not entry.name.isdigit():
                continue
            try:
                links = [entry / "cwd", *(entry / "fd").iterdir()]
            except OSError:
                continue
            for link in links:
                try:
                    resolved = os.readlink(link)
                except OSError:
                    continue
                if resolved == target or resolved.startswith(target + os.sep):
                    try:
                        holders[int(entry.name)] = (entry / "comm").read_text().strip()
                    except OSError:
                        holders[int(entry.name)] = "?"
                    break
    else:
        lsof = shutil.which("lsof", path="/usr/sbin:/usr/bin:/sbin:/bin")
        if not lsof:
            return "could not list processes using it: no /proc and no lsof"
        try:
            listing = subprocess.run([lsof, "-n", "-P", "-F", "pc", "+D", target],
                                     capture_output=True, text=True, timeout=30, check=False)
        except (OSError, subprocess.SubprocessError) as error:
            return f"could not list processes using it: {error}"
        process_id = None
        for line in listing.stdout.splitlines():
            if line.startswith("p"):
                process_id = int(line[1:])
            elif line.startswith("c") and process_id is not None:
                holders[process_id] = line[1:]
    if not holders:
        return "no live process has its working directory or an open file there"
    return "processes still using it: " + ", ".join(
        f"{process_id} ({name})" for process_id, name in sorted(holders.items()))


def _unix_socket_path_error(temp_root):
    """Return an early, actionable error if nested fixture sockets cannot fit."""
    if os.name == "nt":  # pragma: no cover - AF_UNIX path limits differ on Windows
        return None
    # Cover the deepest socket paths used by shutdown, isolation and default
    # supervisor fixtures, including their nested TemporaryDirectory names.
    prospective = (
        temp_root / "studio-shutdown-00000000" / "state" / "canvas.sock",
        temp_root / "studio-decoy-supervisor-00000000" / "supervisor.sock",
        temp_root / "state" / "codex-agents" / "supervisor.sock",
    )
    longest = max(prospective, key=lambda path: len(os.fsencode(path)))
    path_bytes = len(os.fsencode(longest))
    if path_bytes > MAX_UNIX_SOCKET_PATH_BYTES:
        return (f"suite scratch path leaves no room for AF_UNIX sockets: {longest} "
                f"({path_bytes} bytes; limit {MAX_UNIX_SOCKET_PATH_BYTES}); "
                "choose a shorter CODEX_SERVER_TEST_TMP_ROOT")
    return None


def _tmpfs_mounts():
    """Yield writable tmpfs mount points available to this process."""
    if not sys.platform.startswith("linux"):
        return
    try:
        lines = Path("/proc/self/mountinfo").read_text(encoding="ascii").splitlines()
    except OSError:
        return
    mounts = set()
    for line in lines:
        fields = line.split(" - ", 1)
        if len(fields) != 2:
            continue
        before, after = fields
        details = after.split()
        options = before.split()
        if len(details) < 3 or len(options) < 6 or details[0] != "tmpfs":
            continue
        if "ro" in options[5].split(",") or "ro" in details[2].split(","):
            continue
        mount_text = options[4]
        mount = Path(mount_text.replace("\\040", " ").replace("\\011", "\t")
                      .replace("\\134", "\\").replace("\\012", "\n"))
        if len(os.fsencode(mount)) <= MAX_SHORT_TMP_ROOT_BYTES:
            mounts.add(mount)
    yield from sorted(mounts, key=lambda path: (len(os.fsencode(path)), str(path)))


def _probe_scratch_capacity(directory, size_bytes):
    """Verify usable quota by writing, syncing, and deleting a temporary file."""
    probe = directory / ".scratch-capacity-probe"
    descriptor = None
    try:
        descriptor = os.open(probe, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        chunk = b"\0" * min(SCRATCH_PROBE_CHUNK_BYTES, size_bytes)
        remaining = size_bytes
        while remaining:
            block = chunk[:min(len(chunk), remaining)]
            written = os.write(descriptor, block)
            if written <= 0:
                return False
            remaining -= written
        os.fsync(descriptor)
        return True
    except OSError:
        return False
    finally:
        if descriptor is not None:
            os.close(descriptor)
        probe.unlink(missing_ok=True)


def _is_memory_backed_path(path):
    resolved = Path(path).resolve()
    return any(resolved == mount or mount in resolved.parents for mount in _tmpfs_mounts())


def _process_start_time(process_id):
    try:
        stat = Path(f"/proc/{int(process_id)}/stat").read_text(encoding="ascii")
        fields = stat[stat.rfind(")") + 2:].split()
        return fields[19]
    except (OSError, ValueError, IndexError):
        return None


def _cleanup_stale_scratch_roots(mounts=None):
    """Remove roots only when their owner-held kernel lock is available."""
    import fcntl

    for mount in _tmpfs_mounts() if mounts is None else mounts:
        try:
            maintenance_lock = (mount / SCRATCH_MAINTENANCE_LOCK_FILE).open("a+b")
        except OSError:
            continue
        try:
            fcntl.flock(maintenance_lock.fileno(), fcntl.LOCK_EX)
            for directory in mount.glob("csst-*"):
                if not directory.is_dir():
                    continue
                try:
                    lock_file = (directory / SCRATCH_LOCK_FILE).open("r+b")
                except FileNotFoundError:
                    try:
                        age = time.time() - directory.stat().st_mtime
                    except OSError:
                        continue
                    if age > LEGACY_SCRATCH_MAX_AGE_SECONDS:
                        shutil.rmtree(directory, ignore_errors=True)
                    continue
                except OSError:
                    continue
                try:
                    try:
                        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        continue
                    except OSError:
                        continue
                    shutil.rmtree(directory, ignore_errors=True)
                finally:
                    lock_file.close()
        except OSError:
            continue
        finally:
            maintenance_lock.close()


def _remove_owned_scratch_root(directory):
    """Remove this process's scratch root before releasing its ownership lock."""
    lock_file = OWNED_SCRATCH_LOCKS.pop(Path(directory), None)
    shutil.rmtree(directory, ignore_errors=True)
    if lock_file is not None:
        lock_file.close()


@contextmanager
def _runner_live_lock():
    """Hold one per-user lock for the whole run; the kernel releases it on exit."""
    RUNNER_LOCK_PATH.parent.mkdir(parents=True, exist_ok=True)
    with RUNNER_LOCK_PATH.open("a+b") as lock_file:
        import fcntl
        try:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("waiting for another test run to finish", flush=True)
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        yield


def _focused_slot_count(profile=None):
    """Bound focused-suite peak RSS to the memory already reserved by full runs."""
    profile = _load_profile() if profile is None else profile
    baseline_peak = int(_read_profile_file(BASELINE_PATH).get("maxSuiteRssBytes", 0) or 0)
    measured_peak = max(int(profile.get("maxSuiteRssBytes", 0) or 0), baseline_peak)
    if measured_peak <= 0:
        return 0
    available_memory = available_memory_bytes()
    memory_budget = min(MEMORY_RESERVE_BYTES,
                        available_memory // 2,
                        max(0, available_memory - MEMORY_RESERVE_BYTES))
    memory_slots = memory_budget // measured_peak
    cpu_slots = min(FOCUSED_SLOT_CAP, max(1, math.ceil(available_cpu_count() / 4)))
    return min(cpu_slots, memory_slots)


@contextmanager
def _focused_run_slot(profile=None, slots=None):
    """Acquire a per-user slot without waiting behind the whole-run lock."""
    slots = _focused_slot_count(profile) if slots is None else slots
    if slots < 1:
        raise RuntimeError("focused run has no memory-safe admission slot")
    RUNNER_LOCK_PATH.parent.mkdir(parents=True, exist_ok=True)
    import fcntl
    waiting = False
    while True:
        for index in range(slots):
            lock_file = (RUNNER_LOCK_PATH.parent / f"runner-focused-{index}.lock").open("a+b")
            try:
                fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                lock_file.close()
                continue
            try:
                yield
            finally:
                lock_file.close()
            return
        if not waiting:
            print("waiting for a focused-run admission slot", flush=True)
            waiting = True
        time.sleep(0.05)



def _memory_scratch_root(workers, *, simultaneous_runs=1):
    """Use tmpfs only with measured headroom for workers across active runs."""
    import fcntl

    mounts = list(_tmpfs_mounts())
    _cleanup_stale_scratch_roots(mounts)
    candidates = []
    for mount in mounts:
        owned = None
        try:
            required_bytes = workers * simultaneous_runs * SCRATCH_BYTES_PER_WORKER
            probe_bytes = simultaneous_runs * SCRATCH_PROBE_MIN_BYTES
            if shutil.disk_usage(mount).free < required_bytes:
                continue
            maintenance_lock = (mount / SCRATCH_MAINTENANCE_LOCK_FILE).open("a+b")
            try:
                fcntl.flock(maintenance_lock.fileno(), fcntl.LOCK_EX)
                owned = Path(tempfile.mkdtemp(prefix="csst-", dir=mount))
                lock_file = (owned / SCRATCH_LOCK_FILE).open("a+b")
                try:
                    fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                except OSError:
                    lock_file.close()
                    raise
                OWNED_SCRATCH_LOCKS[owned] = lock_file
                (owned / SCRATCH_OWNER_FILE).write_text(json.dumps({
                    "pid": os.getpid(), "startTime": _process_start_time(os.getpid())}), encoding="ascii")
            finally:
                maintenance_lock.close()
            if len(os.fsencode(owned)) > MAX_SHORT_TMP_ROOT_BYTES:
                _remove_owned_scratch_root(owned)
                continue
            if not _probe_scratch_capacity(owned, probe_bytes):
                _remove_owned_scratch_root(owned)
                continue
            candidates.append((shutil.disk_usage(mount).free, owned))
        except OSError:
            if owned is not None:
                _remove_owned_scratch_root(owned)
    if not candidates:
        return None
    candidates.sort(key=lambda candidate: candidate[0], reverse=True)
    selected = candidates[0][1]
    for _free, unused in candidates[1:]:
        _remove_owned_scratch_root(unused)
    return selected


def available_cpu_count():
    """Return CPUs available to this process, including affinity/cgroup limits."""
    count = None
    process_count = getattr(os, "process_cpu_count", None)
    if process_count is not None:
        count = process_count()
    get_affinity = getattr(os, "sched_getaffinity", None)
    if not count and get_affinity is not None:
        try:
            count = len(get_affinity(0))
        except OSError:
            pass
    count = count or os.cpu_count() or 1
    for directory in _cgroup_directories("cpu"):
        try:
            unified = directory / "cpu.max"
            if unified.exists():
                quota_text, period_text = unified.read_text(encoding="ascii").split()
                quota = None if quota_text == "max" else int(quota_text)
                period = int(period_text)
            else:
                quota = int((directory / "cpu.cfs_quota_us").read_text(encoding="ascii").strip())
                period = int((directory / "cpu.cfs_period_us").read_text(encoding="ascii").strip())
            if quota is not None and quota > 0 and period > 0:
                count = min(count, max(1, quota // period))
        except (OSError, ValueError):
            continue
    return count


def _cgroup_directories(controller):
    """Yield this process's cgroup and parents for v1 or v2 controller mounts."""
    candidates = []
    try:
        memberships = Path("/proc/self/cgroup").read_text(encoding="ascii").splitlines()
    except OSError:
        memberships = []
    for membership in memberships:
        try:
            hierarchy, controllers, relative = membership.split(":", 2)
        except ValueError:
            continue
        if hierarchy == "0" and not controllers:
            mounts = [Path("/sys/fs/cgroup")]
        elif controller in controllers.split(","):
            mounts = [Path("/sys/fs/cgroup") / controller]
            if controller == "cpu":
                mounts.append(Path("/sys/fs/cgroup/cpu,cpuacct"))
        else:
            continue
        for mount in mounts:
            current = mount / relative.lstrip("/")
            while current == mount or mount in current.parents:
                candidates.append(current)
                if current == mount:
                    break
                current = current.parent
    if not candidates:
        candidates.append(Path("/sys/fs/cgroup"))
    return tuple(dict.fromkeys(candidates))


def _vm_stat_available_bytes():
    """Free, inactive and speculative pages from macOS vm_stat, or None.

    macOS has no SC_AVPHYS_PAGES; inactive pages are reclaimable without swap.
    """
    try:
        output = subprocess.run(["/usr/bin/vm_stat"], capture_output=True, text=True,
                                timeout=5, check=True).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    page_size = re.search(r"page size of (\d+) bytes", output)
    pages = dict(re.findall(r"^Pages (free|inactive|speculative):\s+(\d+)\.$", output, re.MULTILINE))
    if page_size is None or len(pages) != 3:
        return None
    return int(page_size.group(1)) * sum(int(count) for count in pages.values())


def available_memory_bytes():
    """Return available memory, bounded by this process's cgroup when present."""
    available = None
    try:
        page_size = os.sysconf("SC_PAGE_SIZE")
        available_pages = os.sysconf("SC_AVPHYS_PAGES")
        available = int(page_size * available_pages)
    except (OSError, ValueError, AttributeError):
        pass
    if available is None and sys.platform == "darwin":
        available = _vm_stat_available_bytes()
    try:
        for line in Path("/proc/meminfo").read_text(encoding="ascii").splitlines():
            if line.startswith("MemAvailable:"):
                available = int(line.split()[1]) * 1024
                break
    except (OSError, ValueError, IndexError):
        pass
    for directory in _cgroup_directories("memory"):
        try:
            unified_limit = directory / "memory.max"
            if unified_limit.exists():
                limit_text = unified_limit.read_text(encoding="ascii").strip()
                current = int((directory / "memory.current").read_text(encoding="ascii").strip())
            else:
                limit_text = (directory / "memory.limit_in_bytes").read_text(encoding="ascii").strip()
                current = int((directory / "memory.usage_in_bytes").read_text(encoding="ascii").strip())
            limit = int(limit_text) if limit_text != "max" else None
            if limit is not None and limit > 0:
                cgroup_available = max(0, limit - current)
                available = cgroup_available if available is None else min(available, cgroup_available)
        except (OSError, ValueError):
            continue
    return max(0, available or 0)


def _instantaneous_runnable_process_count():
    """Read the kernel's current runnable-task count when available."""
    try:
        for line in Path("/proc/stat").read_text(encoding="ascii").splitlines():
            if line.startswith("procs_running "):
                return max(0, int(line.split()[1]))
    except (OSError, ValueError, IndexError):
        pass
    return 0


def _load_average_other_count():
    """Without /proc/stat (macOS), use the one-minute load average less this planner."""
    try:
        load = os.getloadavg()[0]
    except (OSError, AttributeError):
        return 0
    return max(0, math.ceil(load) - 1)


def sample_runnable_other_process_count(window_seconds=5.0, interval_seconds=0.5,
                                       *, sample=None, clock=None, sleep=None):
    """Estimate competing runnable work from the sample-window median."""
    if not Path("/proc/stat").is_file():
        return _load_average_other_count()
    sample = _instantaneous_runnable_process_count if sample is None else sample
    clock = time.monotonic if clock is None else clock
    sleep = time.sleep if sleep is None else sleep
    samples = []
    started = clock()
    deadline = started + max(0.0, window_seconds)
    while True:
        value = sample()
        samples.append(value)
        remaining = deadline - clock()
        if remaining <= 0:
            break
        sleep(min(interval_seconds, remaining))
    observed = statistics.median(samples) if samples else 0
    # Each sample includes this main thread, which is runnable while planning.
    return max(0, math.ceil(observed) - 1)


def _read_profile_file(path):
    try:
        saved = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return saved if isinstance(saved, dict) else {}


@contextmanager
def _profile_file_lock():
    """Serialize profile read/merge/write across independent runner processes."""
    PROFILE_PATH.parent.mkdir(parents=True, exist_ok=True)
    lock_path = PROFILE_PATH.with_suffix(".lock")
    with lock_path.open("a+b") as lock_file:
        import fcntl
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_UN)


def _load_profile():
    profile = {}
    for path in (BASELINE_PATH, PROFILE_PATH):
        saved = _read_profile_file(path)
        if not saved:
            continue
        costs = dict(profile.get("suiteSeconds", {}))
        costs.update(saved.get("suiteSeconds", {}))
        profile.update(saved)
        profile["suiteSeconds"] = costs
    return profile


def _save_profile(profile):
    PROFILE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with _profile_file_lock():
        merged = _read_profile_file(PROFILE_PATH)
        suites = dict(merged.get("suiteSeconds", {}))
        suites.update(profile.get("suiteSeconds", {}))
        merged.update(profile)
        merged["suiteSeconds"] = suites
        descriptor, temporary_name = tempfile.mkstemp(
            prefix=f".{PROFILE_PATH.name}.", suffix=".tmp", dir=PROFILE_PATH.parent,
        )
        temporary = Path(temporary_name)
        try:
            with os.fdopen(descriptor, "w", encoding="utf-8") as output:
                json.dump(merged, output, sort_keys=True)
                output.flush()
            os.replace(temporary, PROFILE_PATH)
        finally:
            temporary.unlink(missing_ok=True)


def _measured_child_peak_rss_bytes():
    if resource is None:
        return 0
    peak = int(resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss)
    if sys.platform == "darwin":
        return peak
    return peak * 1024


def worker_plan(entries, profile=None, override=None, sample_seconds=5.0):
    profile = _load_profile() if profile is None else profile
    cpu_limit = available_cpu_count()
    if not entries:
        memory = available_memory_bytes()
        return {
            "workers": 0,
            "limitingBound": "suites",
            "availableCpus": cpu_limit,
            "cpuFloor": max(1, math.ceil(cpu_limit / 4)),
            "unclampedCpuCount": cpu_limit,
            "cpuLimit": cpu_limit,
            "cpuWorkerSlots": 0,
            "otherRunnableProcesses": 0,
            "availableMemoryBytes": memory,
            "memoryBudgetBytes": min(memory // 2, max(0, memory - MEMORY_RESERVE_BYTES)),
            "memoryReserveBytes": MEMORY_RESERVE_BYTES,
            "measuredPeakSuiteRssBytes": int(profile.get("maxSuiteRssBytes", 0) or 0),
            "memoryWorkerSlots": 0,
            "estimatedSuiteSeconds": 0,
            "runnableSuites": 0,
            "codexExecutable": RESOLVED_CODEX_BIN,
        }
    other_runnable = (0 if override is not None else
                      sample_runnable_other_process_count(window_seconds=sample_seconds))
    cpu_floor = max(1, math.ceil(cpu_limit / 4))
    cpu_unclamped = max(1, cpu_limit - other_runnable)
    cpus = max(cpu_floor, cpu_unclamped)
    cpu_worker_slots = min(cpus, len(entries))
    cpu_bound = "cpu-floor" if cpu_unclamped < cpu_floor else "cpu-median"
    memory = available_memory_bytes()
    peak_rss = int(profile.get("maxSuiteRssBytes", 0) or 0)
    memory_budget = min(memory // 2, max(0, memory - MEMORY_RESERVE_BYTES))
    # MemAvailable already reflects pages currently occupied by tmpfs/shmem.
    memory_per_worker = peak_rss
    memory_slots = max(1, memory_budget // memory_per_worker) if memory_per_worker else cpus
    costs = profile.get("suiteSeconds", {})
    total = sum(float(costs.get(path, 0) or 0) for path, _kind in entries)
    if total <= 0:
        total = float(profile.get("observedWallSeconds", 0) or 0)
    suite_count = len(entries)
    if override is not None:
        selected = max(1, min(override, memory_slots, suite_count))
        limiting_bound = ("memory" if memory_slots < override and memory_slots <= suite_count
                          else "suites" if suite_count < override and suite_count < memory_slots
                          else "override")
    else:
        selected = max(1, min(cpus, memory_slots, suite_count))
        limiting_bound = ("memory" if memory_slots <= cpus and memory_slots <= suite_count
                          else "suites" if suite_count <= cpus and suite_count < memory_slots
                          else cpu_bound)
    return {
        "workers": selected,
        "limitingBound": limiting_bound,
        "availableCpus": cpus,
        "cpuFloor": cpu_floor,
        "unclampedCpuCount": cpu_unclamped,
        "cpuLimit": cpu_limit,
        "cpuWorkerSlots": cpu_worker_slots,
        "otherRunnableProcesses": other_runnable,
        "availableMemoryBytes": memory,
        "memoryBudgetBytes": memory_budget,
        "memoryReserveBytes": MEMORY_RESERVE_BYTES,
        "measuredPeakSuiteRssBytes": peak_rss,
        "measuredWorkerMemoryBytes": memory_per_worker,
        "memoryWorkerSlots": memory_slots,
        "estimatedSuiteSeconds": total,
        "runnableSuites": len(entries),
        "codexExecutable": RESOLVED_CODEX_BIN,
    }


def run_suites(entries, opted_in, timeout, expensive_timeout, root=ROOT,
               execute=run_process, workers=None, load_sample_seconds=5.0,
               audit_home=False, shard_index=None, shard_count=None):
    runnable, _skipped = _runnable_entries(entries, opted_in)
    runnable = _apply_shard(runnable, shard_index, shard_count)
    focus_profile = _load_profile() if len(runnable) == FOCUSED_SUITE_LIMIT else None
    focused_slots = _focused_slot_count(focus_profile) if focus_profile is not None else 0
    focused = focused_slots > 0
    # A focused unittest can consume its bounded slots as test slices; admit
    # only one such group at a time. Plain focused scripts retain slot sharing.
    focus_capacity = (1 if runnable and is_unittest_suite(root / runnable[0][0])
                      else focused_slots)
    lock = _focused_run_slot(focus_profile, focus_capacity) if focused else _runner_live_lock()
    with lock:
        return _run_suites_locked(entries, opted_in, timeout, expensive_timeout,
                                  root, execute, workers, load_sample_seconds, audit_home,
                                  focused=focused, focused_slots=focused_slots,
                                  shard_index=shard_index, shard_count=shard_count)


def _runnable_entries(entries, opted_in):
    runnable = []
    skipped = []
    for path, kind in entries:
        if path == TERMINALS_SUITE and not RESOLVED_CODEX_BIN:
            skipped.append((path, "environment"))
        elif kind in {"safe", "component"} or kind in opted_in:
            runnable.append((path, kind))
        else:
            skipped.append((path, kind))
    return runnable, skipped


def shard_entries(entries, shard_index, shard_count):
    """Assign sorted runnable suite paths to stable, non-overlapping shards."""
    if shard_count < 1:
        raise ValueError("shard count must be at least one")
    if not 0 <= shard_index < shard_count:
        raise ValueError("shard index must be between zero and shard count minus one")
    ordered = sorted(entries, key=lambda entry: entry[0])
    selected = [entry for index, entry in enumerate(ordered) if index % shard_count == shard_index]
    if not selected:
        raise ValueError(f"server shard {shard_index + 1}/{shard_count} selected no suites")
    return selected


def _apply_shard(runnable, shard_index, shard_count):
    if shard_index is None and shard_count is None:
        return runnable
    if shard_index is None or shard_count is None:
        raise ValueError("shard index and shard count must be supplied together")
    return shard_entries(runnable, shard_index, shard_count)


def _run_suites_locked(entries, opted_in, timeout, expensive_timeout, root=ROOT,
                       execute=run_process, workers=None, load_sample_seconds=5.0,
                       audit_home=False, focused=False, focused_slots=0,
                       shard_index=None, shard_count=None):
    runnable, skipped = _runnable_entries(entries, opted_in)
    runnable = _apply_shard(runnable, shard_index, shard_count)
    failures = []
    with SUITE_OUTCOME_LOCK:
        SUITE_DURATIONS.clear()
        EXPECTED_FAILURES.clear()
        UNEXPECTED_SUCCESSES.clear()
        TEST_OUTCOME_COUNTS.clear()
        HOME_AUDIT_BLOCKED_PATHS.clear()
        HOME_AUDIT_ISOLATED_ENV_EVENTS.clear()
    started = time.monotonic()
    if not runnable:
        return runnable, skipped, failures, time.monotonic() - started

    profile = _load_profile()
    suite_seconds = dict(profile.get("suiteSeconds", {}))
    max_suite_rss = 0
    save_measurements = execute is run_process
    suite_tmp_root = TEST_TMP_ROOT
    owned_tmp_root = None
    plan = None
    explicit_jobs = workers

    def run_one(index, relative, kind, selectors=None, shard_index=0, expected_ids=None):
        suite_started = time.monotonic()
        try:
            scratch = tempfile.TemporaryDirectory(
                dir=suite_tmp_root, prefix=f"{index:03d}-{shard_index:02d}-")
        except OSError as error:
            return relative, f"could not start: {error}", time.monotonic() - suite_started, 0
        try:
            error, peak_rss = run_in_scratch(
                Path(scratch.name), relative, kind, selectors, expected_ids)
        finally:
            cleanup_error = _cleanup_suite_scratch(scratch)
        if cleanup_error:
            error = f"{error}; {cleanup_error}" if error else cleanup_error
        return relative, error, time.monotonic() - suite_started, peak_rss

    def run_in_scratch(temp_root, relative, kind, selectors, expected_ids):
        """Run one suite in its scratch folder; return its error and peak RSS."""
        deadline = expensive_timeout if kind == "expensive" else timeout
        try:
            socket_path_error = _unix_socket_path_error(temp_root)
            if socket_path_error:
                return socket_path_error, 0
            for name in ("cache", "state", "data", "config", "codex-home", "claude-home"):
                (temp_root / name).mkdir()
            environment = _suite_environment(root, temp_root, audit_home=audit_home,
                                             relative=relative)
            if not relative.endswith(".mjs") and (
                    relative.startswith(f"{SOURCE_REL}/studio_api/")
                    or is_unittest_suite(root / relative)):
                environment["CODEX_SERVER_TEST_RESULT_FILE"] = str(
                    _runner_artifact_root(temp_root) / "unittest-outcomes.jsonl")
            command = _suite_command(relative, root)
            if selectors:
                command.extend(selectors)
            result = execute(
                command, root, deadline, environment,
            )
            if audit_home:
                audit_log = Path(environment["CODEX_SERVER_TEST_AUDIT_LOG"])
                if audit_log.exists() and audit_log.stat().st_size:
                    audit_events = audit_log.read_text(encoding="utf-8", errors="replace").splitlines()
                    blocked = [line for line in audit_events
                               if not line.startswith("ALLOWED_")]
                    with SUITE_OUTCOME_LOCK:
                        HOME_AUDIT_BLOCKED_PATHS.extend(blocked)
                        HOME_AUDIT_ISOLATED_ENV_EVENTS.extend(
                            line.partition("\t")[2] for line in audit_events
                            if line.startswith("ALLOWED_ISOLATED_ENV\t")
                        )
                    if blocked:
                        return "real-home audit blocked: " + "; ".join(blocked), 0
            returncode, error, expected, unexpected = result
            if expected_ids is not None:
                outcome_file = Path(environment["CODEX_SERVER_TEST_RESULT_FILE"])
                records = ([json.loads(line) for line in outcome_file.read_text().splitlines()]
                           if outcome_file.exists() else [])
                observed_ids = {record["id"] for record in records}
                if observed_ids != set(expected_ids):
                    missing = sorted(set(expected_ids) - observed_ids)
                    extra = sorted(observed_ids - set(expected_ids))
                    error = (f"test ID mismatch in slice: missing={missing}; extra={extra}")
            if returncode and "CODEX_SERVER_TEST_RESULT_FILE" in environment:
                outcome_file = Path(environment["CODEX_SERVER_TEST_RESULT_FILE"])
                if outcome_file.exists():
                    records = [json.loads(line) for line in outcome_file.read_text().splitlines()]
                    failed_ids = [record["id"] for record in records
                                  if record.get("outcome") in {"failed", "error", "unexpected_success"}]
                    if failed_ids:
                        base_command = _suite_command(relative, root)
                        id_prefix = (relative[len(f"{SOURCE_REL}/"):-3].replace("/", ".") + "."
                                     if relative.startswith(f"{SOURCE_REL}/studio_api/")
                                     else "__main__.")
                        reproduction = [" ".join((*base_command[:-1], "-v",
                                                  test_id.removeprefix(id_prefix)))
                                        for test_id in failed_ids]
                        error = (f"{error or 'test failure'}; failing IDs: {failed_ids}; "
                                 f"reproduce: {'; '.join(reproduction)}")
            with SUITE_OUTCOME_LOCK:
                EXPECTED_FAILURES.extend(f"{relative}::{name}" for name in expected)
                UNEXPECTED_SUCCESSES.extend(f"{relative}::{name}" for name in unexpected)
        except OSError as error:
            return f"could not start: {error}", 0
        peak_rss = _measured_child_peak_rss_bytes()
        if error:
            return error, peak_rss
        if returncode:
            return f"exit {returncode}", peak_rss
        return None, peak_rss

    indexed = [(index, relative, kind)
               for index, (relative, kind) in enumerate(runnable, start=1)]

    plan = worker_plan(runnable, profile, override=focused_slots if focused else workers,
                       sample_seconds=0 if focused or explicit_jobs is not None else load_sample_seconds)
    if focused and focused_slots:
        plan["workers"] = min(focused_slots, plan["memoryWorkerSlots"])
    workers = plan["workers"]

    TEST_TMP_ROOT.mkdir(parents=True, exist_ok=True)
    if TMP_ROOT_OVERRIDE:
        print(f"Scratch root: configured {TEST_TMP_ROOT}", flush=True)
    elif save_measurements:
        simultaneous_runs = 1
        owned_tmp_root = _memory_scratch_root(
            workers, simultaneous_runs=simultaneous_runs,
        )
        if owned_tmp_root is not None:
            suite_tmp_root = owned_tmp_root
            print(f"Scratch root: tmpfs {owned_tmp_root}", flush=True)
        else:
            print(f"Scratch root: disk {TEST_TMP_ROOT} (tmpfs unavailable; standard plan retained)", flush=True)
    if focused:
        print(f"Admission path: focused slots ({focused_slots} slots; "
              f"combined peak RSS bound {MEMORY_RESERVE_BYTES} bytes)", flush=True)
    else:
        print("Admission path: whole-run lock", flush=True)
    if explicit_jobs is None:
        print("Selected automatic worker plan: " + json.dumps(plan, sort_keys=True), flush=True)
    else:
        print("Selected worker plan: " + json.dumps(plan, sort_keys=True), flush=True)
        if explicit_jobs is not None and plan["workers"] < explicit_jobs:
            print(f"explicit --jobs reduced from {explicit_jobs} to {plan['workers']} by "
                  f"{plan['limitingBound']} resource bound", flush=True)

    indexed.sort(key=lambda item: suite_seconds.get(item[1], 0), reverse=True)
    first_pending_index = len(runnable) - len(indexed) + 1
    indexed = [(index, relative, kind)
               for index, (_old_index, relative, kind)
               in enumerate(indexed, start=first_pending_index)]
    # Focused slices use this bounded executor, and focused invocations are
    # serialized so concurrent calls cannot multiply their allocation.
    pool = ThreadPoolExecutor(max_workers=workers)
    futures = []
    shard_totals = {}
    try:
        for index, relative, kind in indexed:
            deadline = expensive_timeout if kind == "expensive" else timeout
            slices = None
            listing_error = None
            if (focused
                    and relative not in NEVER_SLICE_SUITES
                    and not is_http_boundary_suite(relative)
                    and is_unittest_suite(root / relative)
                    and not has_module_level_mutable_state(root / relative)
                    and not has_custom_unittest_suite(root / relative)):
                listing = tempfile.TemporaryDirectory(dir=suite_tmp_root, prefix=f"{index:03d}-list-")
                try:
                    ids = _suite_test_ids(relative, root, Path(listing.name), deadline)
                finally:
                    listing_error = _cleanup_suite_scratch(listing)
                if ids:
                    prefix = (relative[len(f"{SOURCE_REL}/"):-3].replace("/", ".") + "."
                              if relative.startswith(f"{SOURCE_REL}/studio_api/") else "__main__.")
                    if not all(test_id.startswith(prefix) for test_id in ids):
                        ids = None
                if ids:
                    selectors = [test_id.removeprefix(prefix) for test_id in ids]
                    slice_count = _focused_suite_slice_count(relative, len(selectors), workers)
                    if slice_count > 1:
                        slices = _slice_test_ids(selectors, slice_count)
                    expected_prefix = prefix
            if not slices:
                slices = [None]
                expected_prefix = None
            shard_totals[relative] = {"remaining": len(slices), "elapsed": 0.0,
                                      "error": listing_error, "peak": 0}
            print(f"[{index}/{len(runnable)}] {relative} ({len(slices)} slice(s); deadline {deadline:g}s)",
                  flush=True)
            for shard_index, selectors in enumerate(slices):
                expected_ids = ([expected_prefix + selector for selector in selectors]
                                if expected_prefix else None)
                futures.append(pool.submit(run_one, index, relative, kind,
                                           selectors, shard_index, expected_ids))
        completed = as_completed(futures)
        for future in completed:
            relative, error, suite_elapsed, peak_rss = future.result()
            aggregate = shard_totals[relative]
            aggregate["remaining"] -= 1
            aggregate["elapsed"] = max(aggregate["elapsed"], suite_elapsed)
            aggregate["peak"] = max(aggregate["peak"], peak_rss)
            if error:
                aggregate["error"] = error
            if aggregate["remaining"] == 0:
                suite_elapsed = aggregate["elapsed"]
                suite_seconds[relative] = suite_elapsed
                max_suite_rss = max(max_suite_rss, aggregate["peak"])
                with SUITE_OUTCOME_LOCK:
                    SUITE_DURATIONS[relative] = suite_elapsed
                error = aggregate["error"]
                status = f"failed ({error})" if error else "passed"
                print(f"Finished {relative}: {status}, {suite_elapsed:.2f}s", flush=True)
                if error:
                    failures.append((relative, error))
    except BaseException:
        for future in futures:
            future.cancel()
        with ACTIVE_SUITE_LOCK:
            interrupters = tuple(ACTIVE_SUITE_INTERRUPTERS)
        for interrupt in interrupters:
            interrupt()
        pool.shutdown(wait=True, cancel_futures=True)
        raise
    else:
        pool.shutdown(wait=True)
    finally:
        if owned_tmp_root is not None:
            _remove_owned_scratch_root(owned_tmp_root)
    order = {relative: index for index, (relative, _kind) in enumerate(runnable)}
    failures.sort(key=lambda failure: order[failure[0]])
    if execute is run_process:
        with SUITE_OUTCOME_LOCK:
            observed = set(EXPECTED_FAILURES)
            unexpected = set(UNEXPECTED_SUCCESSES)
        runnable_paths = {path for path, _kind in runnable}
        pinned = {test_id for test_id in _pinned_expected_failure_ids()
                  if test_id.split("::", 1)[0] in runnable_paths}
        if observed != pinned:
            failures.append(("expected-failures", f"pinned set mismatch: missing={sorted(pinned-observed)}; "
                             f"extra={sorted(observed-pinned)}"))
        if unexpected:
            failures.append(("unexpected-successes", f"expected-failure tests unexpectedly passed: {sorted(unexpected)}"))
    profile.update({"suiteSeconds": suite_seconds, "maxSuiteRssBytes": max_suite_rss})
    if save_measurements and not focused:
        _save_profile(profile)
    with SUITE_OUTCOME_LOCK:
        diagnostics = {
            "workerPlan": plan,
            "suiteDurations": dict(SUITE_DURATIONS),
        }
    print("Server runner diagnostics: " + json.dumps(diagnostics, sort_keys=True), flush=True)
    return runnable, skipped, failures, time.monotonic() - started


def validate_deadlines(parser, args):
    for flag, value in (("--timeout", args.timeout), ("--expensive-timeout", args.expensive_timeout)):
        if not math.isfinite(value) or value <= 0:
            parser.error(f"{flag} must be finite and greater than zero")


def excluded_helpers(pattern=None):
    helpers = dict(NON_TESTS)
    for path in (TESTS / "fixtures").rglob("*.py"):
        helpers.setdefault(path.relative_to(ROOT).as_posix(), "fixture data or helper")
    if pattern:
        query = pattern.lower()
        helpers = {path: reason for path, reason in helpers.items()
                   if query in path.lower() or fnmatch.fnmatch(path.lower(), query)}
    return sorted(helpers.items())


def _interrupt(signum, _frame):
    raise KeyboardInterrupt(f"received signal {signum}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true", help="list discovered tests and opt-in groups")
    parser.add_argument("--filter", metavar="TEXT_OR_GLOB", help="select paths by substring or glob")
    parser.add_argument("--include", action="append", choices=sorted(OPT_IN), default=[],
                        help="also run this opt-in group (repeatable)")
    parser.add_argument("--only", choices=sorted(OPT_IN),
                        help="run only suites in this execution class")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS,
                        help="deadline per normal test process (default: 120s)")
    parser.add_argument("--expensive-timeout", type=float, default=EXPENSIVE_TIMEOUT_SECONDS,
                        help="deadline per expensive test process (default: 900s)")
    parser.add_argument("--jobs", type=int,
                        default=os.environ.get("CODEX_SERVER_TEST_JOBS"),
                        help="maximum concurrent test processes; set CODEX_SERVER_TEST_JOBS instead for an environment override (default: automatic)")
    parser.add_argument("--shard-index", type=int,
                        help="zero-based server-suite shard index; requires --shard-count")
    parser.add_argument("--shard-count", type=int,
                        help="number of disjoint server-suite shards; requires --shard-index")
    parser.add_argument("--show-jobs", action="store_true",
                        help="show the automatic worker count without running suites")
    parser.add_argument("--audit-home", action="store_true",
                        help="block and report Python filesystem, SQLite, or subprocess access under real user state")
    parser.add_argument("--load-sample-seconds", type=float,
                        default=float(os.environ.get("CODEX_SERVER_TEST_LOAD_SAMPLE_SECONDS", "5")),
                        help="runnable-load sampling window in seconds (default: 5; also CODEX_SERVER_TEST_LOAD_SAMPLE_SECONDS)")
    args = parser.parse_args()
    validate_deadlines(parser, args)
    if args.jobs is not None and args.jobs < 1:
        parser.error("--jobs must be greater than zero")
    if (args.shard_index is None) != (args.shard_count is None):
        parser.error("--shard-index and --shard-count must be supplied together")
    if args.shard_count is not None and args.shard_count < 1:
        parser.error("--shard-count must be at least one")
    if args.shard_index is not None and not 0 <= args.shard_index < args.shard_count:
        parser.error("--shard-index must be between zero and shard count minus one")
    if not math.isfinite(args.load_sample_seconds) or args.load_sample_seconds < 0:
        parser.error("--load-sample-seconds must be a finite, non-negative number")
    entries = select_entries(args.filter)
    if args.only:
        if args.include:
            parser.error("--only cannot be combined with --include")
        entries = only_category(entries, args.only)
    implicit_opt_ins = exact_filter_opt_ins(args.filter, entries)
    opted_in = set(args.include) | ({args.only} if args.only else set()) | implicit_opt_ins
    if args.shard_index is not None:
        try:
            _apply_shard(_runnable_entries(entries, opted_in)[0], args.shard_index, args.shard_count)
        except ValueError as error:
            parser.error(str(error))
    load_sample_seconds = args.load_sample_seconds
    if args.filter:
        selected_runnable = sum(
            1 for _path, kind in entries
            if kind in {"safe", "component"} or kind in args.include or kind in implicit_opt_ins
        )
        if selected_runnable < available_cpu_count():
            load_sample_seconds = 0.0
    if args.show_jobs:
        runnable, _skipped = _runnable_entries(entries, opted_in)
        try:
            runnable = _apply_shard(runnable, args.shard_index, args.shard_count)
        except ValueError as error:
            parser.error(str(error))
        profile = _load_profile()
        focused_slots = (_focused_slot_count(profile)
                         if len(runnable) == FOCUSED_SUITE_LIMIT else 0)
        focused = focused_slots > 0
        plan = worker_plan(runnable, profile, override=1 if focused else args.jobs,
                           sample_seconds=0 if focused else load_sample_seconds)
        plan["admissionPath"] = "focused-slots" if focused else "whole-run-lock"
        if focused:
            plan["focusedSlots"] = focused_slots
            plan["focusedPeakRssBoundBytes"] = MEMORY_RESERVE_BYTES
            print(f"Focused worker plan: one worker; {focused_slots} per-user admission slots")
        else:
            print("Automatic worker formula: min(max(ceil(CPUs allowed / 4), "
                  f"CPUs allowed - {load_sample_seconds:g}-second median of sampled runnable tasks "
                  "excluding this runner), floor(min(50% of available memory, available memory - "
                  "4 GiB reserve) / measured peak suite RSS), runnable suite count); explicit jobs "
                  "request a count subject to memory and runner capacity")
        print("Admission path: " + plan["admissionPath"])
        print("Worker plan: " + json.dumps(plan, sort_keys=True))
        return 0
    if args.list:
        if args.shard_index is None:
            listed = entries
        else:
            runnable, _skipped = _runnable_entries(entries, opted_in)
            try:
                listed = _apply_shard(runnable, args.shard_index, args.shard_count)
            except ValueError as error:
                parser.error(str(error))
        for path, kind in listed:
            label = "default" if kind in {"safe", "component"} else f"opt-in:{kind}"
            print(f"{label:18} {path}")
        helpers = excluded_helpers(args.filter)
        for path, reason in sorted(helpers):
            print(f"excluded-helper    {path} ({reason})")
        print(f"{len(entries)} tests discovered; {len(helpers)} helpers explicitly excluded")
        return 0
    if not entries:
        helpers = excluded_helpers(args.filter)
        if helpers:
            print("Filter matches excluded non-test helper(s): "
                  + ", ".join(path for path, _ in helpers), file=sys.stderr)
        else:
            print("No server tests matched the filter.", file=sys.stderr)
        return 2
    if (args.only == "expensive" or "expensive" in args.include) and len(entries) != 1:
        parser.error("load-class suites must be selected one at a time")
    runnable, skipped, failures, elapsed = run_suites(
        entries, opted_in, args.timeout, args.expensive_timeout,
        workers=args.jobs, load_sample_seconds=load_sample_seconds,
        audit_home=args.audit_home, shard_index=args.shard_index,
        shard_count=args.shard_count)
    for path, kind in skipped:
        if kind == "environment":
            print(f"SKIP {path}: no executable Codex app-server found (set CODEX_BIN, install codex on PATH, or install ~/.local/bin/codex)")
        else:
            condition = ("an already provisioned, isolated Linux VM and --include vm"
                         if kind == "vm" else f"--include {kind}")
            print(f"SKIP {path}: requires {condition}")
    if not runnable:
        print(f"No runnable server suites: {len(skipped)} skipped (opt-in).", file=sys.stderr)
        return 2
    opt_in_skips = sum(kind != "environment" for _path, kind in skipped)
    environment_skips = len(skipped) - opt_in_skips
    failed_suite_paths = {path for path, _reason in failures if path in {item[0] for item in runnable}}
    runner_errors = len(failures) - len(failed_suite_paths)
    print(f"Server suites: {len(runnable) - len(failed_suite_paths)} passed, "
          f"{len(failed_suite_paths)} failed, {opt_in_skips} skipped (opt-in), "
          f"{environment_skips} skipped (environment), {elapsed:.1f}s; "
          f"plain assertion scripts are counted per suite; {runner_errors} runner errors")
    with SUITE_OUTCOME_LOCK:
        expected = sorted(EXPECTED_FAILURES)
        unexpected = sorted(UNEXPECTED_SUCCESSES)
    print(f"Expected failures: {len(expected)}" + ("; " + ", ".join(expected) if expected else ""))
    print(f"Unexpected successes: {len(unexpected)}" + ("; " + ", ".join(unexpected) if unexpected else ""))
    with SUITE_OUTCOME_LOCK:
        outcome_counts = dict(TEST_OUTCOME_COUNTS)
    print("Test outcomes: " + json.dumps(outcome_counts, sort_keys=True))
    if args.audit_home:
        with SUITE_OUTCOME_LOCK:
            blocked = sorted(HOME_AUDIT_BLOCKED_PATHS)
            isolated_env_events = len(HOME_AUDIT_ISOLATED_ENV_EVENTS)
        print(f"HOME audit: {len(blocked)} blocked accesses" +
              ("; " + "; ".join(blocked) if blocked else ""))
        print("HOME audit permitted real-home executable: " +
              (RESOLVED_CODEX_BIN or "none"))
        print(f"HOME audit isolated Codex child environments: {isolated_env_events}")
    for path, reason in failures:
        print(f"FAIL {path}: {reason}")
    if not args.only and not args.include and all(
        kind in {"safe", "component"} for _path, kind in runnable
    ):
        over_budget = sorted(
            ((path, SUITE_DURATIONS[path]) for path, _kind in runnable
             if SUITE_DURATIONS.get(path, 0) > FAST_SUITE_BUDGET_SECONDS),
            key=lambda item: (-item[1], item[0]),
        )
        if over_budget:
            details = ", ".join(f"{path} ({duration:.2f}s)" for path, duration in over_budget)
            print(f"Parallel-run default-tier suites over {FAST_SUITE_BUDGET_SECONDS:g}s "
                  f"(check alone with 'just test server <exact-suite-path>'): {details}")
        else:
            print(f"Parallel-run default-tier suites over {FAST_SUITE_BUDGET_SECONDS:g}s "
                  f"(check alone with 'just test server <exact-suite-path>'): none")
    return 1 if failures or unexpected else 0


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, _interrupt)
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("Server suite run interrupted; active suite process group terminated.", file=sys.stderr)
        raise SystemExit(130)

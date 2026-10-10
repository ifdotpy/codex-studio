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
    f"{SOURCE_REL}/transcript_storage/tests",
)
SCENARIO_BENCHMARK_ROOTS = (
    f"{SOURCE_REL}/sync/benchmarks/message_delivery",
    f"{SOURCE_REL}/studio_api/benchmarks/runtime_load",
)
STUDIO_API_COMPONENT_ROOT = SERVER_SOURCE_ROOT / "studio_api"
LEGACY_SERVER_JS = {
    f"{TESTS_REL}/portable-smoke.mjs": "safe",
    f"{TESTS_REL}/state-contract-smoke.mjs": "safe",
    f"{TESTS_REL}/swarm-retry-contract.mjs": "expensive",
    f"{SOURCE_REL}/studio_api/benchmarks/runtime_load/test_http_outcomes.mjs": "expensive",
}
OPT_IN = {"native", "live", "expensive", "browser", "vm"}
DEFAULT_TIMEOUT_SECONDS = 120
EXPENSIVE_TIMEOUT_SECONDS = 900
TEST_TMP_ROOT = Path(os.environ.get(
    "CODEX_SERVER_TEST_TMP_ROOT", Path.home() / ".cache" / "cs" / "st",
)).expanduser()
RUNNER_LOCK_PATH = Path.home() / ".cache" / "cs" / "st" / "runner-live.lock"
REPOSITORY_KEY = hashlib.sha256(str(ROOT.resolve()).encode("utf-8")).hexdigest()[:12]
PROFILE_PATH = RUNNER_LOCK_PATH.parent / f"runner-profile-{REPOSITORY_KEY}.json"
MEMORY_RESERVE_BYTES = 4 * 1024**3
BASELINE_PATH = Path(__file__).with_name("timing-baseline.json")
TMP_ROOT_OVERRIDE = os.environ.get("CODEX_SERVER_TEST_TMP_ROOT")
MAX_SHORT_TMP_ROOT_BYTES = 42
MAX_UNIX_SOCKET_PATH_BYTES = 103
SCRATCH_PROBE_MIN_BYTES = 64 * 1024 * 1024
SCRATCH_BYTES_PER_WORKER = 256 * 1024 * 1024
SCRATCH_PROBE_CHUNK_BYTES = 8 * 1024 * 1024
SCRATCH_OWNER_FILE = ".codex-server-test-owner"

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
HOME_AUDIT_BLOCKED_PATHS = []
HOME_AUDIT_ISOLATED_ENV_EVENTS = []
EXPECTED_FAILURES_PATH = TESTS / "server" / "expected_failures.txt"


def resolve_codex_executable():
    """Resolve the single app-server executable used by terminal integration tests."""
    explicit = os.environ.get("CODEX_BIN")
    if explicit:
        candidate = Path(explicit).expanduser()
    else:
        located = shutil.which("codex")
        candidate = Path(located) if located else Path.home() / ".local" / "bin" / "codex"
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
    f"{TESTS_REL}/native-action-context-repair-contract.py",
    f"{TESTS_REL}/native-action-receipts-contract.py", f"{TESTS_REL}/native-action-settings-contract.py",
    f"{TESTS_REL}/native-binary-contract.py", f"{TESTS_REL}/native-command-controls-contract.py",
    f"{TESTS_REL}/native-error-contract.py", f"{TESTS_REL}/native-notification-lookup-contract.py",
    f"{TESTS_REL}/native-release-contract.py", f"{TESTS_REL}/native-runtime-updates-contract.py",
    f"{TESTS_REL}/native-safety-contract.py", f"{TESTS_REL}/native-tools-contract.py",
    f"{TESTS_REL}/native-tools-shutdown-contract.py", f"{TESTS_REL}/native-voice-admission-contract.py",
    f"{TESTS_REL}/native-voice-contract.py",
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
    f"{TESTS_REL}/analytics-usage-memory-contract.py", f"{TESTS_REL}/cost-scanner-contract.py",
    f"{TESTS_REL}/costs-contract.py", f"{TESTS_REL}/limit-payloads-contract.py",
    f"{TESTS_REL}/notification-load-contract.py",
    f"{TESTS_REL}/payload-storage-contract.py", f"{TESTS_REL}/peer-conversion-scale-contract.py",
    f"{TESTS_REL}/preparation-unload-contract.py",
    f"{TESTS_REL}/pricing-session-cost-contract.py", f"{TESTS_REL}/provider-replay-runner.py",
    f"{TESTS_REL}/scheduler-disk-full-contract.py",
    f"{TESTS_REL}/session-cost-memory-contract.py", f"{TESTS_REL}/session-cost-refresh-contract.py",
    f"{TESTS_REL}/session-cost-scan-contract.py",
    f"{TESTS_REL}/supervisor-stream-phase-load-contract.py",
    f"{TESTS_REL}/sync-read-latency-contract.py",
    f"{TESTS_REL}/transcript-latency-contract.py", f"{TESTS_REL}/transcript-streaming-write-volume-contract.py",
    f"{TESTS_REL}/swarm-retry-contract.mjs",
    f"{SOURCE_REL}/sync/benchmarks/message_delivery/test_benchmark.py",
    f"{SOURCE_REL}/studio_api/benchmarks/runtime_load/test_runtime_load.py",
})
NON_TESTS = {
    f"{TESTS_REL}/test_isolation.py": "shared fixture helper",
    f"{TESTS_REL}/vm-native-fixture.py": "shared VM provider fixture helper",
    f"{TESTS_REL}/host-exec-native.py": "manual live VM proof that needs --state, --agent and --cwd",
    f"{TESTS_REL}/mobile-startup-fixture.py": "browser fixture helper",
    f"{TESTS_REL}/mobile-usability-fixture.py": "browser fixture helper",
    f"{TESTS_REL}/runtime-read-lock-fixture.py": "runtime fixture helper",
    f"{TESTS_REL}/sidebar-drag-fixture.py": "browser fixture helper",
    f"{TESTS_REL}/simple-ui-fixture.py": "browser fixture helper",
    f"{TESTS_REL}/native-action-ui-fixture.py": "browser fixture helper",
    f"{TESTS_REL}/provider-replay-server.py": "provider subprocess fixture, not a test entrypoint",
    f"{TESTS_REL}/server/rpc_replay_contract.py": "shared fixture RPC allowlist",
    f"{TESTS_REL}/server/suite_entry.py": "server suite runner entrypoint",
    f"{TESTS_REL}/sync-live-patch-http-contract.py":
        "fixture harness requiring an injected legacy HTTP server and runtime",
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
    for name in ("runtime-live.py", "time-awareness.py", "tool-parity.py",
                 "workspace-protocol.py", "workspace-races.py", "workspace-native-turn.py",
                 "provider-replay-runner.py"):
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


def selected(entries, pattern):
    if not pattern:
        return entries
    query = pattern.lower()
    return [(path, kind) for path, kind in entries
            if query in path.lower() or fnmatch.fnmatch(path.lower(), query)]


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
    # A host-local `codex` shim commonly resolves into ~/.codex/packages. Do
    # not let isolated tests discover or launch it. Keep unrelated PATH tools,
    # but remove entries whose codex executable resolves into protected state.
    protected_codex = (Path.home() / ".codex").resolve()
    inherited_path = []
    for entry in environment.get("PATH", "").split(os.pathsep):
        candidate = Path(entry or ".") / "codex"
        try:
            if candidate.resolve().is_relative_to(protected_codex):
                continue
        except OSError:
            pass
        inherited_path.append(entry)
    environment["PATH"] = os.pathsep.join((str(Path(sys.executable).parent), *inherited_path))
    configured_codex = environment.get("CODEX_BIN")
    if configured_codex:
        try:
            if Path(configured_codex).resolve().is_relative_to(protected_codex):
                environment.pop("CODEX_BIN", None)
        except OSError:
            pass
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
    """Remove only runner scratch directories whose PID/start identity is gone."""
    for mount in _tmpfs_mounts() if mounts is None else mounts:
        try:
            candidates = mount.glob("csst-*")
        except OSError:
            continue
        for directory in candidates:
            if not directory.is_dir():
                continue
            try:
                owner = json.loads((directory / SCRATCH_OWNER_FILE).read_text(encoding="ascii"))
                pid, started = int(owner["pid"]), str(owner["startTime"])
            except (OSError, ValueError, KeyError, TypeError):
                continue
            if _process_start_time(pid) != started:
                shutil.rmtree(directory, ignore_errors=True)


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



def _memory_scratch_root(workers):
    """Use tmpfs only with 256 MiB of measured headroom per planned worker."""
    mounts = list(_tmpfs_mounts())
    _cleanup_stale_scratch_roots(mounts)
    candidates = []
    for mount in mounts:
        owned = None
        try:
            if shutil.disk_usage(mount).free < workers * SCRATCH_BYTES_PER_WORKER:
                continue
            owned = Path(tempfile.mkdtemp(prefix="csst-", dir=mount))
            (owned / SCRATCH_OWNER_FILE).write_text(json.dumps({
                "pid": os.getpid(), "startTime": _process_start_time(os.getpid())}), encoding="ascii")
            if len(os.fsencode(owned)) > MAX_SHORT_TMP_ROOT_BYTES:
                shutil.rmtree(owned)
                continue
            if not _probe_scratch_capacity(owned, SCRATCH_PROBE_MIN_BYTES):
                shutil.rmtree(owned)
                continue
            candidates.append((shutil.disk_usage(mount).free, owned))
        except OSError:
            if owned is not None:
                shutil.rmtree(owned, ignore_errors=True)
    if not candidates:
        return None
    candidates.sort(key=lambda candidate: candidate[0], reverse=True)
    selected = candidates[0][1]
    for _free, unused in candidates[1:]:
        shutil.rmtree(unused)
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
               audit_home=False):
    with _runner_live_lock():
        return _run_suites_locked(entries, opted_in, timeout, expensive_timeout,
                                  root, execute, workers, load_sample_seconds, audit_home)


def _run_suites_locked(entries, opted_in, timeout, expensive_timeout, root=ROOT,
                       execute=run_process, workers=None, load_sample_seconds=5.0,
                       audit_home=False):
    runnable = []
    skipped = []
    for path, kind in entries:
        if path == TERMINALS_SUITE and not RESOLVED_CODEX_BIN:
            skipped.append((path, "environment"))
        elif kind in {"safe", "component"} or kind in opted_in:
            runnable.append((path, kind))
        else:
            skipped.append((path, kind))
    failures = []
    with SUITE_OUTCOME_LOCK:
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

    def run_one(index, relative, kind):
        deadline = expensive_timeout if kind == "expensive" else timeout
        print(f"[{index}/{len(runnable)}] {relative} (deadline {deadline:g}s)", flush=True)
        suite_started = time.monotonic()
        try:
            with tempfile.TemporaryDirectory(dir=suite_tmp_root, prefix=f"{index:03d}-") as temporary:
                temp_root = Path(temporary)
                socket_path_error = _unix_socket_path_error(temp_root)
                if socket_path_error:
                    return (relative, socket_path_error,
                            time.monotonic() - suite_started, 0)
                for name in ("cache", "state", "data", "config", "codex-home", "claude-home"):
                    (temp_root / name).mkdir()
                environment = _suite_environment(root, temp_root, audit_home=audit_home,
                                                 relative=relative)
                if not relative.endswith(".mjs") and (
                        relative.startswith(f"{SOURCE_REL}/studio_api/")
                        or is_unittest_suite(root / relative)):
                    environment["CODEX_SERVER_TEST_RESULT_FILE"] = str(
                        _runner_artifact_root(temp_root) / "unittest-outcomes.jsonl")
                result = execute(
                    _suite_command(relative, root), root, deadline, environment,
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
                            return relative, "real-home audit blocked: " + "; ".join(blocked), time.monotonic() - suite_started, 0
                returncode, error, expected, unexpected = result
                with SUITE_OUTCOME_LOCK:
                    EXPECTED_FAILURES.extend(f"{relative}::{name}" for name in expected)
                    UNEXPECTED_SUCCESSES.extend(f"{relative}::{name}" for name in unexpected)
        except OSError as error:
            return relative, f"could not start: {error}", time.monotonic() - suite_started, 0
        peak_rss = _measured_child_peak_rss_bytes()
        if error:
            return relative, error, time.monotonic() - suite_started, peak_rss
        if returncode:
            return relative, f"exit {returncode}", time.monotonic() - suite_started, peak_rss
        return relative, None, time.monotonic() - suite_started, peak_rss

    indexed = [(index, relative, kind)
               for index, (relative, kind) in enumerate(runnable, start=1)]

    plan = worker_plan(runnable, profile, override=workers,
                       sample_seconds=load_sample_seconds if explicit_jobs is None else 0)
    workers = plan["workers"]

    TEST_TMP_ROOT.mkdir(parents=True, exist_ok=True)
    if TMP_ROOT_OVERRIDE:
        print(f"Scratch root: configured {TEST_TMP_ROOT}", flush=True)
    elif save_measurements:
        owned_tmp_root = _memory_scratch_root(workers)
        if owned_tmp_root is not None:
            suite_tmp_root = owned_tmp_root
            print(f"Scratch root: tmpfs {owned_tmp_root}", flush=True)
        else:
            print(f"Scratch root: disk {TEST_TMP_ROOT} (tmpfs unavailable; standard plan retained)", flush=True)
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
    pool = ThreadPoolExecutor(max_workers=workers)
    futures = []
    try:
        for item in indexed:
            futures.append(pool.submit(run_one, *item))
        completed = as_completed(futures)
        for future in completed:
            relative, error, suite_elapsed, peak_rss = future.result()
            suite_seconds[relative] = suite_elapsed
            max_suite_rss = max(max_suite_rss, peak_rss)
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
            shutil.rmtree(owned_tmp_root)
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
    if save_measurements:
        _save_profile(profile)
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
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS,
                        help="deadline per normal test process (default: 120s)")
    parser.add_argument("--expensive-timeout", type=float, default=EXPENSIVE_TIMEOUT_SECONDS,
                        help="deadline per expensive test process (default: 900s)")
    parser.add_argument("--jobs", type=int,
                        default=os.environ.get("CODEX_SERVER_TEST_JOBS"),
                        help="maximum concurrent test processes; set CODEX_SERVER_TEST_JOBS instead for an environment override (default: automatic)")
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
    if not math.isfinite(args.load_sample_seconds) or args.load_sample_seconds < 0:
        parser.error("--load-sample-seconds must be a finite, non-negative number")
    entries = selected(inventory(), args.filter)
    load_sample_seconds = args.load_sample_seconds
    if args.filter:
        selected_runnable = sum(
            1 for _path, kind in entries
            if kind in {"safe", "component"} or kind in args.include
        )
        if selected_runnable < available_cpu_count():
            load_sample_seconds = 0.0
    if args.show_jobs:
        opted_in = set(args.include)
        runnable = [(path, kind) for path, kind in entries
                    if (kind in {"safe", "component"} or kind in opted_in)
                    and (path != TERMINALS_SUITE or RESOLVED_CODEX_BIN)]
        plan = worker_plan(runnable, override=args.jobs, sample_seconds=load_sample_seconds)
        print("Automatic worker formula: min(max(ceil(CPUs allowed / 4), "
              f"CPUs allowed - {load_sample_seconds:g}-second median of sampled runnable tasks "
              "excluding this runner), floor(min(50% of available memory, available memory - "
              "4 GiB reserve) / measured peak suite RSS), runnable suite count); explicit jobs "
              "request a count subject to memory and runner capacity")
        print("Worker plan: " + json.dumps(plan, sort_keys=True))
        return 0
    if args.list:
        for path, kind in entries:
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
    opted_in = set(args.include)
    runnable, skipped, failures, elapsed = run_suites(
        entries, opted_in, args.timeout, args.expensive_timeout,
        workers=args.jobs, load_sample_seconds=load_sample_seconds,
        audit_home=args.audit_home)
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
    return 1 if failures or unexpected else 0


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, _interrupt)
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("Server suite run interrupted; active suite process group terminated.", file=sys.stderr)
        raise SystemExit(130)

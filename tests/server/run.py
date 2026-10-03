#!/usr/bin/env python3
"""Discover and run isolated server-side Python suites."""

import argparse
import ast
import fnmatch
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

ROOT = Path(__file__).resolve().parents[2]
TESTS = ROOT / "tests"
COMPONENT_ROOTS = (
    "scripts/analytics/tests",
    "scripts/sync/tests",
    "scripts/native_notifications/tests",
    "scripts/transcript_storage/tests",
)
LEGACY_SERVER_JS = {
    "tests/portable-smoke.mjs": "safe",
    "tests/state-contract-smoke.mjs": "safe",
    "tests/swarm-retry-contract.mjs": "expensive",
    "scripts/benchmarks/runtime_load/test_http_outcomes.mjs": "expensive",
}
OPT_IN = {"native", "live", "expensive", "browser"}
DEFAULT_TIMEOUT_SECONDS = 120
EXPENSIVE_TIMEOUT_SECONDS = 900

NATIVE_SUITES = frozenset({
    "tests/account-transfer-native.py", "tests/agent-review-native.py",
    "tests/claude-monitor-native.py", "tests/context-repair-native.py",
    "tests/monitor-completion-native.py", "tests/monitor-continuation-native.py",
    "tests/monitor-shell-native.py", "tests/native-primitives-integration.py",
    "tests/native-safety-integration.py", "tests/native-tools-native.py",
    "tests/portable-history-native.py", "tests/session-names-native.py",
    "tests/spawn-recovery-native.py", "tests/time-awareness-native.py",
    "tests/native-action-context-repair-contract.py",
    "tests/native-action-receipts-contract.py", "tests/native-action-settings-contract.py",
    "tests/native-binary-contract.py", "tests/native-command-controls-contract.py",
    "tests/native-error-contract.py", "tests/native-notification-lookup-contract.py",
    "tests/native-release-contract.py", "tests/native-runtime-updates-contract.py",
    "tests/native-safety-contract.py", "tests/native-tools-contract.py",
    "tests/native-tools-shutdown-contract.py", "tests/native-voice-admission-contract.py",
    "tests/native-voice-contract.py",
})
BROWSER_SUITES = frozenset({
    "tests/browser-backend-smoke.py", "tests/browser-lock-contract.py",
    "tests/browser-native-contract.py", "tests/browser-recovery-contract.py",
})
LIVE_SUITES = frozenset({"tests/runtime-live.py"})
EXPENSIVE_SUITES = frozenset({
    "tests/account-costs-contract.py", "tests/analytics-memory-contract.py",
    "tests/analytics-usage-memory-contract.py", "tests/cost-scanner-contract.py",
    "tests/costs-contract.py", "tests/execution-migration-benchmark.py",
    "tests/execution-write-cost.py", "tests/limit-payloads-contract.py",
    "tests/notification-load-contract.py", "tests/payload-storage-benchmark.py",
    "tests/payload-storage-contract.py", "tests/peer-conversion-scale-contract.py",
    "tests/perf-renderer-sync.py", "tests/preparation-unload-contract.py",
    "tests/pricing-session-cost-contract.py", "tests/provider-replay-runner.py",
    "tests/scheduler-disk-full-contract.py", "tests/search-index-latency.py",
    "tests/session-cost-memory-contract.py", "tests/session-cost-refresh-contract.py",
    "tests/session-cost-scan-contract.py", "tests/streaming-write-volume.py",
    "tests/supervisor-stream-phase-load-contract.py", "tests/sync-entity-measure.py",
    "tests/sync-read-latency-contract.py", "tests/sync-write-volume.py",
    "tests/transcript-latency-contract.py", "tests/transcript-streaming-write-volume-contract.py",
    "tests/swarm-retry-contract.mjs",
    "scripts/benchmarks/message_delivery/test_benchmark.py",
    "scripts/benchmarks/parallel_agents/test_benchmark.py",
    "scripts/benchmarks/runtime_load/test_runtime_load.py",
})
NON_TESTS = {
    "tests/test_isolation.py": "shared fixture helper",
    "tests/delivery-latency-fixture.py": "listener fixture; invoked by its benchmark harness",
    "tests/remaining-delivery-latency-fixture.py": "listener fixture; invoked by its benchmark harness",
    "tests/mobile-startup-fixture.py": "browser fixture helper",
    "tests/mobile-usability-fixture.py": "browser fixture helper",
    "tests/runtime-read-lock-fixture.py": "runtime fixture helper",
    "tests/sidebar-drag-fixture.py": "browser fixture helper",
    "tests/simple-ui-fixture.py": "browser fixture helper",
    "tests/native-action-ui-fixture.py": "browser fixture helper",
    "tests/provider-replay-server.py": "provider subprocess fixture, not a test entrypoint",
    "tests/server/rpc_replay_contract.py": "shared fixture RPC allowlist",
    "tests/fixtures/current_cleanup_receipts.py": "test fixture data",
    "tests/fixtures/current_cleanup_state.py": "test fixture data",
    "scripts/benchmarks/message_delivery/benchmark.py": "manual benchmark entrypoint",
    "scripts/benchmarks/parallel_agents/benchmark.py": "manual benchmark entrypoint",
    "scripts/benchmarks/runtime_load/server.py": "load-test fixture server",
    "scripts/benchmarks/runtime_load/run.mjs": "manual load-test entrypoint",
    "scripts/benchmarks/runtime_load/http_outcomes.mjs": "load-test helper module",
    "scripts/benchmarks/runtime-read-scopes.py": "manual benchmark entrypoint",
    "scripts/benchmarks/scheduler-full-pass-lock.py": "manual benchmark entrypoint",
    "scripts/benchmarks/scheduler-record-lock.py": "manual benchmark entrypoint",
}


def category(path):
    """Return the reviewed execution class for a discovered suite path."""
    relative = path.relative_to(ROOT).as_posix()
    if relative in BROWSER_SUITES:
        return "browser"
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
        if relative in NON_TESTS or "tests/fixtures/" in relative:
            continue
        if is_unittest_suite(path):
            entries[relative] = "component" if "/tests/" in relative and relative.startswith("scripts/") else category(path)
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
    for path in (ROOT / "scripts/benchmarks").rglob("test_*.py"):
        entries[path.relative_to(ROOT).as_posix()] = category(path)
    for path, kind in LEGACY_SERVER_JS.items():
        if (ROOT / path).is_file():
            entries[path] = kind
    # These are deliberately opt-in even though their names do not contain a
    # native/live marker: they exercise measured or broad resource behavior.
    for name in ("execution-migration-benchmark.py", "execution-write-cost.py",
                 "search-index-latency.py", "streaming-write-volume.py",
                 "sync-entity-measure.py", "sync-write-volume.py",
                 "payload-storage-benchmark.py", "perf-renderer-sync.py"):
        path = TESTS / name
        if path.is_file():
            entries[path.relative_to(ROOT).as_posix()] = "expensive"
    for directory in COMPONENT_ROOTS:
        for path in (ROOT / directory).glob("test_*.py"):
            entries[path.relative_to(ROOT).as_posix()] = "component"
    return sorted(entries.items())


def selected(entries, pattern):
    if not pattern:
        return entries
    query = pattern.lower()
    return [(path, kind) for path, kind in entries
            if query in path.lower() or fnmatch.fnmatch(path.lower(), query)]


def run_process(command, cwd, timeout, environment):
    """Run one suite in its own process group and reap it on every exit path."""
    process = subprocess.Popen(command, cwd=cwd, env=environment,
                               start_new_session=(os.name != "nt"))

    def terminate_group():
        try:
            if os.name != "nt":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    try:
        return process.wait(timeout=timeout), None
    except subprocess.TimeoutExpired:
        terminate_group()
        return None, f"timed out after {timeout:g}s"
    except BaseException:
        terminate_group()
        raise


def run_suites(entries, opted_in, timeout, expensive_timeout, root=ROOT, execute=run_process):
    runnable = [(path, kind) for path, kind in entries
                if kind in {"safe", "component"} or kind in opted_in]
    skipped = [(path, kind) for path, kind in entries
               if kind not in {"safe", "component"} and kind not in opted_in]
    failures = []
    started = time.monotonic()
    for index, (relative, kind) in enumerate(runnable, start=1):
        deadline = expensive_timeout if kind == "expensive" else timeout
        print(f"[{index}/{len(runnable)}] {relative} (deadline {deadline:g}s)", flush=True)
        command = (["node", str(root / relative)] if relative.endswith(".mjs") else
                   [sys.executable, "-B", str(root / relative)])
        environment = os.environ.copy()
        scripts_path = str(root / "scripts")
        environment["PYTHONPATH"] = os.pathsep.join(
            item for item in (scripts_path, environment.get("PYTHONPATH", "")) if item)
        try:
            returncode, error = execute(command, root, deadline, environment)
        except OSError as error:
            failures.append((relative, f"could not start: {error}"))
            continue
        if error:
            failures.append((relative, error))
        elif returncode:
            failures.append((relative, f"exit {returncode}"))
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
    args = parser.parse_args()
    validate_deadlines(parser, args)
    entries = selected(inventory(), args.filter)
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
        entries, opted_in, args.timeout, args.expensive_timeout)
    for path, kind in skipped:
        print(f"SKIP {path}: requires --include {kind}")
    if not runnable:
        print(f"No runnable server suites: {len(skipped)} skipped (opt-in).", file=sys.stderr)
        return 2
    print(f"Server suites: {len(runnable) - len(failures)} passed, {len(failures)} failed, "
          f"{len(skipped)} skipped (opt-in), {elapsed:.1f}s")
    for path, reason in failures:
        print(f"FAIL {path}: {reason}")
    return 1 if failures else 0


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, _interrupt)
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("Server suite run interrupted; active suite process group terminated.", file=sys.stderr)
        raise SystemExit(130)

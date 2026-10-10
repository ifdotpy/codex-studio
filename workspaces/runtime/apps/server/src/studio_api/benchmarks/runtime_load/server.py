#!/usr/bin/env python3
"""Start an isolated production Runtime/HTTP server for the runtime load harness."""
from __future__ import annotations

import json
import os
from pathlib import Path
import queue
import resource
import copy
import atexit
import faulthandler
from contextlib import contextmanager
import hashlib
import math
import signal
import sqlite3
import subprocess
import sys
import threading
import time
import uuid
from collections import defaultdict, deque
from unittest.mock import patch


ROOT = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else Path(__file__).resolve().parents[4]
sys.path.insert(0, str(ROOT / "scripts"))
FRONTEND_DIST = Path(sys.argv[3]).resolve() if len(sys.argv) > 3 and sys.argv[3] else None


def diagnostic_synchronous_mode():
    mode = os.environ.get("BENCH_SQLITE_SYNCHRONOUS", "FULL").upper()
    if mode not in {"FULL", "NORMAL"}:
        raise ValueError("BENCH_SQLITE_SYNCHRONOUS must be FULL or NORMAL")
    return mode


def configure_diagnostic_synchronous(db, mode):
    if mode not in {"FULL", "NORMAL"}:
        raise ValueError("diagnostic SQLite synchronous mode must be FULL or NORMAL")
    db.execute(f"PRAGMA synchronous={mode}")


def idle_connection_diagnostic_enabled():
    enabled = os.environ.get("BENCH_SQLITE_IDLE_CONNECTION", "0")
    if enabled not in {"0", "1"}:
        raise ValueError("BENCH_SQLITE_IDLE_CONNECTION must be 0 or 1")
    return enabled == "1"


def open_idle_connection(database_path):
    """Prime a connection before workload, then hold it transaction-free."""
    connection = sqlite3.connect(database_path, timeout=15)
    try:
        cursor = connection.execute("SELECT name FROM sqlite_master")
        cursor.fetchall()
        cursor.close()
        if connection.in_transaction:
            raise RuntimeError("idle SQLite diagnostic retained its priming read transaction")
    except BaseException:
        connection.close()
        raise
    return connection


def database_filesystem_type(path):
    """Return only mounted filesystem type; never disclose the state path."""
    resolved = os.path.realpath(path)
    best_length = -1
    filesystem_type = "unknown"
    try:
        with open("/proc/self/mountinfo", encoding="utf-8") as mounts:
            for line in mounts:
                before, separator, after = line.rstrip("\n").partition(" - ")
                if not separator:
                    continue
                fields = before.split()
                mountpoint = fields[4].replace("\\040", " ").replace("\\011", "\t")
                if resolved == mountpoint or resolved.startswith(mountpoint.rstrip("/") + "/"):
                    if len(mountpoint) > best_length:
                        filesystem_type = after.split()[0]
                        best_length = len(mountpoint)
    except OSError:
        pass
    return filesystem_type


class FakePipe:
    def __init__(self):
        self.lines = queue.Queue()

    def __iter__(self):
        while (line := self.lines.get()) is not None:
            yield line


class FakeAppProcess:
    """In-memory JSON-lines peer; production AppServer owns reader and FIFO."""
    instances = []

    def __init__(self, *args, **kwargs):
        self.stdout = FakePipe()
        self.stdin = self
        self.exited = threading.Event()
        self.writes = []
        type(self).instances.append(self)

    def emit(self, value):
        self.stdout.lines.put(json.dumps(value) + "\n")

    def write(self, text):
        value = json.loads(text)
        self.writes.append(value)
        if value.get("method") == "initialize":
            self.emit({"id": value["id"], "result": {}})

    def flush(self):
        return

    def poll(self):
        return 0 if self.exited.is_set() else None

    def terminate(self):
        if not self.exited.is_set():
            self.exited.set()
            self.stdout.lines.put(None)

    kill = terminate

    def wait(self, timeout=None):
        if not self.exited.wait(timeout):
            raise subprocess.TimeoutExpired("runtime-load-fake", timeout)
        return 0


class BenchmarkRuntimeMixin:
    def schedule(self):
        # Workload identities stand in for already-running native turns. Never
        # launch native sessions or consume provider capacity in this fixture.
        return

    @contextmanager
    def db(self, *, busy_timeout=None):
        mode = diagnostic_synchronous_mode()
        with super().db(busy_timeout=busy_timeout) as db:
            configure_diagnostic_synchronous(db, mode)
            yield db


def percentile(values, p):
    if not values:
        return None
    ordered = sorted(values)
    import math
    return ordered[max(0, math.ceil(p * len(ordered)) - 1)]


def stats(values):
    return {"samples": len(values), "p50": percentile(values, .5),
            "p95": percentile(values, .95), "p99": percentile(values, .99),
            "max": max(values) if values else None}


def timing_stats(values, sample_count=None, total_ms=None):
    count = len(values) if sample_count is None else sample_count
    total = sum(values) if total_ms is None else total_ms
    return {**stats(values), "samples": count,
            "percentileWindowSamples": len(values), "totalMs": round(total, 6),
            "meanMs": round(total / count, 6) if count else None}


def phase_progress_turn_counts(phase, elapsed_seconds, scheduled, started_offers, completed):
    """Keep due intents, protocol offers, and fully completed turns distinct."""
    denominator = elapsed_seconds if elapsed_seconds and elapsed_seconds > 0 else None
    offer_counts = {name: len(values) for name, values in started_offers.items()}
    return {
        "scheduledIntentsByPhase": dict(scheduled),
        "phaseTurnsStartedOffered": offer_counts,
        "phaseTurnsCompleted": dict(completed),
        "currentPhaseAchievedOfferedTurnsPerSecond": (
            offer_counts[phase] / denominator if denominator else None),
        "currentPhaseAchievedCompletedTurnsPerSecond": (
            completed[phase] / denominator if denominator else None),
    }


def invoke_with_duration(callback, record_duration):
    """Record elapsed callback time in a finally block without changing errors."""
    started = time.monotonic()
    try:
        return callback()
    finally:
        record_duration((time.monotonic() - started) * 1000)


@contextmanager
def measured_runtime_db_context(original_runtime_db, sampled, record=None, *, busy_timeout=None):
    """Optionally time, but always delegate, the original Runtime.db context."""
    options = {} if busy_timeout is None else {"busy_timeout": busy_timeout}
    if not sampled:
        with original_runtime_db(**options) as db:
            yield db
        return

    context = original_runtime_db(**options)
    entry_started = time.perf_counter_ns()
    try:
        db = context.__enter__()
    finally:
        record("contextEntryMs", (time.perf_counter_ns() - entry_started) / 1_000_000)

    body_started = time.perf_counter_ns()
    try:
        yield db
    except BaseException as error:
        record("contextBodyMs", (time.perf_counter_ns() - body_started) / 1_000_000)
        exit_started = time.perf_counter_ns()
        try:
            suppressed = context.__exit__(type(error), error, error.__traceback__)
        finally:
            record("contextExitAndCloseMs", (time.perf_counter_ns() - exit_started) / 1_000_000)
        if not suppressed:
            raise
    else:
        record("contextBodyMs", (time.perf_counter_ns() - body_started) / 1_000_000)
        exit_started = time.perf_counter_ns()
        try:
            context.__exit__(None, None, None)
        finally:
            record("contextExitAndCloseMs", (time.perf_counter_ns() - exit_started) / 1_000_000)


def delta_identity(params):
    """Return the production stream key, excluding benchmark-only metadata."""
    return (params.get("threadId"), params.get("turnId"), params.get("itemId"),
            params.get("delta"))


def phase_due_ns(start_ns, index, turns_per_second):
    """Schedule one offered turn relative to its phase's own start time."""
    return start_ns + int(index * 1_000_000_000 / turns_per_second)


def phase_turn_counts(worker_count, rounds, offered_rate, steady_seconds):
    base_count = worker_count * rounds
    return {
        "warmup": base_count,
        "steady": max(base_count, math.ceil(offered_rate * steady_seconds)),
        "burst": base_count,
        "drain": base_count,
    }


def delivery_receipt(event_id, thread_id, turn_id):
    """Build a native-shaped receipt consumed by Runtime.notification."""
    return {"method": "item/completed", "params": {
        "threadId": thread_id, "turnId": turn_id,
        "item": {"id": "receipt:" + event_id, "type": "userMessage", "clientId": event_id}}}


def register_delta_identity(identities, params, identity):
    identities.setdefault(delta_identity(params), deque()).append(identity)


def take_delta_identity(identities, params):
    key = delta_identity(params)
    pending = identities.get(key)
    if not pending:
        return None, None
    identity = pending.popleft()
    if not pending:
        identities.pop(key, None)
    return identity


def witness_marker(agent_id, phase, round_index):
    return f"witness-{agent_id[:8]}-{phase}-r{round_index}"


def assistant_witness_text(phase, marker, agent_name, round_index):
    """Keep the render witness out of deltas; observe final-item display."""
    stream_text = (
        f"{phase} synthetic answer from {agent_name} round {round_index} "
        + ("streamed answer content " * 100)
    )
    return stream_text, f"{stream_text} {marker}"


def is_sqlite_schema_error(error):
    """Only recognize SQLite's exact SQLITE_SCHEMA result code."""
    return (isinstance(error, sqlite3.Error)
            and getattr(error, "sqlite_errorcode", None) == sqlite3.SQLITE_SCHEMA)


def read_accounting_snapshot(db_factory, sql_identity, reader, retry_records,
                             max_attempts=3, max_elapsed_seconds=1.0):
    """Run read-only benchmark accounting with a narrowly bounded schema retry.

    Each attempt gets a fresh connection and schema snapshot. This helper must
    never wrap Runtime writes or AppServer callbacks.
    """
    started = time.monotonic()
    last_error = None
    for attempt in range(1, max_attempts + 1):
        attempt_started = time.monotonic()
        schema_before = None
        try:
            with db_factory() as db:
                schema_before = db.execute("PRAGMA schema_version").fetchone()[0]
                value = reader(db)
            if attempt > 1:
                record = retry_records[-1]
                total_ms = (time.monotonic() - started) * 1000
                record.update({"schemaVersionAfter": schema_before,
                               "recovered": total_ms <= max_elapsed_seconds * 1000,
                               "recoveryAttempt": attempt,
                               "recoveryDurationMs": (time.monotonic() - attempt_started) * 1000,
                               "totalDurationMs": total_ms,
                               "retryBoundExceeded": total_ms > max_elapsed_seconds * 1000})
                if total_ms > max_elapsed_seconds * 1000:
                    raise RuntimeError("SQLITE_SCHEMA accounting retry exceeded its total time bound") from last_error
            return value
        except sqlite3.Error as error:
            if not is_sqlite_schema_error(error):
                raise
            last_error = error
            try:
                with db_factory() as db:
                    schema_after = db.execute("PRAGMA schema_version").fetchone()[0]
            except sqlite3.Error:
                schema_after = None
            duration_ms = (time.monotonic() - attempt_started) * 1000
            total_ms = (time.monotonic() - started) * 1000
            record = {"attempt": attempt, "sqlIdentity": sql_identity,
                      "errorName": type(error).__name__,
                      "sqliteErrorCode": error.sqlite_errorcode,
                      "schemaVersionBefore": schema_before,
                      "schemaVersionAfterError": schema_after,
                      "schemaVersionAfter": schema_after,
                      "recovered": False, "durationMs": duration_ms,
                      "totalDurationMs": total_ms}
            retry_records.append(record)
            print("SQLITE_SCHEMA_ACCOUNTING_RETRY " + json.dumps(record),
                  file=sys.stderr, flush=True)
            if attempt >= max_attempts or total_ms >= max_elapsed_seconds * 1000:
                raise
            remaining = max_elapsed_seconds - (time.monotonic() - started)
            if remaining <= 0:
                raise
            time.sleep(min(0.01 * attempt, remaining))
    raise last_error


_lock_context = threading.local()


class MeasuredRLock:
    """Measure outer Runtime-lock wait/hold intervals and preserve Condition hooks."""
    SAMPLE_LIMIT = 20_000

    def __init__(self, lock=None):
        self._lock = lock or threading.RLock()
        self._local = threading.local()
        self._samples_lock = threading.Lock()
        self._wait = defaultdict(lambda: deque(maxlen=self.SAMPLE_LIMIT))
        self._held = defaultdict(lambda: deque(maxlen=self.SAMPLE_LIMIT))
        self._wait_latest = defaultdict(float)
        self._held_latest = defaultdict(float)
        self._wait_max = defaultdict(float)
        self._held_max = defaultdict(float)
        self._failed_nonblocking = 0

    @staticmethod
    def _context():
        explicit = getattr(_lock_context, "name", None)
        if explicit:
            return explicit
        name = threading.current_thread().name
        if name.startswith("bench-producer_"):
            return "producer"
        if name.startswith("Thread-") and "process_request_thread" in name:
            return "HTTP request"
        if name == "MainThread":
            return "producer"
        return "other"

    def _record(self, target, context, elapsed):
        with self._samples_lock:
            target[context].append(elapsed)
            latest = self._wait_latest if target is self._wait else self._held_latest
            maximum = self._wait_max if target is self._wait else self._held_max
            latest[context] = elapsed
            maximum[context] = max(maximum[context], elapsed)

    def acquire(self, *args, **kwargs):
        depth = getattr(self._local, "depth", 0)
        started = time.perf_counter_ns()
        acquired = self._lock.acquire(*args, **kwargs)
        blocking = kwargs.get("blocking", args[0] if args else True)
        if not acquired and not blocking:
            self._local.last_nonblocking_failed = True
            with self._samples_lock:
                self._failed_nonblocking += 1
        if acquired:
            if not (kwargs.get("blocking", args[0] if args else True)):
                self._local.last_nonblocking_failed = False
            now = time.perf_counter_ns()
            if depth == 0:
                context = self._context()
                self._record(self._wait, context, (now - started) / 1_000_000)
                self._local.context = context
                self._local.held_started = now
            self._local.depth = depth + 1
        return acquired

    def release(self):
        depth = getattr(self._local, "depth", 0)
        if depth == 1:
            self._record(self._held, self._local.context,
                         (time.perf_counter_ns() - self._local.held_started) / 1_000_000)
            self._local.depth = 0
        elif depth > 1:
            self._local.depth = depth - 1
        self._lock.release()

    def __enter__(self):
        self.acquire()
        return self

    def __exit__(self, *_exc):
        self.release()

    def _is_owned(self):
        return self._lock._is_owned()

    def locked(self):
        return self._lock.locked()

    def _release_save(self):
        depth = getattr(self._local, "depth", 0)
        if depth:
            self._record(self._held, self._local.context,
                         (time.perf_counter_ns() - self._local.held_started) / 1_000_000)
        self._local.depth = 0
        state = self._lock._release_save()
        return state, depth

    def _acquire_restore(self, saved):
        state, depth = saved
        started = time.perf_counter_ns()
        self._lock._acquire_restore(state)
        now = time.perf_counter_ns()
        if depth:
            context = self._context()
            self._record(self._wait, context, (now - started) / 1_000_000)
            self._local.depth = depth
            self._local.context = context
            self._local.held_started = now

    def snapshot(self):
        with self._samples_lock:
            waiting = {key: list(values) for key, values in self._wait.items()}
            held = {key: list(values) for key, values in self._held.items()}
            failed_nonblocking = self._failed_nonblocking
        return {"waitMsByContext": {key: stats(values) for key, values in waiting.items()},
                "heldMsByContext": {key: stats(values) for key, values in held.items()},
                "sampleLimitPerContext": self.SAMPLE_LIMIT,
                "failedNonblockingAcquires": failed_nonblocking}

    def progress_snapshot(self):
        with self._samples_lock:
            return {
                "waitByContext": {key: {"samples": len(values), "latestMs": self._wait_latest[key],
                                        "maxMs": self._wait_max[key]}
                                  for key, values in self._wait.items()},
                "heldByContext": {key: {"samples": len(values), "latestMs": self._held_latest[key],
                                        "maxMs": self._held_max[key]}
                                  for key, values in self._held.items()},
                "failedNonblockingAcquires": self._failed_nonblocking,
            }


class MeasuredLock:
    """Measure non-reentrant start-lock use without changing its semantics."""
    def __init__(self, lock):
        self._lock = lock
        self._stats_lock = threading.Lock()
        self._local = threading.local()
        self._acquires = 0
        self._failed_nonblocking = 0
        self._wait_total_ms = 0.0
        self._wait_max_ms = 0.0
        self._held_total_ms = 0.0
        self._held_max_ms = 0.0

    def acquire(self, *args, **kwargs):
        started = time.perf_counter_ns()
        acquired = self._lock.acquire(*args, **kwargs)
        elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
        blocking = kwargs.get("blocking", args[0] if args else True)
        with self._stats_lock:
            if acquired:
                self._acquires += 1
                self._wait_total_ms += elapsed_ms
                self._wait_max_ms = max(self._wait_max_ms, elapsed_ms)
            elif not blocking:
                self._failed_nonblocking += 1
        if acquired:
            self._local.started_ns = time.perf_counter_ns()
        self._local.last_nonblocking_failed = not acquired and not blocking
        return acquired

    def release(self):
        started = getattr(self._local, "started_ns", None)
        if started is not None:
            held_ms = (time.perf_counter_ns() - started) / 1_000_000
            with self._stats_lock:
                self._held_total_ms += held_ms
                self._held_max_ms = max(self._held_max_ms, held_ms)
            del self._local.started_ns
        return self._lock.release()

    def __enter__(self):
        self.acquire()
        return self

    def __exit__(self, *_exc):
        self.release()

    def locked(self):
        return self._lock.locked()

    def progress_snapshot(self):
        with self._stats_lock:
            return {"acquires": self._acquires,
                    "failedNonblockingAcquires": self._failed_nonblocking,
                    "waitTotalMs": round(self._wait_total_ms, 3),
                    "waitMaxMs": round(self._wait_max_ms, 3),
                    "heldTotalMs": round(self._held_total_ms, 3),
                    "heldMaxMs": round(self._held_max_ms, 3)}


def main():
    from concurrent.futures import ThreadPoolExecutor
    from codex_canvas import Canvas, make_server
    import codex_canvas
    from codex_runtime import AppServer, Runtime

    sqlite_synchronous = diagnostic_synchronous_mode()
    idle_connection_diagnostic = idle_connection_diagnostic_enabled()

    class BenchRuntime(BenchmarkRuntimeMixin, Runtime):
        pass

    evidence = Path(sys.argv[1]).resolve()
    state = evidence / "state"
    profile = evidence / "profile"
    state.mkdir(parents=True, exist_ok=True)
    profile.mkdir(parents=True, exist_ok=True)
    previous = os.environ.get("CODEX_HOME")
    os.environ["CODEX_HOME"] = str(profile)
    try:
        runtime = BenchRuntime(state, server_factory=lambda *_a, **_k: (_ for _ in ()).throw(
            RuntimeError("native transport is forbidden in runtime_load")))
    finally:
        if previous is None:
            os.environ.pop("CODEX_HOME", None)
        else:
            os.environ["CODEX_HOME"] = previous
    runtime.projects({"path": str(ROOT)})
    from codex_project_folders import reorder_sidebar
    reorder_sidebar(runtime, {
        "request_id": "runtime-load-initial-sidebar-order",
        "expected_revision": 0,
        "groups": {},
        "migration": True,
    })
    runtime_lock = MeasuredRLock(runtime.lock)
    runtime.lock = runtime_lock
    start_lock = MeasuredLock(runtime.start_lock)
    runtime.start_lock = start_lock
    snapshot_lock_failures = {"startLock": 0, "runtimeLock": 0, "unattributed": 0}
    # Current Runtime snapshots read committed WAL state without lock deferral.
    runtime.ui_condition = threading.Condition(runtime_lock)
    faulthandler.enable(file=sys.stderr)

    def dump_thread_stacks(_signum, _frame):
        print("BENCH_DIAGNOSTIC_STACK_DUMP signal=SIGUSR1", file=sys.stderr, flush=True)
        faulthandler.dump_traceback(file=sys.stderr, all_threads=True)

    signal.signal(signal.SIGUSR1, dump_thread_stacks)
    canvas = Canvas(root=state)
    canvas.runtime = runtime
    if FRONTEND_DIST:
        codex_canvas.WEB = FRONTEND_DIST
    server = make_server(canvas)
    http_latencies = {}
    http_errors = []
    accounting_schema_retries = []
    http_diag_lock = threading.Lock()
    http_active_routes = {}
    http_request_counts = {}
    http_response_statuses = {}
    production_app = server.app

    async def measured_app(scope, receive, send):
        if scope.get("type") != "http":
            await production_app(scope, receive, send)
            return
        route = scope.get("path", "")
        started = time.monotonic()
        with http_diag_lock:
            http_request_counts[route] = http_request_counts.get(route, 0) + 1
            http_active_routes[route] = http_active_routes.get(route, 0) + 1

        async def measured_send(message):
            if message.get("type") == "http.response.start":
                status = message.get("status", 0)
                with http_diag_lock:
                    key = f"{route} {status}"
                    http_response_statuses[key] = http_response_statuses.get(key, 0) + 1
            await send(message)

        try:
            if route == "/__bench/diagnostics":
                with http_diag_lock:
                    snapshot = {
                        "activeRoutes": dict(http_active_routes),
                        "requestCounts": dict(http_request_counts),
                        "responseStatuses": dict(http_response_statuses),
                    }
                body = json.dumps(snapshot).encode("utf-8")
                await measured_send({
                    "type": "http.response.start",
                    "status": 200,
                    "headers": [(b"content-type", b"application/json"),
                                (b"content-length", str(len(body)).encode("ascii"))],
                })
                await measured_send({"type": "http.response.body", "body": body})
                return
            await production_app(scope, receive, measured_send)
        except BaseException as exc:
            with http_diag_lock:
                http_errors.append({"route": route, "error": f"{type(exc).__name__}: {exc}"})
            raise
        finally:
            with http_diag_lock:
                http_active_routes[route] -= 1
                if http_active_routes[route] == 0:
                    del http_active_routes[route]
                http_latencies.setdefault(route, []).append((time.monotonic() - started) * 1000)

    server.app = measured_app
    server.server.config.app = measured_app
    thread = threading.Thread(target=server.serve_forever, name="runtime-load-http", daemon=True)
    thread.start()
    queue_limit = AppServer.CALLBACK_QUEUE_LIMIT
    offered_rate = int(os.environ.get("BENCH_OFFERED_TURNS_PER_SECOND", "160"))
    quick_check = os.environ.get("BENCH_QUICK_CHECK") == "1"
    producer_pool_size = int(os.environ.get("BENCH_PRODUCER_POOL_SIZE", "1"))
    if not 1 <= producer_pool_size <= 16:
        raise ValueError("BENCH_PRODUCER_POOL_SIZE must be 1..16")
    producer_max_inflight = producer_pool_size
    workers_per_team = 2 if quick_check else int(os.environ.get("BENCH_WORKERS_PER_TEAM", "32"))
    team_count = 1 if quick_check else int(os.environ.get("BENCH_TEAMS", "8"))
    steady_seconds = 0 if quick_check else float(os.environ.get("BENCH_STEADY_SECONDS", "30"))
    lock = threading.Lock()
    dispatched = {}
    receive_to_callback_ms = deque(maxlen=100_000)
    callback_duration_ms = deque(maxlen=100_000)
    callback_wrapper_duration_ms = deque(maxlen=100_000)
    callback_queue_peak = [0]
    category_offered = {}
    category_dispatched = {}
    payload_bytes_by_category = {}
    errors = []
    notification_coverage = {}
    notification_samples = []
    callback_invocations_by_method = {}
    callback_samples = 0
    callback_count = 0
    callback_timing_totals = {"callbacks": 0, "productionSamples": 0,
                              "productionMs": 0.0, "wrapperMs": 0.0,
                              "productionMaxMs": 0.0, "wrapperMaxMs": 0.0}
    callback_timing_by_method = {}
    transaction_sample_every = 32
    transaction_context_counts = {name: 0 for name in ("producer", "AppServer callback", "HTTP request", "other")}
    transaction_sample_local = threading.local()
    transaction_timing_lock = threading.Lock()
    transaction_timing_samples = {
        context: {name: deque(maxlen=512) for name in
                  ("contextEntryMs", "contextBodyMs", "contextExitAndCloseMs")}
        for context in transaction_context_counts
    }
    transaction_timing_totals = {
        context: {name: {"samples": 0, "totalMs": 0.0} for name in metrics}
        for context, metrics in transaction_timing_samples.items()
    }
    analytics_delta_timing_samples = {
        context: deque(maxlen=512) for context in transaction_context_counts
    }
    callback_sample_local = threading.local()
    analytics_sample_local = threading.local()
    def record_transaction_timing(context, metric, elapsed_ms):
        with transaction_timing_lock:
            transaction_timing_samples[context][metric].append(elapsed_ms)
            totals = transaction_timing_totals[context][metric]
            totals["samples"] += 1
            totals["totalMs"] += elapsed_ms

    def transaction_timing_snapshot():
        with transaction_timing_lock:
            copied = {
                context: (transaction_context_counts[context],
                          {name: list(values) for name, values in metrics.items()},
                          {name: dict(values) for name, values in transaction_timing_totals[context].items()},
                          list(analytics_delta_timing_samples[context]))
                for context, metrics in transaction_timing_samples.items()
            }
        return {
            context: {"sampledRuntimeDbContexts": sampled_count,
                      **{name: timing_stats(
                          values, totals[name]["samples"], totals[name]["totalMs"])
                         for name, values in metrics.items()},
                      "deltaAnalyticsMs": stats(analytics_values)}
            for context, (sampled_count, metrics, totals, analytics_values) in copied.items()
        }

    original_runtime_db = runtime.db

    @contextmanager
    def measured_runtime_db(*, busy_timeout=None):
        context = MeasuredRLock._context()
        calls_by_context = getattr(transaction_sample_local, "calls", None)
        if calls_by_context is None:
            calls_by_context = {}
            transaction_sample_local.calls = calls_by_context
        calls_by_context[context] = calls_by_context.get(context, 0) + 1
        sampled = calls_by_context[context] % transaction_sample_every == 0
        if sampled:
            with transaction_timing_lock:
                transaction_context_counts[context] += 1

            def record(metric, elapsed_ms):
                record_transaction_timing(context, metric, elapsed_ms)
        else:
            record = None
        # Sampling changes only recording. Every entry, yielded connection,
        # exception path and exit still belongs to the bound original Runtime.db.
        with measured_runtime_db_context(
            original_runtime_db, sampled, record, busy_timeout=busy_timeout
        ) as db:
            yield db

    runtime.db = measured_runtime_db

    original_analytics_event = runtime.analytics_event

    def measured_analytics_event(*args, **kwargs):
        if not getattr(analytics_sample_local, "enabled", False):
            return original_analytics_event(*args, **kwargs)
        started = time.perf_counter_ns()
        try:
            return original_analytics_event(*args, **kwargs)
        finally:
            context = getattr(analytics_sample_local, "context", "AppServer callback")
            with transaction_timing_lock:
                analytics_delta_timing_samples[context].append(
                    (time.perf_counter_ns() - started) / 1_000_000)

    runtime.analytics_event = measured_analytics_event
    producer_metrics_lock = threading.Lock()
    phase_names = ("warmup", "steady", "burst", "drain")
    turn_offer_lateness_ms = {name: [] for name in phase_names}
    turn_start_offer_ns = {name: [] for name in phase_names}
    turn_completion_lateness_ms = {name: [] for name in phase_names}
    turn_completed_ns = {name: [] for name in phase_names}
    intent_schedule_skew_ms = {name: [] for name in phase_names}
    producer_metrics = {"scheduledIntents": 0, "completedChatWrites": 0,
                        "completedReceiptCallbacks": 0, "acceptedTurns": 0,
                        "inflight": 0, "active": 0, "peakInflight": 0,
                        "peakQueueDepth": 0, "maxQueueAgeMs": 0.0}
    producer_queue_wait_ms = deque(maxlen=100_000)
    producer_queue_age_ms = deque(maxlen=100_000)
    producer_errors = []
    producer_receipt_events = {}
    producer_receipt_lock = threading.Lock()
    producer_previous_turn = {}
    producer_offer_lock = threading.Lock()
    callback_done = threading.Condition()
    progress_lock = threading.Lock()
    progress_phase = ["ready"]
    progress_phase_turns = {name: 0 for name in ("warmup", "steady", "burst", "drain")}
    scheduled_by_phase = {name: 0 for name in progress_phase_turns}
    progress_phase_started = [None]
    progress_started = [None]
    progress_cpu_started = [None]
    progress_stop = threading.Event()
    progress_thread = None
    final_witness_offered_at = {}
    delta_identities = {}
    account_count = max(1, int(os.environ.get("BENCH_ACCOUNT_COUNT", "2")))
    account_keys = [f"bench-{i + 1:02}" for i in range(account_count)]
    seeded_agents_by_thread = {}
    original_runtime_notification = runtime.notification
    def measured_notification(message, account_key="default", connection_id=None):
        nonlocal callback_count, callback_samples
        dispatched_at = message.get("_studioDispatchedAt")
        received_at = message.get("_studioReceivedAt")
        started = time.monotonic()
        method = message.get("method", "")
        previous_lock_context = getattr(_lock_context, "name", None)
        _lock_context.name = "AppServer callback"
        params = message.get("params") if isinstance(message.get("params"), dict) else {}
        thread_id = params.get("threadId") or (params.get("thread") or {}).get("id")
        matched = False
        matched_agent = None
        actual_account_key = None
        if thread_id:
            seeded_agent = seeded_agents_by_thread.get(thread_id)
            matched = seeded_agent is not None
            if seeded_agent:
                matched_agent = seeded_agent["id"]
                actual_account_key = seeded_agent.get("accountKey", "default")
            if not matched:
                errors.append(f"notification thread did not match a Runtime record: {thread_id}")
            elif actual_account_key != account_key:
                errors.append(f"notification account mismatch for {thread_id}: callback={account_key}, record={actual_account_key}")
        sample_state = len(notification_samples) < 20
        events_before = runtime.agent(matched_agent).get("events") if matched_agent and sample_state else None
        if method == "item/agentMessage/delta":
            callback_sample_local.delta_count = getattr(callback_sample_local, "delta_count", 0) + 1
        previous_analytics_sample = getattr(analytics_sample_local, "enabled", False)
        previous_analytics_context = getattr(analytics_sample_local, "context", None)
        analytics_sample_local.enabled = (
            method == "item/agentMessage/delta"
            and callback_sample_local.delta_count % transaction_sample_every == 0)
        analytics_sample_local.context = _lock_context.name
        production_duration_ms = None
        production_ended = None
        def record_production_duration(duration_ms):
            nonlocal production_duration_ms, production_ended
            production_duration_ms = duration_ms
            production_ended = time.monotonic()
        try:
            # Include failed production callbacks in the timing denominator;
            # callback errors remain separately visible.
            invoke_with_duration(
                lambda: original_runtime_notification(message, account_key, connection_id),
                record_production_duration,
            )
            samples = message.get("_studioNotificationSamples") or [params]
            for sample in samples:
                if method == "item/agentMessage/delta":
                    identity = delta_identity(sample)
                    key, kind = take_delta_identity(delta_identities, sample)
                    if key is None:
                        errors.append(f"delta callback identity missing: {identity[:3]}")
                        continue
                else:
                    key = sample.get("_benchEventId") or message.get("_benchEventId")
                    kind = sample.get("_benchCategory") or message.get("_benchCategory")
                if key and kind:
                    with lock:
                        if key in dispatched:
                            errors.append(f"duplicate callback identity: {key}")
                        dispatched[key] = time.monotonic_ns()
                        category_dispatched[kind] = category_dispatched.get(kind, 0) + 1
                    if kind == "durableMessage":
                        with producer_receipt_lock:
                            receipt_event = producer_receipt_events.get(key)
                        if receipt_event is not None:
                            receipt_event.set()
                        with producer_metrics_lock:
                            producer_metrics["completedReceiptCallbacks"] += 1
        except BaseException as exc:
            errors.append(f"{message.get('method')}: {type(exc).__name__}: {exc}")
            raise
        finally:
            if previous_lock_context is None:
                try:
                    del _lock_context.name
                except AttributeError:
                    pass
            else:
                _lock_context.name = previous_lock_context
            analytics_sample_local.enabled = previous_analytics_sample
            if previous_analytics_context is None:
                try:
                    del analytics_sample_local.context
                except AttributeError:
                    pass
            else:
                analytics_sample_local.context = previous_analytics_context
            ended = time.monotonic()
            if type(received_at) in (int, float) and type(dispatched_at) in (int, float):
                receive_to_callback_ms.append(max(0, dispatched_at - received_at) * 1000)
            callback_wrapper_duration_ms.append((ended - started) * 1000)
            production_ms = production_duration_ms
            callback_duration_ms.append(production_ms)
            coverage = notification_coverage.setdefault(method, {"callbacks": 0, "matchedThread": 0})
            coverage["callbacks"] += 1
            callback_invocations_by_method[method] = callback_invocations_by_method.get(method, 0) + 1
            coverage["matchedThread"] += int(matched)
            coverage["matchedAccount"] = coverage.get("matchedAccount", 0) + int(matched and actual_account_key == account_key)
            if len(notification_samples) < 20:
                notification_samples.append({"method": method, "threadId": thread_id,
                    "turnId": params.get("turnId"), "turn": params.get("turn"),
                    "eventsBefore": events_before,
                    "eventsAfter": runtime.agent(matched_agent).get("events") if matched_agent and sample_state else None,
                    "callbackAccount": account_key, "recordAccount": actual_account_key})
            with callback_done:
                callback_count += 1
                callback_samples += len(message.get("_studioNotificationSamples") or [params])
                wrapper_ms = (ended - started) * 1000
                callback_timing_totals["wrapperMs"] += wrapper_ms
                callback_timing_totals["callbacks"] += 1
                callback_timing_totals["wrapperMaxMs"] = max(
                    callback_timing_totals["wrapperMaxMs"], wrapper_ms)
                if production_ended is not None:
                    callback_timing_totals["productionSamples"] += 1
                    callback_timing_totals["productionMs"] += production_ms
                    callback_timing_totals["productionMaxMs"] = max(
                        callback_timing_totals["productionMaxMs"], production_ms)
                method_key = method if method in callback_timing_by_method or len(callback_timing_by_method) < 32 else "other"
                method_timing = callback_timing_by_method.setdefault(
                    method_key, {"count": 0, "productionMs": 0.0, "wrapperMs": 0.0,
                                 "productionMaxMs": 0.0, "wrapperMaxMs": 0.0})
                method_timing["count"] += 1
                method_timing["wrapperMs"] += wrapper_ms
                method_timing["wrapperMaxMs"] = max(method_timing["wrapperMaxMs"], wrapper_ms)
                if production_ended is not None:
                    method_timing["productionSamples"] = method_timing.get("productionSamples", 0) + 1
                    method_timing["productionMs"] += production_ms
                    method_timing["productionMaxMs"] = max(
                        method_timing["productionMaxMs"], production_ms)
                callback_done.notify_all()

    runtime.notification = measured_notification
    appservers = []
    fake_processes = []
    FakeAppProcess.instances.clear()
    with patch("codex_runtime.subprocess.Popen", FakeAppProcess):
        for account_key in account_keys:
            (state / "app-servers" / account_key).mkdir(parents=True, exist_ok=True)
            appserver = AppServer(
                state / "app-servers" / account_key,
                lambda message, key=account_key: runtime.notification(message, key, None),
                lambda _message: None,
                lambda: None,
                home=profile / account_key,
            )
            appservers.append(appserver)
    fake_processes = list(FakeAppProcess.instances)
    cleanup_finished = False
    idle_connection = [None]
    def cleanup_fixture():
        nonlocal cleanup_finished
        if cleanup_finished:
            return
        cleanup_finished = True
        for appserver in appservers:
            appserver.close()
        for appserver in appservers:
            appserver.join_callbacks(5)
        runtime.close()
        server.shutdown()
        server.server_close()
        thread.join(5)
        if idle_connection[0] is not None:
            if idle_connection[0].in_transaction:
                raise RuntimeError("idle SQLite diagnostic unexpectedly owns a transaction at teardown")
            idle_connection[0].close()
            idle_connection[0] = None
    atexit.register(cleanup_fixture)
    # Create one production Runtime record as a shape template, then seed the
    # other synthetic identities with Runtime.put. This is fixture setup, not
    # agent spawning: no scheduler/native session is involved.
    template = runtime.create({"name": "Load lead 01", "cwd": str(ROOT),
                               "prompt": "Synthetic active-turn benchmark identity"}, defer=True)
    template["accountKey"] = account_keys[0]
    leads = []
    workers = []

    def seeded(template_record, name, *, parent=None, team=0, index=0):
        record = copy.deepcopy(template_record)
        record.update(id=str(uuid.uuid4()), name=name, threadId=str(uuid.uuid4()),
                      parentId=parent["id"] if parent else None,
                      rootId=parent["id"] if parent else "",
                      isLead=parent is None, role="orchestrator" if parent is None else "reviewer",
                      autoWake=True, yoloMode=True, status="running", inFlight=True,
                      turnId=f"bench-turn-{team}-{index}", syntheticBenchmarkTurn=True)
        record["accountKey"] = account_keys[team % len(account_keys)]
        if parent is None:
            record["rootId"] = record["id"]
        return record

    with runtime.lock, runtime.db() as db:
        template.update(threadId=str(uuid.uuid4()), autoWake=True, yoloMode=True,
                        status="running", inFlight=True, turnId="bench-turn-lead-0",
                        syntheticBenchmarkTurn=True, accountKey=account_keys[0])
        runtime.put(db, "agents", template)
        leads.append(template)
        for team in range(1, team_count):
            lead = seeded(template, f"Load lead {team + 1:02}", team=team)
            runtime.put(db, "agents", lead)
            leads.append(lead)
        for team, lead in enumerate(leads):
            for index in range(workers_per_team):
                agent = seeded(template, f"Load {team + 1:02}/{index + 1:02}",
                               parent=lead, team=team, index=index)
                runtime.put(db, "agents", agent)
                workers.append(agent)
    seeded_agents_by_thread.update((record["threadId"], record) for record in [template, *leads, *workers])
    if idle_connection_diagnostic:
        idle_connection[0] = open_idle_connection(runtime.db_path)

    print(json.dumps({"kind": "ready", "origin": f"http://127.0.0.1:{server.server_port}",
                      "sourceRevision": os.environ.get("BENCH_SOURCE_REVISION", "unknown"),
                      "sqliteSynchronousMode": sqlite_synchronous,
                      "durabilityProfile": ("production-default FULL" if sqlite_synchronous == "FULL"
                                            else "DIAGNOSTIC ONLY: NORMAL weakens durability; not acceptance evidence"),
                      "databaseFilesystemType": database_filesystem_type(state),
                      "databasePathFieldRecorded": False,
                      "idleSQLiteConnectionDiagnostic": {
                          "enabled": idle_connection_diagnostic,
                          "openedAfterSeedBeforeWorkload": idle_connection[0] is not None,
                          "preWorkloadPrimingRead": "sqlite_master",
                          "queriesDuringWorkload": False,
                          "transactionDuringWorkload": False},
                      "teams": team_count,
                      "workersPerTeam": workers_per_team,
                      "witnesses": [{"agentId": team[0]["id"]}
                                    for team in [workers[i:i + workers_per_team] for i in range(0, len(workers), workers_per_team)]]}), flush=True)

    def wait_callbacks(target, timeout=120):
        deadline = time.monotonic() + timeout
        with callback_done:
            while callback_samples < target:
                if errors:
                    raise RuntimeError(f"callback failed after {callback_samples}/{target} samples: {errors[0]}")
                left = deadline - time.monotonic()
                if left <= 0:
                    raise TimeoutError(f"AppServer callback samples stalled at {callback_samples}/{target}; invocations={callback_count}; identities={len(dispatched)}/{len(emitted_keys)}; queues={[(s.callbacks.qsize(), s.callbacks.unfinished_tasks, s.reader_done.is_set(), s.dispatch_stopped) for s in appservers]}")
                callback_done.wait(min(left, .25))
        if errors:
            raise RuntimeError(errors[0])

    def emit_progress(reason):
        with progress_lock:
            phase = progress_phase[0]
            phase_turns = dict(progress_phase_turns)
            offered = dict(category_offered)
        with producer_metrics_lock:
            scheduled_phases = dict(scheduled_by_phase)
        phase_elapsed = ((time.monotonic() - progress_phase_started[0])
                         if progress_phase_started[0] and phase in progress_phase_turns else None)
        turn_progress = phase_progress_turn_counts(
            phase, phase_elapsed, scheduled_phases,
            {name: turn_start_offer_ns[name] for name in phase_names}, phase_turns)
        with callback_done:
            callbacks = {"samples": callback_samples, "invocations": callback_count}
            callback_timings = dict(callback_timing_totals)
            callback_timings["meanProductionMs"] = (
                callback_timings["productionMs"] / callback_timings["productionSamples"]
                if callback_timings["productionSamples"] else 0)
            callback_timings["meanWrapperMs"] = (
                callback_timings["wrapperMs"] / callback_timings["callbacks"]
                if callback_timings["callbacks"] else 0)
            callback_methods = {
                name: {**values,
                       "meanProductionMs": (
                           values["productionMs"] / values["productionSamples"]
                           if values.get("productionSamples") else 0),
                       "meanWrapperMs": values["wrapperMs"] / values["count"]}
                for name, values in callback_timing_by_method.items()}
        with lock:
            dispatched_count = len(dispatched)
            dispatched_categories = dict(category_dispatched)
        with producer_metrics_lock:
            producer_state = dict(producer_metrics)
            producer_state["queueDepth"] = max(0, producer_state["inflight"] - producer_state["active"])
            producer_state["poolSize"] = producer_pool_size
            producer_state["maxInflight"] = producer_max_inflight
            producer_state["queueWaitSamples"] = len(producer_queue_wait_ms)
            producer_state["queueWaitMaxMs"] = max(producer_queue_wait_ms, default=0.0)
            producer_state["queueAgeSamples"] = len(producer_queue_age_ms)
        elapsed = (time.monotonic() - progress_started[0]) if progress_started[0] else 0
        cpu_elapsed = ((time.process_time() - progress_cpu_started[0])
                       if progress_cpu_started[0] is not None else 0)
        snapshot = {
            "kind": "progress", "reason": reason, "elapsedSeconds": round(elapsed, 3),
            "processCpuSeconds": round(cpu_elapsed, 6),
            "processCpuToElapsedRatio": cpu_elapsed / elapsed if elapsed > 0 else None,
            "phase": phase,
            **turn_progress,
            "producer": producer_state,
            "currentPhaseElapsedSeconds": round(phase_elapsed, 3) if phase_elapsed is not None else None,
            "offeredByCategory": offered, "completedCallbackSamples": callbacks["samples"],
            "callbackInvocations": callbacks["invocations"],
            "callbackTimingTotalsMs": callback_timings,
            "callbackTimingByMethodMs": callback_methods,
            "runtimeDatabaseTiming": {
                "samplingEveryRuntimeDbContextsPerContext": transaction_sample_every,
                "deltaAnalyticsSampling": "every 32nd delta callback per dispatcher thread",
                "scope": "Runtime.db only; all contexts delegate to the bound original; sampled entry, yielded body, and combined original exit/close",
                "byContext": transaction_timing_snapshot(),
            },
            "dispatchedIdentities": dispatched_count,
            "dispatchedByCategory": dispatched_categories,
            "callbackQueues": [{"transport": index, "depth": appserver.callbacks.qsize(),
                                 "unfinished": appserver.callbacks.unfinished_tasks}
                                for index, appserver in enumerate(appservers)],
            "runtimeLock": runtime_lock.progress_snapshot(),
            "runtimeLockDistribution": runtime_lock.snapshot(),
            "startLock": start_lock.progress_snapshot(),
            "snapshotDeferredByLock": dict(snapshot_lock_failures),
        }
        print(json.dumps(snapshot, separators=(",", ":")), flush=True)

    def periodic_progress():
        while not progress_stop.wait(5):
            emit_progress("interval")

    def acknowledge_chat_event(event_id, recipient):
        operation = {"agent": recipient["id"], "epoch": recipient["epoch"],
                     "accountKey": recipient["accountKey"], "connectionId": None,
                     "threadId": recipient["threadId"], "turnId": recipient["turnId"]}
        with runtime.lock, runtime.db() as db:
            meta_row = db.execute("SELECT record FROM runtime_event_meta WHERE id=?", (event_id,)).fetchone()
            if meta_row is None:
                db.execute("INSERT INTO runtime_event_meta VALUES (?,?)", (event_id, "{}"))
                meta = {}
            else:
                meta = json.loads(meta_row[0])
            meta["native"] = operation
            db.execute("UPDATE runtime_event_meta SET record=? WHERE id=?", (json.dumps(meta), event_id))
            db.execute("UPDATE runtime_events SET status='dispatching',turn_id=? WHERE id=? AND status='pending'",
                       (recipient["turnId"], event_id))
        return delivery_receipt(event_id, recipient["threadId"], recipient["turnId"])

    for line in sys.stdin:
        command = json.loads(line)
        if command.get("action") == "run":
            rounds = int(command.get("rounds", 1))
            if not 1 <= rounds <= 8:
                raise ValueError("rounds must be in 1..8")
            progress_started[0] = time.monotonic()
            progress_cpu_started[0] = time.process_time()
            progress_phase[0] = "starting"
            progress_stop.clear()
            progress_thread = threading.Thread(target=periodic_progress,
                                               name="runtime-load-progress", daemon=True)
            progress_thread.start()
            emit_progress("run-start")
            total_started = time.monotonic_ns()
            event_rows_before = read_accounting_snapshot(
                runtime.db, "runtime_events.count.before",
                lambda db: db.execute("SELECT COUNT(*) FROM runtime_events").fetchone()[0],
                accounting_schema_retries)
            identities = []
            for agent in workers:
                team = workers.index(agent) // workers_per_team
                identities.append((agent, leads[team]))
            emitted_keys = set()
            expected_runtime_event_ids = []
            turn_counts = phase_turn_counts(len(workers), rounds, offered_rate, steady_seconds)
            witness_markers = defaultdict(list)
            steady_witness_markers = defaultdict(list)
            burst_drain_ms = None
            current_phase = [None]
            burst_last_offer_ns = [None]

            def offer(kind, key, process, payload, turn_start_due_ns=None):
                with producer_offer_lock:
                    if key in emitted_keys:
                        raise RuntimeError(f"duplicate offered identity: {key}")
                    emitted_keys.add(key)
                    payload_bytes_by_category[kind] = payload_bytes_by_category.get(kind, 0) + len(json.dumps(payload).encode("utf-8"))
                with progress_lock:
                    category_offered[kind] = category_offered.get(kind, 0) + 1
                if kind == "assistantDelta":
                    register_delta_identity(delta_identities, payload["params"], (key, kind))
                else:
                    payload["_benchEventId"] = key
                    payload["_benchCategory"] = kind
                if kind != "assistantDelta" and isinstance(payload.get("params"), dict):
                    payload["params"]["_benchEventId"] = key
                    payload["params"]["_benchCategory"] = kind
                if turn_start_due_ns is not None:
                    offered_at_ns = time.monotonic_ns()
                    turn_start_offer_ns[current_phase[0]].append(offered_at_ns)
                    turn_offer_lateness_ms[current_phase[0]].append(
                        (offered_at_ns - turn_start_due_ns) / 1_000_000)
                process.emit(payload)
                if current_phase[0] == "burst":
                    burst_last_offer_ns[0] = time.monotonic_ns()
                for appserver in appservers:
                    callback_queue_peak[0] = max(callback_queue_peak[0], appserver.callbacks.qsize())

            producer_pool = (ThreadPoolExecutor(max_workers=producer_pool_size,
                                                thread_name_prefix="bench-producer")
                             if producer_pool_size > 1 else None)
            producer_slots = threading.BoundedSemaphore(producer_max_inflight)
            active_producer_futures = []
            def accept_completed_turn(phase, due_ns):
                accepted_ns = time.monotonic_ns()
                turn_completed_ns[phase].append(accepted_ns)
                turn_completion_lateness_ms[phase].append((accepted_ns - due_ns) / 1_000_000)
                with progress_lock:
                    progress_phase_turns[phase] += 1
                with producer_metrics_lock:
                    producer_metrics["acceptedTurns"] += 1

            def write_turn_messages(phase, local_round, agent, lead, worker_index, team_start):
                completion_events = []
                team_index = worker_index % workers_per_team
                peer = workers[team_start + (team_index + 1) % workers_per_team]
                process = fake_processes[(team_start // workers_per_team) % len(fake_processes)]
                for recipient, label in ((peer, "peer"), (lead, "lead")):
                    msg_id = str(uuid.uuid5(uuid.NAMESPACE_URL,
                                            f"{phase}:{local_round}:{agent['id']}:{label}"))
                    runtime.chat_message(agent["id"], recipient["id"],
                                         f"{phase} coordination {label} {msg_id}", msg_id)
                    with producer_metrics_lock:
                        producer_metrics["completedChatWrites"] += 1
                    event_id = "chat:" + msg_id + ":" + recipient["id"]
                    expected_runtime_event_ids.append(event_id)
                    receipt = acknowledge_chat_event(event_id, recipient)
                    receipt_key = "msg:" + msg_id
                    receipt_event = threading.Event()
                    with producer_receipt_lock:
                        producer_receipt_events[receipt_key] = receipt_event
                    offer("durableMessage", receipt_key, process, receipt)
                    completion_events.append((receipt_key, receipt_event))
                for receipt_key, receipt_event in completion_events:
                    receipt_deadline = time.monotonic() + 45
                    while not receipt_event.wait(.1):
                        if errors:
                            raise RuntimeError(f"receipt callback failed: {errors[0]}")
                        if time.monotonic() >= receipt_deadline:
                            raise TimeoutError(f"production receipt callback did not complete: {receipt_key}")
                    with producer_receipt_lock:
                        producer_receipt_events.pop(receipt_key, None)

            def complete_turn_work(phase, local_round, agent, lead, worker_index, team_start, due_ns,
                                   queued_at_ns=None):
                with producer_metrics_lock:
                    producer_metrics["active"] += 1
                    if queued_at_ns is not None:
                        queue_age = (time.monotonic_ns() - queued_at_ns) / 1_000_000
                        producer_queue_age_ms.append(queue_age)
                        producer_metrics["maxQueueAgeMs"] = max(
                            producer_metrics["maxQueueAgeMs"], queue_age)
                try:
                    write_turn_messages(phase, local_round, agent, lead, worker_index, team_start)
                    accept_completed_turn(phase, due_ns)
                except BaseException as exc:
                    with producer_metrics_lock:
                        producer_errors.append(f"{phase}/{agent['id']}: {type(exc).__name__}: {exc}")
                    raise
                finally:
                    with producer_metrics_lock:
                        producer_metrics["active"] -= 1
                        producer_metrics["inflight"] -= 1
                    producer_slots.release()

            def drain_producer_futures():
                for future in active_producer_futures:
                    future.result()
                active_producer_futures.clear()
                if producer_errors:
                    raise RuntimeError(producer_errors[0])

            phase_elapsed = {}
            phase_effective_rate = {}
            phase_completed_rate = {}
            phase_offer_elapsed_seconds = {}
            for phase, scale in (("warmup", 2), ("steady", 1), ("burst", 4), ("drain", 1)):
                current_phase[0] = phase
                with progress_lock:
                    progress_phase[0] = phase
                    progress_phase_started[0] = time.monotonic()
                emit_progress("phase-start")
                phase_count = turn_counts[phase]
                phase_started = time.monotonic_ns() + 100_000_000
                for i in range(phase_count):
                    agent, lead = identities[i % len(identities)]
                    local_round = i // len(workers)
                    due = phase_due_ns(phase_started, i, offered_rate * scale)
                    intent_schedule_ns = time.monotonic_ns()
                    intent_schedule_skew_ms[phase].append(
                        (intent_schedule_ns - due) / 1_000_000)
                    with producer_metrics_lock:
                        producer_metrics["scheduledIntents"] += 1
                        scheduled_by_phase[phase] += 1
                    previous_future = producer_previous_turn.get(agent["id"])
                    if previous_future is not None:
                        previous_future.result()
                    if producer_errors:
                        raise RuntimeError(producer_errors[0])
                    left = (due - time.monotonic_ns()) / 1e9
                    if left > 0:
                        time.sleep(left)
                    queue_wait_started = time.perf_counter_ns()
                    producer_slots.acquire()
                    queue_wait = (time.perf_counter_ns() - queue_wait_started) / 1_000_000
                    producer_queue_wait_ms.append(queue_wait)
                    with producer_metrics_lock:
                        producer_metrics["inflight"] += 1
                        producer_metrics["peakInflight"] = max(
                            producer_metrics["peakInflight"], producer_metrics["inflight"])
                        queued_depth = max(0, producer_metrics["inflight"] - producer_metrics["active"])
                        producer_metrics["peakQueueDepth"] = max(
                            producer_metrics["peakQueueDepth"], queued_depth)
                    turn_id = f"bench-turn-{phase}-{local_round}-{agent['id']}"
                    item_id = f"bench-item-{phase}-{local_round}-{agent['id']}"
                    marker = witness_marker(agent["id"], phase, local_round)
                    stream_text, final_text = assistant_witness_text(
                        phase, marker, agent["name"], local_round
                    )
                    witness_markers[agent["id"]].append(marker)
                    if phase == "steady":
                        steady_witness_markers[agent["id"]].append(marker)
                    team_number = i % len(workers) // workers_per_team
                    process = fake_processes[team_number % len(fake_processes)]
                    sequence = [
                        ("turnLifecycle", "turn/started", {"turn": {"id": turn_id}}),
                        ("toolStatus", "item/started", {"turnId": turn_id, "item": {"id": item_id + "-tool", "type": "commandExecution", "command": "[synthetic tool]"}}),
                        ("toolOutput", "item/completed", {"turnId": turn_id, "item": {"id": item_id + "-tool", "type": "commandExecution", "aggregatedOutput": "synthetic tool output\n" * 64, "exitCode": 0}}),
                        ("assistantFinal", "item/completed", {"threadId": agent["threadId"], "turnId": turn_id, "item": {"id": item_id, "type": "agentMessage", "text": final_text, "phase": "final_answer"}}),
                        ("turnLifecycle", "turn/completed", {"threadId": agent["threadId"], "turnId": turn_id, "turn": {"id": turn_id, "status": "completed"}}),
                    ]
                    for kind, method, params in sequence:
                        if method == "item/completed" and params.get("item", {}).get("type") == "agentMessage":
                            for fragment in range(8):
                                fragment_text = stream_text[fragment * 256:(fragment + 1) * 256]
                                delta_key = f"evt:{phase}:{local_round}:{agent['id']}:delta:{fragment}"
                                offer("assistantDelta", delta_key, process, {"method": "item/agentMessage/delta", "params": {"threadId": agent["threadId"], "turnId": turn_id, "itemId": item_id, "delta": fragment_text}})
                            final_witness_offered_at[marker] = time.time() * 1000
                        key = f"evt:{phase}:{local_round}:{agent['id']}:{method}:{params.get('itemId') or params.get('item', {}).get('id') or (params.get('run') or {}).get('id', '')}"
                        offer(kind, key, process,
                              {"method": method, "params": {"threadId": agent["threadId"], **params}},
                              due if method == "turn/started" else None)
                    team_start = (i % len(workers) // workers_per_team) * workers_per_team
                    producer_queued_at_ns = time.monotonic_ns()
                    if producer_pool is None:
                        complete_turn_work(phase, local_round, agent, lead, i % len(workers), team_start,
                                           due, producer_queued_at_ns)
                    else:
                        future = producer_pool.submit(
                            complete_turn_work, phase, local_round, agent, lead, i % len(workers),
                            team_start, due, producer_queued_at_ns)
                        producer_previous_turn[agent["id"]] = future
                        active_producer_futures = [f for f in active_producer_futures if not f.done()]
                        active_producer_futures.append(future)
                        if len(active_producer_futures) > producer_max_inflight:
                            raise RuntimeError("bounded producer in-flight limit was exceeded")
                drain_producer_futures()
                phase_elapsed[phase] = (time.monotonic_ns() - phase_started) / 1e9
                if turn_start_offer_ns[phase]:
                    phase_offer_elapsed_seconds[phase] = max(
                        0.001, (turn_start_offer_ns[phase][-1] - phase_started) / 1e9)
                    phase_effective_rate[phase] = (
                        len(turn_start_offer_ns[phase]) / phase_offer_elapsed_seconds[phase])
                else:
                    phase_offer_elapsed_seconds[phase] = None
                    phase_effective_rate[phase] = None
                phase_completed_rate[phase] = (
                    len(turn_completed_ns[phase]) / phase_elapsed[phase]
                    if turn_completed_ns[phase] and phase_elapsed[phase] > 0 else None)
                emit_progress("phase-complete")
                if phase == "burst":
                    wait_callbacks(sum(category_offered.values()), 30)
                    for appserver in appservers:
                        appserver.callbacks.join()
                    burst_drain_ms = (
                        time.monotonic_ns() - burst_last_offer_ns[0]
                    ) / 1_000_000
            if producer_pool is not None:
                producer_pool.shutdown(wait=True)

            # Wait until all turn-complete callbacks have persisted child_result
            # events; only then enumerate their exact original IDs for receipts.
            wait_callbacks(sum(category_offered.values()), 30 if quick_check else 150)
            for appserver in appservers:
                appserver.callbacks.join()
            if delta_identities:
                raise RuntimeError(f"unconsumed streamed fragment identities: {sum(map(len, delta_identities.values()))}")

            # Consume child-result inbox events on their original lead IDs using
            # the production userMessage receipt callback after worker finishes.
            child_events = read_accounting_snapshot(
                runtime.db, "runtime_events.child_result.pending",
                lambda db: [dict(row) for row in db.execute(
                    "SELECT id,agent FROM runtime_events WHERE kind='child_result' AND status='pending' ORDER BY created,id")],
                accounting_schema_retries)
            for event in child_events:
                expected_runtime_event_ids.append(event["id"])
                recipient = runtime.agent(event["agent"])
                receipt = acknowledge_chat_event(event["id"], recipient)
                lead_index = next((index for index, lead in enumerate(leads) if lead["id"] == recipient["id"]), 0)
                process = fake_processes[lead_index % len(fake_processes)]
                offer("childResultReceipt", "child-receipt:" + event["id"], process, receipt)
            wait_callbacks(sum(category_offered.values()), 30 if quick_check else 150)
            for appserver in appservers:
                appserver.callbacks.join()
            chat_rows = 0
            event_rows = 0
            event_kinds = {}
            analytics_rows = 0
            analytics_payload_bytes = 0
            transcript_counts = {k: 0 for k in ("assistant", "tool", "output")}
            def collect_accounting(db):
                chat_rows = db.execute("SELECT COUNT(*) FROM runtime_chat_messages").fetchone()[0]
                event_rows = db.execute("SELECT COUNT(*) FROM runtime_events").fetchone()[0]
                event_kinds = dict(db.execute(
                    "SELECT kind,COUNT(*) FROM runtime_events GROUP BY kind"
                ).fetchall())
                analytics = db.execute("SELECT COUNT(*),COALESCE(SUM(bytes),0) FROM analytics_notifications").fetchone()
                consumed_runtime_events = db.execute("SELECT COUNT(*) FROM runtime_events WHERE status='delivered'").fetchone()[0]
                pending_runtime_events = db.execute("SELECT COUNT(*) FROM runtime_events WHERE status IN ('pending','dispatching','reserved')").fetchone()[0]
                delivered_ids = {row[0] for row in db.execute(
                    "SELECT id FROM runtime_events WHERE status='delivered'")}
                return (chat_rows, event_rows, event_kinds, analytics,
                        consumed_runtime_events, pending_runtime_events, delivered_ids)

            (chat_rows, event_rows, event_kinds, analytics,
             consumed_runtime_events, pending_runtime_events, delivered_ids) = read_accounting_snapshot(
                runtime.db, "runtime_load.final.read_only_accounting_snapshot",
                collect_accounting, accounting_schema_retries)
            analytics_rows, analytics_payload_bytes = analytics[0], analytics[1]
            expected_delivered_ids = set(expected_runtime_event_ids)
            if len(expected_delivered_ids) != len(expected_runtime_event_ids):
                raise RuntimeError("a Runtime inbox event ID was acknowledged more than once")
            missing_deliveries = sorted(expected_delivered_ids - delivered_ids)
            if missing_deliveries:
                raise RuntimeError(f"original Runtime event IDs were not acknowledged: {missing_deliveries[:5]}")
            if set(dispatched) != emitted_keys:
                raise RuntimeError(f"AppServer callback identity mismatch: missing={sorted(emitted_keys-set(dispatched))[:5]}, unexpected={sorted(set(dispatched)-emitted_keys)[:5]}")
            for agent in workers:
                for item in runtime.transcript(agent["id"])["items"]:
                    transcript_counts[item.get("role", "unknown")] = transcript_counts.get(item.get("role", "unknown"), 0) + 1
            usage = resource.getrusage(resource.RUSAGE_SELF)
            progress_stop.set()
            if progress_thread:
                progress_thread.join(timeout=1)
            progress_phase[0] = "complete"
            emit_progress("run-complete")
            report_elapsed = time.monotonic_ns() / 1e9 - total_started / 1e9
            report_cpu = time.process_time() - progress_cpu_started[0]
            report = {"teams": team_count, "workersPerTeam": workers_per_team,
                      "syntheticActiveTurns": len(workers) + len(leads), "rounds": rounds,
                      "sqliteSynchronousMode": sqlite_synchronous,
                      "durabilityProfile": ("production-default FULL" if sqlite_synchronous == "FULL"
                                            else "DIAGNOSTIC ONLY: NORMAL weakens durability; not acceptance evidence"),
                      "databaseFilesystemType": database_filesystem_type(state),
                      "databasePathFieldRecorded": False,
                      "idleSQLiteConnectionDiagnostic": {
                          "enabled": idle_connection_diagnostic,
                          "openedAfterSeedBeforeWorkload": idle_connection[0] is not None,
                          "preWorkloadPrimingRead": "sqlite_master",
                          "queriesDuringWorkload": False,
                          "transactionDuringWorkload": False},
                      "steadySeconds": steady_seconds,
                      "phaseTurnCounts": turn_counts,
                      "burstDrainMs": burst_drain_ms,
                      "readOnlyAccountingSchemaRetries": {
                          "count": len(accounting_schema_retries),
                          "totalDurationMs": sum(max(
                              record.get("totalDurationMs", record["durationMs"])
                              for record in accounting_schema_retries
                              if record["sqlIdentity"] == sql_identity)
                              for sql_identity in {record["sqlIdentity"] for record in accounting_schema_retries}),
                          "attempts": accounting_schema_retries},
                      "syntheticAccounts": {"keys": account_keys,
                                            "teamAccountKeys": [account_keys[index % len(account_keys)] for index in range(team_count)]},
                      "phases": ["warmup", "steady", "burst", "drain"],
                      "producerWork": {
                          "poolSize": producer_pool_size,
                          "maxInflight": producer_max_inflight,
                          "queueCapacity": producer_max_inflight,
                          "scheduledIntentsByPhase": scheduled_by_phase,
                          "acceptedTurnsByPhase": progress_phase_turns,
                          "scheduledIntents": producer_metrics["scheduledIntents"],
                          "acceptedTurns": producer_metrics["acceptedTurns"],
                          "completedChatWrites": producer_metrics["completedChatWrites"],
                          "completedReceiptCallbacks": producer_metrics["completedReceiptCallbacks"],
                          "unacceptedIntentBacklog": (producer_metrics["scheduledIntents"]
                                                       - producer_metrics["acceptedTurns"]),
                          "peakInflight": producer_metrics["peakInflight"],
                          "peakQueueDepth": producer_metrics["peakQueueDepth"],
                          "maxQueueAgeMs": producer_metrics["maxQueueAgeMs"],
                          "queueWaitMs": stats(producer_queue_wait_ms),
                          "queueAgeMs": stats(producer_queue_age_ms),
                          "errors": producer_errors,
                      },
                      "offered": category_offered, "dispatched": category_dispatched,
                      "payloadBytesByCategory": payload_bytes_by_category,
                      "exactlyOnce": {"offeredEvents": sum(category_offered.values()),
                                      "completedDispatches": sum(category_dispatched.values()),
                                      "uniqueDispatchedIdentities": len(dispatched),
                                      "durableChatMessages": chat_rows,
                                      "queuedRuntimeEventsBefore": event_rows_before,
                                      "queuedRuntimeEventsAfter": event_rows,
                                      "queuedRuntimeEventsAdded": event_rows - event_rows_before,
                                      "queuedRuntimeEventsByKind": event_kinds,
                                      "consumedRuntimeEvents": consumed_runtime_events,
                                      "pendingRuntimeEvents": pending_runtime_events,
                                      "originalRuntimeEventIdsExpected": len(expected_runtime_event_ids),
                                      "originalRuntimeEventIdsAcknowledged": len(expected_delivered_ids),
                                      "originalRuntimeEventIdsSha256": hashlib.sha256("\n".join(sorted(expected_runtime_event_ids)).encode()).hexdigest(),
                                      "offeredCallbackIdentitiesSha256": hashlib.sha256("\n".join(sorted(emitted_keys)).encode()).hexdigest(),
                                      "dispatchedCallbackIdentitiesSha256": hashlib.sha256("\n".join(sorted(dispatched)).encode()).hexdigest()},
                      "queue": {"kind": "production AppServer.callbacks FIFO per fake transport",
                                "capacityPerTransport": queue_limit,
                                "transports": len(appservers),
                                "peakSampledDepth": callback_queue_peak[0],
                                "drained": all(appserver.callbacks.unfinished_tasks == 0 for appserver in appservers),
                                "depthAtDrain": sum(appserver.callbacks.qsize() for appserver in appservers)},
                      "latencyMs": {"receiveToCallback": stats(receive_to_callback_ms),
                                    "callbackDuration": stats(callback_duration_ms),
                                    "callbackWrapperDuration": stats(callback_wrapper_duration_ms),
                                    "runtimeLock": runtime_lock.snapshot()},
                      "snapshotDeferredByLock": dict(snapshot_lock_failures),
                      "startLock": start_lock.progress_snapshot(),
                      "offeredRateTurnsPerSecond": offered_rate,
                      "phaseElapsedSeconds": phase_elapsed,
                      "phaseActualNotificationOfferElapsedSeconds": phase_offer_elapsed_seconds,
                      "phaseActualNotificationOfferCounts": {phase: len(values) for phase, values in turn_start_offer_ns.items()},
                      "phaseScheduledIntentSkewMs": {phase: stats(values) for phase, values in intent_schedule_skew_ms.items()},
                      "phaseActualNotificationOfferLatenessMs": {phase: stats(values) for phase, values in turn_offer_lateness_ms.items()},
                      "phaseCompletedTurnLatenessMs": {phase: stats(values) for phase, values in turn_completion_lateness_ms.items()},
                      "phaseAchievedOfferedTurnsPerSecond": phase_effective_rate,
                      "phaseAchievedCompletedTurnsPerSecond": phase_completed_rate,
                      "witnessMarkersByAgent": dict(witness_markers),
                      "steadyWitnessMarkersByAgent": dict(steady_witness_markers),
                      "finalWitnessOfferedAtEpochMs": final_witness_offered_at,
                      "httpServerLatencyMs": {route: stats(values) for route, values in http_latencies.items()},
                      "httpServerErrors": http_errors,
                      "callbackInvocationCount": callback_count,
                      "callbackEventSampleCount": callback_samples,
                      "notificationCoverage": notification_coverage,
                      "callbackTimingByMethodMs": callback_timing_by_method,
                      "runtimeDatabaseTiming": {
                          "samplingEveryRuntimeDbContextsPerContext": transaction_sample_every,
                          "deltaAnalyticsSampling": "every 32nd delta callback per dispatcher thread",
                          "scope": "Runtime.db only; all contexts delegate to the bound original; sampled entry, yielded body, and combined original exit/close",
                          "byContext": transaction_timing_snapshot(),
                      },
                      "streamCoalescing": {
                          "offeredAssistantFragments": category_offered.get("assistantDelta", 0),
                          "productionCallbackInvocations": callback_invocations_by_method.get("item/agentMessage/delta", 0),
                          "fragmentsPerCallback": (
                              category_offered.get("assistantDelta", 0)
                              / callback_invocations_by_method["item/agentMessage/delta"]
                              if callback_invocations_by_method.get("item/agentMessage/delta") else None),
                      },
                      "notificationSamples": notification_samples,
                      "transcriptItemsByRole": transcript_counts,
                      "workerStates": [{"id": agent["id"], "threadId": agent["threadId"], "accountKey": agent.get("accountKey", "default"),
                                       "status": runtime.agent(agent["id"]).get("status"),
                                       "turnId": runtime.agent(agent["id"]).get("turnId"),
                                       "events": runtime.agent(agent["id"]).get("events")}
                                      for agent in workers[:8]],
                      "analytics": {"notificationRows": analytics_rows,
                                    "payloadBytes": analytics_payload_bytes},
                      "cpuProcessSeconds": time.process_time(),
                      "cpuProcessSecondsSinceRunStart": report_cpu,
                      "cpuToElapsedRatioSinceRunStart": report_cpu / report_elapsed if report_elapsed > 0 else None,
                      "peakRss": {"value": usage.ru_maxrss,
                                  "unit": "KiB" if sys.platform != "darwin" else "bytes",
                                  "source": "resource.getrusage(RUSAGE_SELF).ru_maxrss"},
                      "elapsedSeconds": report_elapsed}
            print(json.dumps({"kind": "result", "report": report}), flush=True)
        elif command.get("action") == "shutdown":
            break
    cleanup_fixture()
    if thread.is_alive():
        raise RuntimeError("HTTP server failed to stop")


if __name__ == "__main__":
    main()

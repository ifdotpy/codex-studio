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


ROOT = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts"))
FRONTEND_DIST = Path(sys.argv[3]).resolve() if len(sys.argv) > 3 and sys.argv[3] else None


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
    SAMPLE_LIMIT = 100_000

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

    @staticmethod
    def _context():
        return getattr(_lock_context, "name", None) or threading.current_thread().name

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
        if acquired:
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
        return {"waitMsByContext": {key: stats(values) for key, values in waiting.items()},
                "heldMsByContext": {key: stats(values) for key, values in held.items()},
                "sampleLimitPerContext": self.SAMPLE_LIMIT}

    def progress_snapshot(self):
        with self._samples_lock:
            return {
                "waitByContext": {key: {"samples": len(values), "latestMs": self._wait_latest[key],
                                        "maxMs": self._wait_max[key]}
                                  for key, values in self._wait.items()},
                "heldByContext": {key: {"samples": len(values), "latestMs": self._held_latest[key],
                                        "maxMs": self._held_max[key]}
                                  for key, values in self._held.items()},
            }


def main():
    from codex_canvas import Canvas, make_server
    import codex_canvas
    from codex_runtime import AppServer, Runtime

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
    runtime_lock = MeasuredRLock(runtime.lock)
    runtime.lock = runtime_lock
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
    handler = server.RequestHandlerClass
    original_do_get = handler.do_GET
    original_send_response = handler.send_response

    def measured_send_response(self, code, message=None):
        from urllib.parse import urlsplit
        route = urlsplit(self.path).path
        with http_diag_lock:
            key = f"{route} {code}"
            http_response_statuses[key] = http_response_statuses.get(key, 0) + 1
        return original_send_response(self, code, message)

    def measured_do_get(self):
        from urllib.parse import urlsplit
        route = urlsplit(self.path).path
        started = time.monotonic()
        with http_diag_lock:
            http_request_counts[route] = http_request_counts.get(route, 0) + 1
            http_active_routes[route] = http_active_routes.get(route, 0) + 1
        try:
            if route == "/__bench/diagnostics":
                with http_diag_lock:
                    snapshot = {
                        "activeRoutes": dict(http_active_routes),
                        "requestCounts": dict(http_request_counts),
                        "responseStatuses": dict(http_response_statuses),
                    }
                body = json.dumps(snapshot).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            return original_do_get(self)
        except BaseException as exc:
            with lock:
                http_errors.append({"route": route, "error": f"{type(exc).__name__}: {exc}"})
            raise
        finally:
            with http_diag_lock:
                http_active_routes[route] -= 1
                if http_active_routes[route] == 0:
                    del http_active_routes[route]
            with lock:
                http_latencies.setdefault(route, []).append((time.monotonic() - started) * 1000)
    handler.do_GET = measured_do_get
    handler.send_response = measured_send_response
    thread = threading.Thread(target=server.serve_forever, name="runtime-load-http", daemon=True)
    thread.start()
    queue_limit = AppServer.CALLBACK_QUEUE_LIMIT
    offered_rate = int(os.environ.get("BENCH_OFFERED_TURNS_PER_SECOND", "160"))
    quick_check = os.environ.get("BENCH_QUICK_CHECK") == "1"
    workers_per_team = 2 if quick_check else int(os.environ.get("BENCH_WORKERS_PER_TEAM", "32"))
    team_count = 1 if quick_check else int(os.environ.get("BENCH_TEAMS", "8"))
    steady_seconds = 0 if quick_check else float(os.environ.get("BENCH_STEADY_SECONDS", "30"))
    lock = threading.Lock()
    dispatched = {}
    receive_to_callback_ms = []
    callback_duration_ms = []
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
    callback_done = threading.Condition()
    progress_lock = threading.Lock()
    progress_phase = ["ready"]
    progress_phase_turns = {name: 0 for name in ("warmup", "steady", "burst", "drain")}
    progress_phase_started = [None]
    progress_started = [None]
    progress_stop = threading.Event()
    progress_thread = None
    final_witness_offered_at = {}
    delta_identities = {}
    account_count = max(1, int(os.environ.get("BENCH_ACCOUNT_COUNT", "2")))
    account_keys = [f"bench-{i + 1:02}" for i in range(account_count)]
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
            with runtime.db() as db:
                row = db.execute("SELECT record FROM runtime_agents WHERE json_extract(record,'$.threadId')=? LIMIT 1",
                                 (thread_id,)).fetchone()
                matched = row is not None
                if row:
                    matched_record = json.loads(row[0])
                    matched_agent = matched_record.get("id")
                    actual_account_key = matched_record.get("accountKey", "default")
            if not matched:
                errors.append(f"notification thread did not match a Runtime record: {thread_id}")
            elif actual_account_key != account_key:
                errors.append(f"notification account mismatch for {thread_id}: callback={account_key}, record={actual_account_key}")
        events_before = runtime.agent(matched_agent).get("events") if matched_agent else None
        try:
            original_runtime_notification(message, account_key, connection_id)
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
            ended = time.monotonic()
            if type(received_at) in (int, float) and type(dispatched_at) in (int, float):
                receive_to_callback_ms.append(max(0, dispatched_at - received_at) * 1000)
            callback_duration_ms.append((ended - started) * 1000)
            coverage = notification_coverage.setdefault(method, {"callbacks": 0, "matchedThread": 0})
            coverage["callbacks"] += 1
            callback_invocations_by_method[method] = callback_invocations_by_method.get(method, 0) + 1
            coverage["matchedThread"] += int(matched)
            coverage["matchedAccount"] = coverage.get("matchedAccount", 0) + int(matched and actual_account_key == account_key)
            if len(notification_samples) < 20:
                notification_samples.append({"method": method, "threadId": thread_id,
                    "turnId": params.get("turnId"), "turn": params.get("turn"),
                    "eventsBefore": events_before,
                    "eventsAfter": runtime.agent(matched_agent).get("events") if matched_agent else None,
                    "callbackAccount": account_key, "recordAccount": actual_account_key})
            with callback_done:
                callback_count += 1
                callback_samples += len(message.get("_studioNotificationSamples") or [params])
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

    print(json.dumps({"kind": "ready", "origin": f"http://127.0.0.1:{server.server_port}",
                      "sourceRevision": os.environ.get("BENCH_SOURCE_REVISION", "unknown"),
                      "state": str(state), "teams": team_count,
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
        with callback_done:
            callbacks = {"samples": callback_samples, "invocations": callback_count}
        with lock:
            dispatched_count = len(dispatched)
            dispatched_categories = dict(category_dispatched)
        elapsed = (time.monotonic() - progress_started[0]) if progress_started[0] else 0
        phase_elapsed = ((time.monotonic() - progress_phase_started[0])
                         if progress_phase_started[0] and phase in progress_phase_turns else None)
        snapshot = {
            "kind": "progress", "reason": reason, "elapsedSeconds": round(elapsed, 3),
            "phase": phase, "phaseTurnsOffered": phase_turns,
            "currentPhaseElapsedSeconds": round(phase_elapsed, 3) if phase_elapsed is not None else None,
            "currentPhaseAchievedOfferedTurnsPerSecond": (
                round(phase_turns[phase] / phase_elapsed, 3)
                if phase_elapsed and phase_elapsed > 0 else None),
            "offeredByCategory": offered, "completedCallbackSamples": callbacks["samples"],
            "callbackInvocations": callbacks["invocations"],
            "dispatchedIdentities": dispatched_count,
            "dispatchedByCategory": dispatched_categories,
            "callbackQueues": [{"transport": index, "depth": appserver.callbacks.qsize(),
                                 "unfinished": appserver.callbacks.unfinished_tasks}
                                for index, appserver in enumerate(appservers)],
            "runtimeLock": runtime_lock.progress_snapshot(),
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
            turn_lateness_ms = {name: [] for name in ("warmup", "steady", "burst", "drain")}
            turn_offered_ns = {name: [] for name in ("warmup", "steady", "burst", "drain")}
            turn_counts = phase_turn_counts(len(workers), rounds, offered_rate, steady_seconds)
            witness_markers = defaultdict(list)
            steady_witness_markers = defaultdict(list)
            burst_drain_ms = None
            current_phase = [None]
            burst_last_offer_ns = [None]

            def offer(kind, key, process, payload):
                if key in emitted_keys:
                    raise RuntimeError(f"duplicate offered identity: {key}")
                emitted_keys.add(key)
                with progress_lock:
                    category_offered[kind] = category_offered.get(kind, 0) + 1
                payload_bytes_by_category[kind] = payload_bytes_by_category.get(kind, 0) + len(json.dumps(payload).encode("utf-8"))
                if kind == "assistantDelta":
                    register_delta_identity(delta_identities, payload["params"], (key, kind))
                else:
                    payload["_benchEventId"] = key
                    payload["_benchCategory"] = kind
                if kind != "assistantDelta" and isinstance(payload.get("params"), dict):
                    payload["params"]["_benchEventId"] = key
                    payload["params"]["_benchCategory"] = kind
                process.emit(payload)
                if current_phase[0] == "burst":
                    burst_last_offer_ns[0] = time.monotonic_ns()
                for appserver in appservers:
                    callback_queue_peak[0] = max(callback_queue_peak[0], appserver.callbacks.qsize())

            phase_elapsed = {}
            phase_effective_rate = {}
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
                    left = (due - time.monotonic_ns()) / 1e9
                    if left > 0:
                        time.sleep(left)
                    actual_offer_ns = time.monotonic_ns()
                    turn_offered_ns[phase].append(actual_offer_ns)
                    turn_lateness_ms[phase].append((actual_offer_ns - due) / 1_000_000)
                    with progress_lock:
                        progress_phase_turns[phase] += 1
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
                        ("hookLifecycle", "hook/started", {"turnId": turn_id, "run": {"id": item_id + "-hook", "eventName": "AfterAgentTurn", "status": "running", "entries": [{"kind": "context", "text": "synthetic hook context"}]}}),
                        ("toolStatus", "item/started", {"turnId": turn_id, "item": {"id": item_id + "-tool", "type": "commandExecution", "command": "[synthetic tool]"}}),
                        ("toolOutput", "item/completed", {"turnId": turn_id, "item": {"id": item_id + "-tool", "type": "commandExecution", "aggregatedOutput": "synthetic tool output\n" * 64, "exitCode": 0}}),
                        ("assistantFinal", "item/completed", {"threadId": agent["threadId"], "turnId": turn_id, "item": {"id": item_id, "type": "agentMessage", "text": final_text, "phase": "final_answer"}}),
                        ("hookLifecycle", "hook/completed", {"turnId": turn_id, "run": {"id": item_id + "-hook", "eventName": "AfterAgentTurn", "status": "completed", "entries": [{"kind": "context", "text": "synthetic hook context"}, {"kind": "warning", "text": "synthetic hook warning"}]}}),
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
                        offer(kind, key, process, {"method": method, "params": {"threadId": agent["threadId"], **params}})
                    # Durable event APIs write worker→worker and worker→lead messages.
                    team_start = (i % len(workers) // workers_per_team) * workers_per_team
                    team_index = i % workers_per_team
                    peer = workers[team_start + (team_index + 1) % workers_per_team]
                    for recipient, label in ((peer, "peer"), (lead, "lead")):
                        msg_id = str(uuid.uuid5(uuid.NAMESPACE_URL, f"{phase}:{local_round}:{agent['id']}:{label}"))
                        chat = runtime.chat_message(agent["id"], recipient["id"],
                            f"{phase} coordination {label} {msg_id}", msg_id)
                        event_id = "chat:" + msg_id + ":" + recipient["id"]
                        expected_runtime_event_ids.append(event_id)
                        account_process = fake_processes[(team_start // workers_per_team) % len(fake_processes)]
                        receipt = acknowledge_chat_event(event_id, recipient)
                        offer("durableMessage", f"msg:{msg_id}", account_process, receipt)
                phase_elapsed[phase] = (time.monotonic_ns() - phase_started) / 1e9
                offered_times = turn_offered_ns[phase]
                phase_effective_rate[phase] = (
                    len(offered_times) / (phase_elapsed[phase])
                    if offered_times and phase_elapsed[phase] > 0
                    else None)
                emit_progress("phase-complete")
                if phase == "burst":
                    wait_callbacks(sum(category_offered.values()), 30)
                    for appserver in appservers:
                        appserver.callbacks.join()
                    burst_drain_ms = (
                        time.monotonic_ns() - burst_last_offer_ns[0]
                    ) / 1_000_000

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
            report = {"teams": team_count, "workersPerTeam": workers_per_team,
                      "syntheticActiveTurns": len(workers) + len(leads), "rounds": rounds,
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
                                    "runtimeLock": runtime_lock.snapshot()},
                      "offeredRateTurnsPerSecond": offered_rate,
                      "phaseElapsedSeconds": phase_elapsed,
                      "phaseOfferedTurnLatenessMs": {phase: stats(values) for phase, values in turn_lateness_ms.items()},
                      "phaseAchievedOfferedTurnsPerSecond": phase_effective_rate,
                      "witnessMarkersByAgent": dict(witness_markers),
                      "steadyWitnessMarkersByAgent": dict(steady_witness_markers),
                      "finalWitnessOfferedAtEpochMs": final_witness_offered_at,
                      "httpServerLatencyMs": {route: stats(values) for route, values in http_latencies.items()},
                      "httpServerErrors": http_errors,
                      "callbackInvocationCount": callback_count,
                      "callbackEventSampleCount": callback_samples,
                      "notificationCoverage": notification_coverage,
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
                      "peakRss": {"value": usage.ru_maxrss,
                                  "unit": "KiB" if sys.platform != "darwin" else "bytes",
                                  "source": "resource.getrusage(RUSAGE_SELF).ru_maxrss"},
                      "elapsedSeconds": time.monotonic_ns() / 1e9 - total_started / 1e9}
            print(json.dumps({"kind": "result", "report": report}), flush=True)
        elif command.get("action") == "shutdown":
            break
    cleanup_fixture()
    if thread.is_alive():
        raise RuntimeError("HTTP server failed to stop")


if __name__ == "__main__":
    main()

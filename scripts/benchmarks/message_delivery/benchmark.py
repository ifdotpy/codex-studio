#!/usr/bin/env python3
"""Synthetic end-to-end notification to local transcript pull benchmark."""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import math
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

SCRIPTS = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SCRIPTS))

DEFAULT_AGENT_COUNTS = (1, 8, 32)
DEFAULT_MESSAGES_PER_AGENT = 8
DEFAULT_REPETITIONS = 1
DEFAULT_RATE_HZ = 80.0
MAX_MESSAGES_PER_AGENT = 100  # The production transcript page is 120 items.
MAX_CASE_REPETITIONS = 100
MAX_TOTAL_CASES = 24
MAX_OFFERED_RATE_HZ = 10000.0
MAX_OFFER_WINDOW_SECONDS = 45.0
CASE_TIMEOUT_SECONDS = 60.0
CLEANUP_TIMEOUT_SECONDS = 5.0
EVENT_TIMEOUT_SECONDS = 15.0
SOCKET_TIMEOUT_SECONDS = 20.0
QUEUE_CAPACITY = 4096
SYNTHETIC_HISTORY_RECORDS = 1024
SUPPORTED_AGENT_COUNTS = frozenset((1, 8, 32))


def percentile(values: list[float], percent: float) -> float | None:
    """Nearest-rank percentile (1-based), retaining observed samples."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(percent * len(ordered)))
    return ordered[rank - 1]


def summary(values: list[float]) -> dict:
    return {"p50": percentile(values, .50), "p95": percentile(values, .95),
            "p99": percentile(values, .99), "max": max(values) if values else None}


def import_overlap(write_times: list[int], execution_intervals: list[tuple[int, int]],
                   offer_interval: tuple[int, int]) -> dict:
    """Prefer observed writes during notifications, then the full delivery window."""
    for name, intervals in (("notification_execution", execution_intervals),
                            ("fixed_offer_to_final_client_receipt", [offer_interval])):
        matching = [stamp for stamp in write_times
                    if any(start <= stamp <= end for start, end in intervals)]
        if matching:
            break
    return {"overlapWindow": name, "overlapIntervalsNs": intervals,
            "overlapWriteTimestampsNs": matching, "overlappedEventWindow": bool(matching)}


def time_left(deadline: float) -> float:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("message delivery case exceeded its 60-second deadline")
    return remaining


def wait_queue_drained(work: queue.Queue, deadline: float) -> None:
    # Queue.join has no timeout. Its standard task condition accepts our case deadline.
    with work.all_tasks_done:
        while work.unfinished_tasks:
            work.all_tasks_done.wait(time_left(deadline))


class TrackedWorkQueue(queue.Queue):
    def __init__(self, maxsize: int):
        self.peak = 0
        super().__init__(maxsize=maxsize)

    def _put(self, item):
        super()._put(item)
        if item is not None:
            self.peak = max(self.peak, len(self.queue))


def runtime_class():
    from codex_runtime import Runtime

    class FixtureRuntime(Runtime):
        def schedule(self):
            # Keep native dispatch and unrelated scheduler work disabled while
            # still forwarding committed protocol-3 resource invalidations.
            while not self.closed:
                self.changed.wait(0.1)
                self.changed.clear()
                if not self.closed:
                    self._publish_committed_resource_changes()

    return FixtureRuntime


def rss_peak() -> dict:
    try:
        import resource
        value = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        if sys.platform == "darwin":
            return {"value": value, "unit": "bytes", "source": "getrusage.ru_maxrss"}
        return {"value": value, "unit": "KiB", "source": "getrusage.ru_maxrss"}
    except (ImportError, AttributeError, OSError):
        return {"value": None, "unit": None, "source": "unavailable"}


class SyncClient(threading.Thread):
    """One protocol-3 invalidation stream and concurrent transcript pulls."""
    def __init__(self, base: str, agents: list[dict], expected: dict[str, str], receipt: dict,
                 error_queue: queue.Queue, stop: threading.Event, ready: threading.Event,
                 receipt_changed: threading.Event, deadline: float):
        super().__init__(name="benchmark-sync-client", daemon=False)
        self.base, self.agents, self.expected, self.receipt = base, agents, expected, receipt
        self.error_queue, self.stop, self.ready = error_queue, stop, ready
        self.receipt_changed, self.deadline = receipt_changed, deadline
        self.checkpoints = {agent["id"]: 0 for agent in agents}
        self.agents_by_id = {agent["id"]: agent for agent in agents}
        self.transport = "sync"
        self.resources = json.dumps(
            [{"kind": "transcript", "agentId": agent["id"]} for agent in agents],
            separators=(",", ":"),
        )

    def pull(self, agent: dict) -> None:
        if self.transport == "sync":
            query = urllib.parse.urlencode({"scope": "transcript:" + agent["id"],
                                            "after": self.checkpoints[agent["id"]]})
            url = self.base + "/api/sync/pull?" + query
            with urllib.request.urlopen(url,
                                        timeout=min(SOCKET_TIMEOUT_SECONDS, time_left(self.deadline))) as response:
                result = json.load(response)
            self.checkpoints[agent["id"]] = result["checkpoint"]["seq"]
            documents = result["documents"]
            for document in documents:
                if document.get("id") != "transcript:" + agent["id"]:
                    raise RuntimeError("sync pull returned an unexpected transcript identity")
                payload = json.loads(document["payload"])
                self.observe_items(payload.get("items", []))
        else:
            query = urllib.parse.urlencode({"id": agent["id"]})
            url = self.base + "/api/transcript?" + query
            with urllib.request.urlopen(url,
                                        timeout=min(SOCKET_TIMEOUT_SECONDS, time_left(self.deadline))) as response:
                result = json.load(response)
            self.observe_items(result.get("items", []))

    def observe_items(self, items: list[dict]) -> None:
        observed = time.monotonic_ns()
        for item in items:
            identity = item.get("id")
            if identity not in self.expected:
                continue
            text = item.get("text")
            if text != self.expected[identity]:
                raise RuntimeError(f"corrupt final text for {identity}: {text!r}")
            if identity not in self.receipt:
                self.receipt[identity] = observed
                self.receipt_changed.set()

    def pulls(self, agent_ids: list[str] | None = None):
        agents = [self.agents_by_id[agent_id] for agent_id in agent_ids] if agent_ids else self.agents
        with ThreadPoolExecutor(max_workers=min(32, len(agents))) as pool:
            list(pool.map(self.pull, agents))

    def run(self):
        with urllib.request.urlopen(
            self.base + "/api/sync/protocol",
            timeout=min(SOCKET_TIMEOUT_SECONDS, time_left(self.deadline)),
        ) as protocol_response:
            api_schema = protocol_response.headers.get("X-Studio-API-Schema")
        parameters = {"protocol": 3, "resources": self.resources}
        if api_schema:
            parameters["apiSchema"] = api_schema
        query = urllib.parse.urlencode(parameters)
        request = urllib.request.Request(self.base + "/api/sync/stream?" + query,
                                         headers={"Accept": "text/event-stream"})
        try:
            with urllib.request.urlopen(request, timeout=min(SOCKET_TIMEOUT_SECONDS, time_left(self.deadline))) as response:
                if response.status != 200:
                    raise RuntimeError(f"sync stream returned HTTP {response.status}")
                event_name = "message"
                data_lines = []
                initialized = False
                while not self.stop.is_set():
                    raw = response.readline()
                    if not raw:
                        break
                    line = raw.decode("utf-8").rstrip("\r\n")
                    if line.startswith("event:"):
                        event_name = line[6:].strip()
                    elif line.startswith("data:"):
                        data_lines.append(line[5:].lstrip())
                    elif not line and data_lines:
                        payload = json.loads("\n".join(data_lines))
                        current_event = event_name
                        event_name, data_lines = "message", []
                        if current_event != "resources":
                            continue
                        agent_ids = [
                            resource["agentId"]
                            for resource in payload.get("resources", [])
                            if resource.get("kind") == "transcript"
                            and resource.get("agentId") in self.agents_by_id
                        ]
                        if not agent_ids:
                            continue
                        self.pulls(agent_ids)
                        if not initialized:
                            initialized = True
                            self.ready.set()
        except (OSError, urllib.error.URLError, ValueError, RuntimeError) as error:
            if not self.stop.is_set():
                self.error_queue.put(f"sync client: {error}")
                self.receipt_changed.set()
        finally:
            self.ready.set()


def _agent(runtime, root: Path, index: int) -> dict:
    agent = runtime.create({"name": f"Synthetic agent {index}", "cwd": str(root),
                            "prompt": "Synthetic benchmark fixture", "yolo_mode": True}, defer=True)
    thread_id = str(uuid.uuid4())
    agent.update(threadId=thread_id, turnId="bench-turn", autoWake=False, status="paused")
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", agent)
    return agent


def _synthetic_rollout(home: Path, agent: dict, lines: int = SYNTHETIC_HISTORY_RECORDS) -> None:
    session_dir = home / "sessions" / "benchmark" / "message_delivery" / "fixture"
    session_dir.mkdir(parents=True, exist_ok=True)
    path = session_dir / f"synthetic-{agent['threadId']}.jsonl"
    rows = [
        {"type": "session_meta", "payload": {"id": agent["threadId"]}},
        {"type": "turn_context", "payload": {"turn_id": "bench-import-turn", "model": "synthetic-benchmark"}},
    ]
    rows.extend({"type": "token_usage_record", "payload": {
        "thread_id": agent["threadId"], "turn_id": "bench-import-turn",
        "response_id": f"synthetic-response-{index}",
        "usage": {"input_tokens": 9, "output_tokens": index + 1, "total_tokens": index + 10},
        "thread_token_usage": {"input_tokens": 9, "output_tokens": index + 1, "total_tokens": index + 10},
    }} for index in range(lines))
    with path.open("w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row, separators=(",", ":")) + "\n")


def _import_history(runtime, agent_id: str, work_started: threading.Event, gate: threading.Event,
                    report: dict, stop: threading.Event, deadline: float) -> None:
    try:
        if not gate.wait(min(EVENT_TIMEOUT_SECONDS, time_left(deadline))):
            report["error"] = "analytics import start gate timed out"
            return
        report["writeTimestampsNs"] = []
        previous_usage_rows = 0
        for steps in range(10000):
            if stop.is_set():
                raise RuntimeError("analytics import stopped after case failure")
            time_left(deadline)
            if "startedNs" not in report:
                report["startedNs"] = time.monotonic_ns()
            runtime.analytics_history_step(max_records=128)
            with runtime.db() as db:
                row = db.execute("SELECT record FROM analytics_history WHERE agent=?", (agent_id,)).fetchone()
                usage_rows = db.execute("SELECT COUNT(*) FROM analytics_usage WHERE agent=?", (agent_id,)).fetchone()[0]
            if usage_rows > previous_usage_rows:
                report["writeTimestampsNs"].append(time.monotonic_ns())
                previous_usage_rows = usage_rows
            if "readyNs" not in report:
                report["readyNs"] = time.monotonic_ns()
                work_started.set()
            state = json.loads(row[0]) if row else None
            if state and state.get("status") == "current" and state.get("offset") == state.get("fileBytes"):
                report["state"] = state
                report["steps"] = steps + 1
                report["analyticsRowsWritten"] = usage_rows
                report["completedNs"] = time.monotonic_ns()
                return
        report["error"] = "analytics import exceeded bounded step count"
    except Exception as error:  # surfaced by scenario runner
        report["error"] = f"{type(error).__name__}: {error}"
        work_started.set()
    report["completedNs"] = time.monotonic_ns()


def run_case(agent_count: int, messages_per_agent: int, rate_hz: float,
             with_analytics: bool, repetition: int = 1, inject: str | None = None,
             transport: str = "sync", temporary_parent: str | None = None) -> dict:
    deadline = time.monotonic() + CASE_TIMEOUT_SECONDS
    if os.environ.get("CODEX_BENCH_NATIVE_TRANSPORT"):
        raise RuntimeError("native transport is forbidden by this benchmark")
    if (agent_count not in SUPPORTED_AGENT_COUNTS or messages_per_agent < 1
            or messages_per_agent > MAX_MESSAGES_PER_AGENT
            or not math.isfinite(rate_hz) or not 0 < rate_hz <= MAX_OFFERED_RATE_HZ):
        raise ValueError("agents must be 1, 8, or 32; messages per agent and rate must be within supported bounds")
    expected_count = agent_count * messages_per_agent
    if expected_count / rate_hz > MAX_OFFER_WINDOW_SECONDS:
        raise ValueError(f"offered workload must fit within {MAX_OFFER_WINDOW_SECONDS:g} seconds")
    messages = {}
    scheduled_times = {}
    dispatch_times = {}
    notification_completed_times = {}
    enqueue_times = {}
    producer_lateness = {}
    receipts = {}
    errors = queue.Queue()
    receipt_changed = threading.Event()
    stop = threading.Event()
    gate = threading.Event()
    import_work_started = threading.Event()
    import_report = {}
    event_start_ns = 0
    original_home = os.environ.get("CODEX_HOME")
    drained = [0]
    work = TrackedWorkQueue(maxsize=QUEUE_CAPACITY)
    now = time.monotonic_ns
    process_started = time.process_time()

    with tempfile.TemporaryDirectory(prefix="studio-message-bench-", dir=temporary_parent) as temporary:
        root = Path(temporary) / "state"
        home = Path(temporary) / "profile"
        root.mkdir()
        home.mkdir()
        os.environ["CODEX_HOME"] = str(home)
        try:
            from codex_canvas import Canvas, make_server
            def forbidden_transport(*_args, **_kwargs):
                raise RuntimeError("benchmark fixture attempted native transport")

            runtime = runtime_class()(root, server_factory=forbidden_transport)
        finally:
            if original_home is None:
                os.environ.pop("CODEX_HOME", None)
            else:
                os.environ["CODEX_HOME"] = original_home
        if hasattr(runtime, "analytics_history_thread"):
            runtime.close()
            raise RuntimeError("unexpected automatic analytics importer in benchmark runtime")
        canvas = Canvas(root=root)
        canvas.runtime = runtime
        server = make_server(canvas)
        server_thread = threading.Thread(target=server.serve_forever, name="benchmark-http", daemon=False)
        server_thread.start()
        clients = []
        workers = []
        importer = None
        try:
            agents = []
            for index in range(agent_count):
                time_left(deadline)
                agents.append(_agent(runtime, root, index))
            base = f"http://127.0.0.1:{server.server_port}"
            expected = {f"{agent['id']}:bench-item-{i}": f"synthetic message {i} for {agent['id']}"
                        for agent in agents for i in range(messages_per_agent)}
            ready = threading.Event()
            if transport in ("sync", "transcript"):
                client = SyncClient(base, agents, expected, receipts, errors, stop, ready,
                                    receipt_changed, deadline)
                client.transport = transport
                client.start()
                clients.append(client)
            else:
                raise ValueError("transport must be sync or transcript")
            if not ready.wait(min(EVENT_TIMEOUT_SECONDS, time_left(deadline))):
                raise RuntimeError(f"{transport} client did not establish its subscription")
            if not errors.empty():
                raise RuntimeError(errors.get())
            if with_analytics:
                _synthetic_rollout(home, agents[0])
                importer = threading.Thread(target=_import_history,
                    args=(runtime, agents[0]["id"], import_work_started, gate, import_report, stop, deadline),
                    name="benchmark-analytics-import", daemon=False)
                importer.start()

            if inject == "stall_notification":
                lock_held = threading.Event()
                def hold_runtime_lock():
                    with runtime.lock:
                        lock_held.set()
                        threading.Event().wait()
                threading.Thread(target=hold_runtime_lock, name="benchmark-test-lock-holder", daemon=True).start()
                if not lock_held.wait(time_left(deadline)):
                    raise TimeoutError("test lock holder did not acquire runtime lock")

            # One producer offers at an absolute fixed rate and records queue depth.
            def produce():
                for seq in range(expected_count):
                    if stop.is_set():
                        return
                    due = event_start_ns + int(seq * 1_000_000_000 / rate_hz)
                    time_left(deadline)
                    delay = (due - now()) / 1_000_000_000
                    if delay > 0:
                        stop.wait(min(delay, time_left(deadline)))
                        time_left(deadline)
                    if stop.is_set():
                        return
                    agent_index = seq % agent_count
                    local_index = seq // agent_count
                    agent = agents[agent_index]
                    item_id = f"bench-item-{local_index}"
                    identity = f"{agent['id']}:{item_id}"
                    content = f"synthetic message {local_index} for {agent['id']}"
                    if inject == "missing" and seq == expected_count - 1:
                        continue
                    if inject == "corrupt" and seq == expected_count - 1:
                        content += " corrupted"
                    messages[identity] = (agent, item_id, content)
                    scheduled_times[identity] = due
                    enqueued = now()
                    enqueue_times[identity] = enqueued
                    producer_lateness[identity] = max(0, enqueued - due)
                    work.put((identity, agent, item_id, content, enqueued), timeout=min(EVENT_TIMEOUT_SECONDS, time_left(deadline)))

            def dispatch_worker():
                while True:
                    record = work.get()
                    try:
                        if record is None:
                            return
                        identity, agent, item_id, content, _enqueued = record
                        if os.environ.get("CODEX_BENCH_TEST_DISPATCH_FAILURE") == "1":
                            raise RuntimeError("injected benchmark dispatch failure")
                        if importer and not gate.is_set():
                            gate.set()
                            if not import_work_started.wait(time_left(deadline)):
                                raise TimeoutError("analytics importer did not begin work after first dispatch")
                            if import_report.get("error"):
                                raise RuntimeError("analytics importer failed: " + import_report["error"])
                        dispatch_times[identity] = now()
                        runtime.notification({"method": "item/completed", "params": {
                            "threadId": agent["threadId"], "turnId": agent["turnId"],
                            "item": {"id": item_id, "type": "agentMessage", "text": content}}})
                        notification_completed_times[identity] = now()
                        drained[0] += 1
                    except BaseException as error:
                        errors.put(f"dispatch worker {threading.current_thread().name}: {type(error).__name__}: {error}")
                        stop.set()
                        receipt_changed.set()
                    finally:
                        work.task_done()

            worker_count = min(agent_count, 32)
            workers = [threading.Thread(target=dispatch_worker, name=f"benchmark-dispatch-{i}", daemon=False)
                       for i in range(worker_count)]
            for worker in workers:
                worker.start()
            event_start_ns = now()
            produce()
            wait_queue_drained(work, deadline)
            end_ns = now()
            for _ in workers:
                work.put(None, timeout=EVENT_TIMEOUT_SECONDS)
            for worker in workers:
                worker.join(EVENT_TIMEOUT_SECONDS)
                if worker.is_alive():
                    raise RuntimeError(f"dispatch worker {worker.name} failed to stop")
            while len(receipts) < len(messages):
                if not errors.empty():
                    raise RuntimeError(errors.get())
                receipt_changed.clear()
                if len(receipts) >= len(messages):
                    break
                receipt_changed.wait(time_left(deadline))
            if importer:
                importer.join(time_left(deadline))
                if importer.is_alive():
                    raise RuntimeError("analytics importer failed to stop")
                if import_report.get("error"):
                    raise RuntimeError("analytics import failed: " + import_report["error"])
                write_times = import_report.get("writeTimestampsNs", [])
                execution_intervals = [(started, notification_completed_times[identity])
                                       for identity, started in dispatch_times.items()]
                offer_start = min(scheduled_times.values()) if scheduled_times else end_ns
                receipt_end = max(receipts.values()) if receipts else end_ns
                import_report.update(import_overlap(write_times, execution_intervals,
                                                    (offer_start, receipt_end)))
                import_report["expectedRecords"] = SYNTHETIC_HISTORY_RECORDS + 2  # Header, context, and usage rows.
                import_report["syntheticDataRecords"] = SYNTHETIC_HISTORY_RECORDS
                import_report["importedRecords"] = (import_report.get("state") or {}).get("importedRecords", 0)
                if not import_report["overlappedEventWindow"]:
                    raise RuntimeError("analytics import did not overlap the measured event window")
                if import_report["importedRecords"] < import_report["expectedRecords"]:
                    raise RuntimeError("analytics history import did not ingest the complete synthetic journal: "
                                       + repr(import_report.get("state")))
                if import_report.get("analyticsRowsWritten") != SYNTHETIC_HISTORY_RECORDS:
                    raise RuntimeError("analytics history import did not write every synthetic usage row")

            if not errors.empty():
                raise RuntimeError(errors.get())
            missing = sorted(set(messages) - set(receipts))
            if missing:
                raise RuntimeError(f"missing {len(missing)} SSE final messages; first identities: {missing[:5]}")
            if len(messages) != expected_count:
                raise RuntimeError(f"offered {expected_count} messages but queued {len(messages)}")
            enqueue_to_dispatch = [(dispatch_times[key] - enqueue_times[key]) / 1e6 for key in messages]
            dispatch_to_receipt = [(receipts[key] - dispatch_times[key]) / 1e6 for key in messages]
            enqueue_to_receipt = [(receipts[key] - enqueue_times[key]) / 1e6 for key in messages]
            scheduled_to_enqueue = [(enqueue_times[key] - scheduled_times[key]) / 1e6 for key in messages]
            scheduled_to_receipt = [(receipts[key] - scheduled_times[key]) / 1e6 for key in messages]
            return {"agents": agent_count, "messagesPerAgent": messages_per_agent,
                    "expected": expected_count, "received": len(receipts), "samples": len(enqueue_to_receipt),
                    "repetition": repetition, "analyticsImport": with_analytics, "transport": transport,
                    "latencyMs": {"scheduledToEnqueue": summary(scheduled_to_enqueue),
                                  "scheduledToClientReceipt": summary(scheduled_to_receipt),
                                  "enqueueToDispatch": summary(enqueue_to_dispatch),
                                  "dispatchToClientReceipt": summary(dispatch_to_receipt),
                                  "enqueueToClientReceipt": summary(enqueue_to_receipt)},
                    "elapsedMs": (max(receipts.values()) - event_start_ns) / 1e6,
                    "offeredRateMessagesPerSecond": rate_hz,
                    "perAgentOfferedRateMessagesPerSecond": rate_hz / agent_count,
                    "producerLatenessMs": summary([value / 1e6 for value in producer_lateness.values()]),
                    "cpuProcessSeconds": time.process_time() - process_started,
                    "peakRss": rss_peak(), "queuePeak": work.peak,
                    "queueDrained": drained[0], "analyticsProgress": import_report or None}
        finally:
            stop.set()
            gate.set()
            prior_exception = sys.exc_info()[0]
            cleanup_errors = []
            if importer and importer.is_alive():
                importer.join(CLEANUP_TIMEOUT_SECONDS)
                if importer.is_alive():
                    cleanup_errors.append("analytics importer failed to stop before deadline")
            active_workers = [worker for worker in workers if worker.is_alive()]
            try:
                for _ in active_workers:
                    work.put(None, timeout=CLEANUP_TIMEOUT_SECONDS)
                for worker in active_workers:
                    worker.join(CLEANUP_TIMEOUT_SECONDS)
                    if worker.is_alive():
                        cleanup_errors.append(f"dispatch worker {worker.name} failed to stop before deadline")
            except Exception as error:
                cleanup_errors.append(f"dispatch worker cleanup failed: {error}")
            try:
                runtime.close()
            except Exception as error:
                cleanup_errors.append(f"runtime cleanup failed: {error}")
            server.shutdown()
            server.server_close()
            for client in clients:
                client.join(CLEANUP_TIMEOUT_SECONDS)
                if client.is_alive():
                    cleanup_errors.append(f"SSE client {client.name} failed to stop before deadline")
            server_thread.join(CLEANUP_TIMEOUT_SECONDS)
            if server_thread.is_alive():
                cleanup_errors.append("HTTP server thread failed to stop before deadline")
            if cleanup_errors and prior_exception is None:
                raise RuntimeError("; ".join(cleanup_errors))


def run_supervised_case(agent_count: int, messages_per_agent: int, rate_hz: float,
                        with_analytics: bool, repetition: int = 1,
                        inject: str | None = None, transport: str = "sync",
                        timeout: float = CASE_TIMEOUT_SECONDS) -> dict:
    """Run one case in a child process the parent can terminate at its deadline."""
    payload = {"agent_count": agent_count, "messages_per_agent": messages_per_agent,
               "rate_hz": rate_hz, "with_analytics": with_analytics,
               "repetition": repetition, "inject": inject, "transport": transport}
    deadline = time.monotonic() + timeout
    # The parent owns cleanup even when a killed child cannot run its finally block.
    with tempfile.TemporaryDirectory(prefix="studio-message-supervisor-") as temporary:
        payload["temporary_parent"] = temporary
        process = subprocess.Popen([sys.executable, "-B", str(Path(__file__).resolve()), "--_case-worker"],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        try:
            stdout, stderr = process.communicate(json.dumps(payload), timeout=max(0.01, deadline - time.monotonic()))
        except subprocess.TimeoutExpired as error:
            process.kill()
            stdout, stderr = process.communicate()
            diagnostic = stderr.strip() or stdout.strip()
            raise TimeoutError(f"case process exceeded its {timeout:g}-second parent deadline; "
                               f"child terminated. {diagnostic}") from error
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()
    if process.returncode != 0:
        raise RuntimeError(f"case process exited {process.returncode}: {stderr.strip() or stdout.strip()}")
    try:
        return json.loads(stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"case process returned invalid JSON: {stdout[:500]!r}; {stderr.strip()}") from error

def main(argv=None) -> int:
    if argv is None and sys.argv[1:] == ["--_case-worker"]:
        try:
            case = json.load(sys.stdin)
            print(json.dumps(run_case(**case), separators=(",", ":")))
            return 0
        except BaseException as error:
            print(f"message delivery case worker failed: {type(error).__name__}: {error}", file=sys.stderr)
            return 1
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agents", type=int, nargs="+", default=list(DEFAULT_AGENT_COUNTS))
    parser.add_argument("--messages-per-agent", type=int, default=DEFAULT_MESSAGES_PER_AGENT)
    parser.add_argument("--rate", type=float, default=DEFAULT_RATE_HZ, help="offered event rate (messages/second)")
    parser.add_argument("--repetitions", type=int, default=DEFAULT_REPETITIONS)
    parser.add_argument("--analytics", choices=("off", "on", "both"), default="both")
    parser.add_argument("--transport", choices=("sync", "transcript", "both"), default="both")
    parser.add_argument("--output", type=Path, help="JSON report path; defaults to stdout")
    parser.add_argument("--check", action="store_true", help="quick local smoke configuration")
    args = parser.parse_args(argv)
    if args.check:
        args.agents, args.messages_per_agent, args.repetitions, args.analytics = [1], 8, 1, "off"
        args.transport = "sync"
    if (not set(args.agents) <= SUPPORTED_AGENT_COUNTS or len(set(args.agents)) != len(args.agents)):
        parser.error("--agents accepts distinct values from 1, 8, and 32")
    if not 1 <= args.messages_per_agent <= MAX_MESSAGES_PER_AGENT:
        parser.error(f"--messages-per-agent must be 1..{MAX_MESSAGES_PER_AGENT}")
    if (not math.isfinite(args.rate) or not 0 < args.rate <= MAX_OFFERED_RATE_HZ):
        parser.error(f"--rate must be finite and in (0, {MAX_OFFERED_RATE_HZ:g}]")
    if not 1 <= args.repetitions <= MAX_CASE_REPETITIONS:
        parser.error(f"--repetitions must be 1..{MAX_CASE_REPETITIONS}")
    total_cases = len(args.agents) * (2 if args.analytics == "both" else 1) \
        * (2 if args.transport == "both" else 1) * args.repetitions
    if total_cases > MAX_TOTAL_CASES:
        parser.error(f"the selected matrix exceeds the {MAX_TOTAL_CASES}-case command limit")
    if max(args.agents) * args.messages_per_agent / args.rate > MAX_OFFER_WINDOW_SECONDS:
        parser.error(f"the slowest offered workload exceeds {MAX_OFFER_WINDOW_SECONDS:g} seconds")
    if args.output and args.output.resolve().is_relative_to(Path(__file__).resolve().parents[3]):
        parser.error("--output must be outside the repository checkout")
    cases = [False, True] if args.analytics == "both" else [args.analytics == "on"]
    transports = ["sync", "transcript"] if args.transport == "both" else [args.transport]
    report = {"schemaVersion": 1, "benchmark": "message_delivery", "timestampUtc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "config": {"agents": args.agents, "messagesPerAgent": args.messages_per_agent,
                         "offeredRateMessagesPerSecond": args.rate, "repetitions": args.repetitions,
                         "analyticsImportModes": ["on" if value else "off" for value in cases],
                         "transports": transports,
                         "invalidationPath": "Runtime.notification -> SQLite transcript -> protocol-3 resource SSE",
                         "syncPullPath": "protocol-3 invalidation -> /api/sync/pull?scope=transcript:<id>",
                         "transcriptPullPath": "protocol-3 invalidation -> /api/transcript?id=<id>",
                         "clock": "time.monotonic_ns", "nativeTransport": False},
              "cases": []}
    try:
        for count in args.agents:
            for analytics in cases:
                for transport in transports:
                    for repetition in range(1, args.repetitions + 1):
                        report["cases"].append(run_supervised_case(
                            count, args.messages_per_agent, args.rate, analytics, repetition,
                            transport=transport))
    except Exception as error:
        print(f"message delivery benchmark failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 1
    rendered = json.dumps(report, indent=2, sort_keys=True)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
    else:
        print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

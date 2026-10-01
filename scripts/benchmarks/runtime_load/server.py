#!/usr/bin/env python3
"""Start an isolated production Runtime/HTTP server for the runtime load harness."""
from __future__ import annotations

import json
import os
from pathlib import Path
import queue
import resource
import copy
import sys
import threading
import time
import uuid


ROOT = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts"))


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


def main():
    from codex_canvas import Canvas, make_server
    from codex_runtime import Runtime

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
    canvas = Canvas(root=state)
    canvas.runtime = runtime
    server = make_server(canvas)
    thread = threading.Thread(target=server.serve_forever, name="runtime-load-http", daemon=False)
    thread.start()
    deadline_s = 180
    queue_limit = 16384
    offered_rate = 160
    quick_check = os.environ.get("BENCH_QUICK_CHECK") == "1"
    workers_per_team = 2 if quick_check else 32
    team_count = 1 if quick_check else 8
    inbox = queue.Queue(maxsize=queue_limit)
    peak = 0
    lock = threading.Lock()
    accepted = {}
    dispatched = {}
    queue_latencies = []
    dispatch_latencies = []
    category_offered = {}
    category_dispatched = {}
    errors = queue.Queue()
    stop = threading.Event()
    # Create one production Runtime record as a shape template, then seed the
    # other synthetic identities with Runtime.put. This is fixture setup, not
    # agent spawning: no scheduler/native session is involved.
    template = runtime.create({"name": "Load lead 01", "cwd": str(ROOT),
                               "prompt": "Synthetic active-turn benchmark identity"}, defer=True)
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
        if parent is None:
            record["rootId"] = record["id"]
        return record

    with runtime.lock, runtime.db() as db:
        template.update(threadId=str(uuid.uuid4()), autoWake=True, yoloMode=True,
                        status="running", inFlight=True, turnId="bench-turn-lead-0",
                        syntheticBenchmarkTurn=True)
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
                      "workersPerTeam": workers_per_team}), flush=True)

    def receiver():
        nonlocal peak
        while not stop.is_set() or not inbox.empty():
            try:
                task = inbox.get(timeout=.05)
            except queue.Empty:
                continue
            if task is None:
                inbox.task_done()
                return
            kind, key, due_ns, queued_ns, fn = task
            started = time.monotonic_ns()
            try:
                fn()
                ended = time.monotonic_ns()
                with lock:
                    accepted[key] = (due_ns, queued_ns)
                    dispatched[key] = ended
                    queue_latencies.append((started - queued_ns) / 1e6)
                    dispatch_latencies.append((ended - started) / 1e6)
                    category_dispatched[kind] = category_dispatched.get(kind, 0) + 1
            except BaseException as exc:
                errors.put(f"{kind}/{key}: {type(exc).__name__}: {exc}")
            finally:
                inbox.task_done()

    # A single ordered consumer mirrors the one shared account AppServer callback
    # dispatcher. The benchmark queue is still explicitly fixture-owned.
    receivers = [threading.Thread(target=receiver, name="runtime-load-dispatch-0", daemon=False)]
    for worker in receivers:
        worker.start()

    for line in sys.stdin:
        command = json.loads(line)
        if command.get("action") == "run":
            rounds = int(command.get("rounds", 1))
            if not 1 <= rounds <= 8:
                raise ValueError("rounds must be in 1..8")
            with runtime.db() as db:
                event_rows_before = db.execute("SELECT COUNT(*) FROM runtime_events").fetchone()[0]
            total_started = time.monotonic_ns()
            identities = []
            for agent in workers:
                team = workers.index(agent) // workers_per_team
                identities.append((agent, leads[team]))
            serial = 0

            def offer(kind, key, fn, due):
                nonlocal peak
                wait = (due - time.monotonic_ns()) / 1e9
                if wait > 0:
                    time.sleep(wait)
                now = time.monotonic_ns()
                due_ns = due
                queued_ns = now
                inbox.put((kind, key, due_ns, queued_ns, fn), timeout=10)
                with lock:
                    peak = max(peak, inbox.qsize())
                    category_offered[kind] = category_offered.get(kind, 0) + 1

            origin_due = time.monotonic_ns() + 100_000_000
            for phase, scale in (("ramp", 2), ("steady", 1), ("burst", 4), ("drain", 1)):
                phase_count = len(workers) * rounds
                for i in range(phase_count):
                    agent, lead = identities[i % len(identities)]
                    local_round = i // len(workers)
                    turn_id = f"bench-turn-{phase}-{local_round}-{agent['id']}"
                    item_id = f"bench-item-{phase}-{local_round}-{agent['id']}"
                    text = f"{phase} synthetic answer from {agent['name']} round {local_round}"
                    base_due = origin_due + int(serial * 1_000_000_000 / (offered_rate * scale))
                    sequence = [
                        ("turnLifecycle", "turn/started", {"turn": {"id": turn_id}}),
                        ("toolStatus", "item/started", {"turnId": turn_id, "item": {"id": item_id + "-tool", "type": "commandExecution", "command": "[synthetic tool]"}}),
                        ("toolOutput", "item/completed", {"turnId": turn_id, "item": {"id": item_id + "-tool", "type": "commandExecution", "aggregatedOutput": "synthetic tool output\n", "exitCode": 0}}),
                        ("assistantDelta", "item/agentMessage/delta", {"threadId": agent["threadId"], "turnId": turn_id, "itemId": item_id, "delta": text}),
                        ("assistantFinal", "item/completed", {"threadId": agent["threadId"], "turnId": turn_id, "item": {"id": item_id, "type": "agentMessage", "text": text, "phase": "final_answer"}}),
                        ("turnLifecycle", "turn/completed", {"threadId": agent["threadId"], "turnId": turn_id, "turn": {"id": turn_id, "status": "completed"}}),
                    ]
                    for kind, method, params in sequence:
                        key = f"evt:{phase}:{local_round}:{agent['id']}:{method}:{params.get('itemId') or params.get('item', {}).get('id', '')}"
                        payload = {"method": method, "params": {"threadId": agent["threadId"], **params}}
                        offer(kind, key, lambda payload=payload: runtime.notification(payload), base_due)
                    # Durable event APIs write worker→worker and worker→lead messages.
                    team_start = (i % len(workers) // workers_per_team) * workers_per_team
                    team_index = i % workers_per_team
                    peer = workers[team_start + (team_index + 1) % workers_per_team]
                    for recipient, label in ((peer, "peer"), (lead, "lead")):
                        msg_id = str(uuid.uuid5(uuid.NAMESPACE_URL, f"{phase}:{local_round}:{agent['id']}:{label}"))
                        offer("durableMessage", f"msg:{msg_id}", lambda a=agent, r=recipient, mid=msg_id, label=label:
                              runtime.chat_message(a["id"], r["id"], f"{phase} coordination {label} {mid}", mid), base_due)
                    serial += 1
                origin_due += int(max(phase_count / (offered_rate * scale), .1) * 1_000_000_000) + 100_000_000

            inbox.join()
            if not errors.empty():
                raise RuntimeError(errors.get())
            remaining = deadline_s - (time.monotonic() - total_started / 1e9)
            if remaining <= 0:
                raise TimeoutError("runtime load exceeded deadline")
            chat_rows = 0
            event_rows = 0
            event_kinds = {}
            analytics_rows = 0
            analytics_payload_bytes = 0
            transcript_counts = {k: 0 for k in ("assistant", "tool", "output")}
            with runtime.db() as db:
                chat_rows = db.execute("SELECT COUNT(*) FROM runtime_chat_messages").fetchone()[0]
                event_rows = db.execute("SELECT COUNT(*) FROM runtime_events").fetchone()[0]
                event_kinds = dict(db.execute(
                    "SELECT kind,COUNT(*) FROM runtime_events GROUP BY kind"
                ).fetchall())
                analytics = db.execute("SELECT COUNT(*),COALESCE(SUM(bytes),0) FROM analytics_notifications").fetchone()
                analytics_rows, analytics_payload_bytes = analytics[0], analytics[1]
            for agent in workers:
                for item in runtime.transcript(agent["id"])["items"]:
                    transcript_counts[item.get("role", "unknown")] = transcript_counts.get(item.get("role", "unknown"), 0) + 1
            usage = resource.getrusage(resource.RUSAGE_SELF)
            report = {"teams": team_count, "workersPerTeam": workers_per_team,
                      "syntheticActiveTurns": len(workers) + len(leads), "rounds": rounds,
                      "phases": ["ramp", "steady", "burst", "drain"],
                      "offered": category_offered, "dispatched": category_dispatched,
                      "exactlyOnce": {"offeredEvents": sum(category_offered.values()),
                                      "completedDispatches": sum(category_dispatched.values()),
                                      "uniqueDispatchedIdentities": len(dispatched),
                                      "durableChatMessages": chat_rows,
                                      "queuedRuntimeEventsBefore": event_rows_before,
                                      "queuedRuntimeEventsAfter": event_rows,
                                      "queuedRuntimeEventsAdded": event_rows - event_rows_before,
                                      "queuedRuntimeEventsByKind": event_kinds},
                      "queue": {"capacity": queue_limit, "peak": peak, "drained": inbox.unfinished_tasks == 0,
                                "depthAtDrain": inbox.qsize()},
                      "latencyMs": {"harnessQueueWait": stats(queue_latencies),
                                    "runtimeDispatch": stats(dispatch_latencies)},
                      "transcriptItemsByRole": transcript_counts,
                      "analytics": {"notificationRows": analytics_rows,
                                    "payloadBytes": analytics_payload_bytes},
                      "cpuProcessSeconds": time.process_time(),
                      "peakRss": {"value": usage.ru_maxrss,
                                  "unit": "KiB" if sys.platform != "darwin" else "bytes",
                                  "source": "resource.getrusage(RUSAGE_SELF).ru_maxrss"},
                      "elapsedSeconds": time.monotonic_ns() / 1e9 - total_started / 1e9}
            print(json.dumps({"kind": "result", "report": report}), flush=True)
            break
    stop.set()
    for _ in receivers:
        inbox.put(None, timeout=5)
    for worker in receivers:
        worker.join(5)
    runtime.close()
    server.shutdown()
    server.server_close()
    thread.join(5)
    if thread.is_alive():
        raise RuntimeError("HTTP server failed to stop")


if __name__ == "__main__":
    main()

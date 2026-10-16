"""Differential fixtures for the completed-start restart recovery exclusion."""
import copy
import concurrent.futures
import importlib.util
import json
import pathlib
import shutil
import sqlite3
import sys
import threading
import time
from unittest.mock import patch


CASES = [
    {"id": "completed_start", "flow": "completed", "mutations": {}},
    {"id": "in_flight", "flow": "written", "mutations": {}},
    {"id": "unknown_outcome", "flow": "unknown", "mutations": {}},
    {"id": "auth_pin_changed", "flow": "auth_pin_changed", "mutations": {}},
    {"id": "action", "mutations": {"startAttempt.action": "turn/start"},
     "handShapedReason": "The public flow cannot persist an action field on a completed start receipt."},
    {"id": "active_reservation", "mutations": {"startAttempt.activeAtReservation": True},
     "handShapedReason": "This is a scheduler reservation predicate; the provider flow does not expose a completed receipt with the reservation bit retained."},
    {"id": "outcome_hold", "mutations": {"startOutcomeHold": {"stage": "account_settlement"}},
     "handShapedReason": "The hold is an account-settlement recovery marker, unavailable from the scripted provider's normal completion path."},
    {"id": "response_error", "mutations": {"startAttempt.responseError": "lost response"},
     "handShapedReason": "The stored responseError is an internal recovery field; the scripted provider flow covers unknown response separately."},
    {"id": "different_turn", "mutations": {"startAttempt.turnId": "other-turn"},
     "handShapedReason": "A contradictory turn binding is an intentionally malformed historical receipt, not a possible successful provider flow."},
    {"id": "non_completed_last_status", "mutations": {"lastCompletedTurnStatus": "failed"},
     "handShapedReason": "This isolates the terminal-status predicate while retaining a completed start receipt, which the normal provider flow cannot produce."},
    {"id": "outside_idle_status", "mutations": {"status": "queued"},
     "handShapedReason": "This isolates the completed-start status predicate while avoiding the independent running-turn interruption path; the real in-flight case is covered separately."},
]


def _set_path(value, path, replacement):
    parts = path.split(".")
    target = value
    for part in parts[:-1]:
        target = target.setdefault(part, {})
    target[parts[-1]] = replacement


def _state(root):
    with sqlite3.connect(pathlib.Path(root) / "canvas.sqlite3") as db:
        agents = [{"id": row[0], "record": json.loads(row[1])} for row in db.execute("SELECT id,record FROM runtime_agents ORDER BY id")]
        names = [row[1] for row in db.execute("PRAGMA table_info(runtime_events)")]
        events = [dict(zip(names, row)) for row in db.execute("SELECT * FROM runtime_events ORDER BY id")]
    return {"agents": agents, "events": events}


def _changed_paths(before, after, path=""):
    if isinstance(before, dict) and isinstance(after, dict):
        return sorted(set().union(*(
            _changed_paths(before.get(key), after.get(key), path + "/" + key)
            for key in before.keys() | after.keys()
        )))
    if isinstance(before, list) and isinstance(after, list) and len(before) == len(after):
        return sorted(set().union(*(
            _changed_paths(left, right, path + "/" + str(index))
            for index, (left, right) in enumerate(zip(before, after))
        )))
    return [] if before == after else [path]


def run_case(command, template, QuietRuntime):
    case = command["case"]
    root = pathlib.Path(command["root"])
    rust = pathlib.Path(command["rust"])
    if case.get("flow"):
        if case["flow"] == "auth_pin_changed":
            fixture_path = (pathlib.Path(__file__).parents[6]
                            / "workspaces/runtime/apps/server/tests/claude-auth-wait-contract.py")
            spec = importlib.util.spec_from_file_location("claude_auth_wait_fixture", fixture_path)
            fixture = importlib.util.module_from_spec(spec)
            sys.modules[spec.name] = fixture
            spec.loader.exec_module(fixture)
            flow = fixture.AuthWait("test_fresh_same_account_queues_original_inputs_once")
            harness = sys.modules["__main__"]
            enable_pool = patch.object(
                concurrent.futures.ThreadPoolExecutor, "submit", harness.ORIGINAL_EXECUTOR_SUBMIT
            )
            enable_threads = patch.object(threading.Thread, "start", harness.ORIGINAL_THREAD_START)
            enable_catalog = patch.object(
                fixture.codex_runtime.Runtime,
                "catalog",
                lambda _runtime, _account: {"data": [{
                    "model": "default",
                    "defaultReasoningEffort": "medium",
                    "supportedReasoningEfforts": [{"reasoningEffort": "medium"}],
                }]},
            )
            enable_pool.start()
            enable_threads.start()
            enable_catalog.start()
            runtime = None
            try:
                flow.setUp()
                runtime = flow.runtime
                flow.fail_auth()
                with runtime.accounts.lock:
                    runtime.accounts._row("claude-local")["claudeOptions"] = {
                        "configDir": str(flow.root / "changed-profile")
                    }
                    runtime.accounts._save()
                root = flow.root / "state"
                before = _state(root)
                rust.mkdir(parents=True)
                for directory in ("multi-server", "accounts"):
                    source = root / directory
                    if source.exists():
                        shutil.copytree(source, rust / directory, dirs_exist_ok=True)
                if (root / "analytics.sqlite3").exists():
                    shutil.copyfile(root / "analytics.sqlite3", rust / "analytics.sqlite3")
                with sqlite3.connect(root / "canvas.sqlite3") as source, sqlite3.connect(rust / "canvas.sqlite3") as target:
                    source.backup(target)
                runtime.close()
                runtime = QuietRuntime(root, lambda *_a, **_kw: None)
                after = _state(root)
                return {"case": case["id"], "flow": case["flow"],
                        "providerFrameCount": 0, "before": before, "after": after,
                        "pythonChangedPaths": _changed_paths(before, after)}
            finally:
                if runtime is not None:
                    runtime.close()
                flow.doCleanups()
                enable_catalog.stop()
                enable_threads.stop()
                enable_pool.stop()
        fixture_path = (pathlib.Path(__file__).parents[6]
                        / "workspaces/runtime/apps/server/tests/turn-start-contract.py")
        spec = importlib.util.spec_from_file_location("turn_start_fixture", fixture_path)
        fixture = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = fixture
        spec.loader.exec_module(fixture)
        flow = fixture.TurnStartContract("test_completed_turn_survives_timeout_and_late_reply")
        flow.setUp()
        runtime = flow.runtime
        # Keep the public admission path real while avoiding the asynchronous
        # model/list fixture request racing the scripted turn/start request.
        runtime.catalog = lambda _account: {"data": [{
            "model": "gpt-6-astra",
            "defaultReasoningEffort": "medium",
            "supportedReasoningEfforts": [
                {"reasoningEffort": value}
                for value in ("low", "medium", "high", "xhigh", "max", "ultra")
            ],
        }]}
        root = pathlib.Path(runtime.root)
        try:
            mode = {"completed": "completed", "written": "started", "unknown": "silent"}[case["flow"]]
            class CapturedDelivery:
                def __init__(self):
                    self.jobs = []

                def submit(self, function, *args, **kwargs):
                    self.jobs.append((function, args, kwargs))

            delivery = CapturedDelivery()
            runtime.delivery_executor = lambda: delivery
            flow.server.mode = mode
            agent = runtime.create({"name": "Lead", "cwd": flow.temp.name, "prompt": "First input"})
            agent_id = agent["id"]
            harness = sys.modules["__main__"]
            with patch.object(concurrent.futures.ThreadPoolExecutor, "submit",
                              harness.ORIGINAL_EXECUTOR_SUBMIT), \
                    patch.object(threading.Thread, "start", harness.ORIGINAL_THREAD_START):
                runtime.dispatch(agent_id)
                assert len(delivery.jobs) == 1, f"{case['flow']}: no real Runtime.start job was reserved"
                function, args, kwargs = delivery.jobs[0]
                function(*args, **kwargs)
                deadline = time.monotonic() + 5
                while not flow.server.deferred and time.monotonic() < deadline:
                    time.sleep(0.02)
                if not flow.server.deferred:
                    current = runtime.agent(agent_id)
                    with runtime.read_db() as db:
                        event_rows = [dict(row) for row in db.execute(
                            "SELECT id,status,error FROM runtime_events WHERE agent=?", (agent_id,)
                        )]
                    raise AssertionError(
                        f"{case['flow']}: scripted provider received no turn/start; "
                        f"status={current.get('status')!r} error={current.get('error')!r} "
                        f"attempt={current.get('startAttempt')!r} events={event_rows!r} "
                        f"calls={flow.server.calls!r}"
                    )
                if case["flow"] in {"completed", "written"}:
                    flow.accept()
                    entry = runtime.server.deferred[0]
                    if case["flow"] == "completed":
                        fixture.eventually(lambda: runtime.agent(agent_id)["status"] == "completed")
                    else:
                        fixture.eventually(lambda: runtime.agent(agent_id)["startAttempt"].get("submitted") is True)
                    assert entry["handled"].wait(60)
                else:
                    fixture.eventually(lambda: runtime.agent(agent_id)["startAttempt"].get("executionOutcome") == "unknown")
            agent = runtime.agent(agent_id)
            if case.get("mutations"):
                for path, value in case["mutations"].items():
                    _set_path(agent, path, value)
                with runtime.db() as db:
                    runtime.put(db, "agents", agent)
            provider_frames = sum(method == "turn/start" for method, _ in runtime.server.calls)
            before = _state(root)
            rust.mkdir(parents=True)
            shutil.copytree(root / "multi-server", rust / "multi-server", dirs_exist_ok=True)
            if (root / "accounts").exists():
                shutil.copytree(root / "accounts", rust / "accounts", dirs_exist_ok=True)
            if (root / "analytics.sqlite3").exists():
                shutil.copyfile(root / "analytics.sqlite3", rust / "analytics.sqlite3")
            with sqlite3.connect(root / "canvas.sqlite3") as source, sqlite3.connect(rust / "canvas.sqlite3") as target:
                source.backup(target)
            runtime.close()
            runtime = QuietRuntime(root, lambda *_a, **_kw: None)
            after = _state(root)
            return {"case": case["id"], "flow": case["flow"],
                    "providerFrameCount": provider_frames, "before": before, "after": after,
                    "pythonChangedPaths": _changed_paths(before, after),
                    "handShapedReason": case.get("handShapedReason")}
        finally:
            runtime.close()
            flow.temp.cleanup()

    shutil.copytree(template, root)
    runtime = QuietRuntime(root, lambda *_a, **_kw: None)
    try:
        # These predicate variants intentionally isolate internal receipt fields.
        # Completed, written and unknown attempts above are produced by the scripted provider.
        agent = runtime.create({"name": "recovery fixture", "cwd": str(root), "prompt": "seed"}, defer=True)
        agent.update(status="completed", threadId="thread", turnId="turn",
                     lastCompletedTurn="turn", lastCompletedTurnStatus="completed",
                     inFlight=False, autoWake=False)
        agent["startAttempt"] = {
            "id": "attempt", "epoch": agent["epoch"], "accountKey": "default",
            "events": ["event"], "submitted": True, "activeAtReservation": False,
            "executionOutcome": "completed", "turnId": "turn",
        }
        for path, value in case["mutations"].items():
            _set_path(agent, path, value)
        with runtime.db() as db:
            runtime.put(db, "agents", agent)
            runtime.enqueue(db, agent, "user", "recovery fixture", key="event")
        before = _state(root)
        rust.mkdir(parents=True)
        shutil.copytree(root / "multi-server", rust / "multi-server", dirs_exist_ok=True)
        shutil.copyfile(root / "analytics.sqlite3", rust / "analytics.sqlite3")
        with sqlite3.connect(root / "canvas.sqlite3") as source, sqlite3.connect(rust / "canvas.sqlite3") as target:
            source.backup(target)

        # Runtime construction is the production restart path and applies the
        # scheduler/recovery predicates to Python-written durable state.
        runtime.close()
        runtime = QuietRuntime(root, lambda *_a, **_kw: None)
        after = _state(root)
        return {"case": case["id"], "before": before, "after": after,
                "pythonChangedPaths": _changed_paths(before, after),
                "handShapedReason": case.get("handShapedReason")}
    finally:
        runtime.close()

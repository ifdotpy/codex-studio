#!/usr/bin/env python3
"""Fast dispatch coalesces per agent while event identity stays durable."""

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import json
import sqlite3
import threading
import unittest
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_runtime import Runtime


class ManualExecutor:
    def __init__(self, fail=False):
        self.tasks = []
        self.fail = fail

    def submit(self, function, *args):
        if self.fail:
            raise RuntimeError("executor stopped")
        self.tasks.append((function, args))
        return object()


def bare_runtime(executor=None):
    runtime = Runtime.__new__(Runtime)
    runtime.lock = threading.RLock()
    runtime._dispatch_executor = executor or ManualExecutor()
    runtime._fast_dispatch_lock = threading.Lock()
    runtime._fast_dispatch_states = {}
    runtime.changed = threading.Event()
    runtime.closed = False
    return runtime


class FastDispatchCoalescingContract(unittest.TestCase):
    def test_queued_events_coalesce_per_agent_and_keep_other_agents_runnable(self):
        executor = ManualExecutor()
        runtime = bare_runtime(executor)
        runtime.schedule_fast_dispatch("agent-a", "user", 101)
        runtime.schedule_fast_dispatch("agent-a", "user", 202)
        runtime.schedule_fast_dispatch("agent-a", "tool", 250)
        runtime.schedule_fast_dispatch("agent-b", "tool", 303)

        self.assertEqual(len(executor.tasks), 2)
        state = runtime._fast_dispatch_states["agent-a"]
        self.assertEqual(state["current"], (101, 250))
        self.assertTrue(state["current_urgent"])
        self.assertTrue(state["current_wake"].is_set())
        self.assertNotIn("event_ids", state)
        self.assertEqual(runtime._fast_dispatch_states["agent-b"]["current"], (303, 303))

    def test_arrival_during_pass_gets_one_successor_then_state_clears(self):
        executor = ManualExecutor()
        runtime = bare_runtime(executor)
        calls = []

        def dispatch(agent_id, _event_ids=None, _scheduled_at=None, event_window=None):
            calls.append((agent_id, event_window))
            if len(calls) == 1:
                runtime.schedule_fast_dispatch(agent_id, "user", 202)

        runtime.dispatch_fast = dispatch
        runtime.schedule_fast_dispatch("agent-a", "tool", 101)
        first = executor.tasks[0]
        first[0](*first[1])

        self.assertEqual(calls, [("agent-a", (101, 101))])
        self.assertEqual(len(executor.tasks), 2)
        self.assertEqual(runtime._fast_dispatch_states["agent-a"]["current"], (202, 202))
        second = executor.tasks[1]
        second[0](*second[1])

        self.assertEqual(calls, [("agent-a", (101, 101)), ("agent-a", (202, 202))])
        self.assertNotIn("agent-a", runtime._fast_dispatch_states)

    def test_dispatch_failure_clears_state_and_wakes_durable_fallback(self):
        executor = ManualExecutor()
        runtime = bare_runtime(executor)

        def dispatch(*_args, **_kwargs):
            raise RuntimeError("dispatch failed")

        runtime.dispatch_fast = dispatch
        runtime.schedule_fast_dispatch("agent-a", "tool", 101)
        function, args = executor.tasks[0]
        with self.assertRaisesRegex(RuntimeError, "dispatch failed"):
            function(*args)

        self.assertNotIn("agent-a", runtime._fast_dispatch_states)
        self.assertTrue(runtime.changed.is_set())

    def test_window_timing_uses_each_durable_queue_stamp(self):
        runtime = Runtime.__new__(Runtime)
        db = sqlite3.connect(":memory:")
        db.row_factory = sqlite3.Row
        db.executescript("""
            CREATE TABLE runtime_events (id TEXT PRIMARY KEY, agent TEXT, status TEXT);
            CREATE TABLE runtime_event_meta (id TEXT PRIMARY KEY, record TEXT NOT NULL);
        """)
        for event_id, agent_id, queued_at in (
            ("event-a1", "agent-a", 101),
            ("event-a2", "agent-a", 202),
            ("event-a3", "agent-a", 303),
            ("event-b1", "agent-b", 150),
        ):
            db.execute("INSERT INTO runtime_events VALUES (?, ?, 'pending')", (event_id, agent_id))
            db.execute("INSERT INTO runtime_event_meta VALUES (?, ?)",
                       (event_id, json.dumps({"timing": {"fastQueuedAt": queued_at}})))

        runtime.mark_fast_dispatch_window(db, "agent-a", (101, 202), {
            "fastEnteredAt": 404,
            "fastLockOwners": ["scheduler"],
        })

        records = {row["id"]: json.loads(row["record"]) for row in
                   db.execute("SELECT id, record FROM runtime_event_meta")}
        self.assertEqual(records["event-a1"]["timing"]["fastScheduledAt"], 101)
        self.assertEqual(records["event-a2"]["timing"]["fastScheduledAt"], 202)
        self.assertEqual(records["event-a1"]["timing"]["fastEnteredAt"], 404)
        self.assertEqual(records["event-a2"]["timing"]["fastEnteredAt"], 404)
        self.assertNotIn("fastScheduledAt", records["event-a3"]["timing"])
        self.assertNotIn("fastScheduledAt", records["event-b1"]["timing"])
        db.close()

    def test_executor_submit_failure_removes_admission_and_wakes_fallback(self):
        runtime = bare_runtime(ManualExecutor(fail=True))
        with self.assertRaisesRegex(RuntimeError, "executor stopped"):
            runtime.schedule_fast_dispatch("agent-a", "tool", 101)

        self.assertNotIn("agent-a", runtime._fast_dispatch_states)
        self.assertTrue(runtime.changed.is_set())


if __name__ == "__main__":
    unittest.main()

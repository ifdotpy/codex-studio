#!/usr/bin/env python3
"""Deleting a conversation releases its active native turn slot."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "preparation_writer_fixture", Path(__file__).with_name("preparation-writer-lock-contract.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class DeleteInflightContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-delete-inflight-")
        self.addCleanup(self.temp.cleanup)
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.FakeServer)
        self.addCleanup(self.runtime.close)

    def slots(self):
        with self.runtime.read_db() as db:
            return self.runtime.dispatch_active_slots(db)

    def running_chat(self):
        agent = self.runtime.create(
            {"name": "Delete while running", "cwd": self.temp.name, "prompt": "Start"},
            defer=True,
        )
        baseline = len(self.slots())
        self.runtime.send(agent["id"], "Run a turn", "delete-inflight-start")
        self.runtime.dispatch(agent["id"])
        fixture.fixture.eventually(lambda: self.runtime.agent(agent["id"]).get("turnId"))
        self.assertEqual(len(self.slots()), baseline + 1)
        return self.runtime.agent(agent["id"]), baseline

    def assert_deleted_and_released(self, agent_id, baseline):
        current = self.runtime.agent(agent_id)
        self.assertTrue(current.get("deletedAt"))
        self.assertFalse(current["autoWake"])
        self.assertEqual(current["status"], "paused")
        self.assertFalse(current.get("inFlight"))
        self.assertIsNone(current.get("turnId"))
        self.assertEqual(len(self.slots()), baseline)

    def test_interrupt_answered_before_delete_returns_releases_slot(self):
        agent, baseline = self.running_chat()
        deleted = self.runtime.delete_conversation(agent["id"])
        self.assertEqual(deleted["deleted"], [agent["id"]])
        self.assert_deleted_and_released(agent["id"], baseline)

    def test_interrupt_timeout_releases_slot_and_late_completion_stays_tombstoned(self):
        agent, baseline = self.running_chat()
        server = self.runtime.server
        interrupt_calls = []
        original = server.call

        def call(method, params, timeout=60):
            if method == "turn/interrupt":
                interrupt_calls.append((method, params))
                raise RuntimeError("turn/interrupt response timed out; outcome unknown")
            return original(method, params, timeout)

        with patch.object(server, "call", side_effect=call):
            self.runtime.delete_conversation(agent["id"])
            self.assert_deleted_and_released(agent["id"], baseline)
            self.assertIn("outcome unknown", self.runtime.agent(agent["id"]).get("error", ""))
            self.assertEqual(server.active_turns[agent["threadId"]]["id"], agent["turnId"])
            repeated = self.runtime.delete_conversation(agent["id"])
        self.assertEqual(len(interrupt_calls), 1)
        self.assertEqual(repeated["deleted"], [agent["id"]])
        self.assert_deleted_and_released(agent["id"], baseline)

        before = self.runtime.agent(agent["id"])
        with self.runtime.db() as db:
            item_count = db.execute("SELECT COUNT(*) FROM runtime_items WHERE agent=?", (agent["id"],)).fetchone()[0]
            completed_count = db.execute("SELECT COUNT(*) FROM runtime_completed_turns WHERE id LIKE ?",
                                         (agent["id"] + ":%",)).fetchone()[0]
        for message in (
            {"method": "item/started", "params": {"threadId": agent["threadId"], "turnId": agent["turnId"],
                "item": {"id": "late-item", "type": "commandExecution", "command": "ignored"}}},
            {"method": "thread/tokenUsage/updated", "params": {"threadId": agent["threadId"],
                "tokenUsage": {"total": {"totalTokens": 999}, "last": {"totalTokens": 999},
                               "modelContextWindow": 1000}}},
            {"method": "error", "params": {"threadId": agent["threadId"], "turnId": agent["turnId"],
                "error": {"message": "late native error"}}},
            {"method": "turn/completed", "params": {"threadId": agent["threadId"],
                "turn": {"id": agent["turnId"], "status": "completed"}}},
        ):
            self.runtime.notification(message)
        self.assertEqual(self.runtime.agent(agent["id"]), before)
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_items WHERE agent=?", (agent["id"],)).fetchone()[0], item_count)
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_completed_turns WHERE id LIKE ?",
                                        (agent["id"] + ":%",)).fetchone()[0], completed_count)
        self.assert_deleted_and_released(agent["id"], baseline)

    def test_late_approval_is_declined_and_tool_call_is_rejected(self):
        agent, baseline = self.running_chat()
        self.runtime.delete_conversation(agent["id"])
        status = self.runtime.agent(agent["id"])["status"]
        server = self.runtime.server
        server.request({"id": "late-approval", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": agent["threadId"], "turnId": agent["turnId"], "itemId": "late"}})
        self.assertEqual(server.responses[-1], {"id": "late-approval", "result": {"decision": "decline"}})
        with self.runtime.db() as db:
            self.assertFalse(any(r["rpcId"] == "late-approval" for r in self.runtime.records(db, "requests")))
        self.assertEqual(self.runtime.agent(agent["id"])["status"], status)
        server.request({"id": "late-tool", "method": "item/tool/call", "params": {
            "threadId": agent["threadId"], "turnId": agent["turnId"], "callId": "late-tool-call",
            "tool": "orchestration_status", "arguments": {}}})
        self.assertFalse(server.responses[-1]["result"]["success"])
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db, "tool_requests"), [])
        self.assertEqual(self.runtime.agent(agent["id"])["status"], status)
        self.assertEqual(len(self.slots()), baseline)

    def test_delete_during_unanswered_start_interrupts_late_accepted_turn(self):
        agent, _ = self.running_chat()
        server = self.runtime.server
        server.complete(agent["threadId"], agent["turnId"])
        fixture.fixture.eventually(lambda: not self.runtime.agent(agent["id"]).get("inFlight"))
        baseline = len(self.slots())
        gate = fixture.fixture.threading.Event()
        server.start_gate = gate
        self.runtime.send(agent["id"], "Run while start is pending", "late-start-race")
        self.runtime.dispatch(agent["id"])
        fixture.fixture.eventually(lambda: sum(method == "turn/start" for method, _ in server.calls) == 2)
        self.runtime.delete_conversation(agent["id"])
        self.assert_deleted_and_released(agent["id"], baseline)
        gate.set()
        fixture.fixture.eventually(lambda: any(method == "turn/interrupt" for method, _ in server.calls))
        self.assert_deleted_and_released(agent["id"], baseline)
        self.assertTrue(server.active_turns.get(agent["threadId"]) is None)

    def test_unanswered_steer_reservation_is_released_and_restart_keeps_evidence(self):
        agent, baseline = self.running_chat()
        server = self.runtime.server
        first_turn = agent["turnId"]
        gate = fixture.fixture.threading.Event()
        server.start_gate = gate
        self.runtime.send(agent["id"], "Steer while busy", "late-steer-race", delivery="steer")
        self.runtime.dispatch(agent["id"])
        fixture.fixture.eventually(lambda: sum(method == "turn/start" for method, _ in server.calls) == 2)
        self.runtime.delete_conversation(agent["id"])
        self.assert_deleted_and_released(agent["id"], baseline)
        attempt = self.runtime.agent(agent["id"]).get("startAttempt") or {}
        self.assertFalse(attempt.get("activeAtReservation"))
        event_id = attempt["events"][0]
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id=?", (event_id,)).fetchone()[0],
                             "dispatching")
        gate.set()
        fixture.fixture.eventually(lambda: sum(method == "turn/interrupt" for method, _ in server.calls) >= 1)
        self.assert_deleted_and_released(agent["id"], baseline)
        self.runtime.close()
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.FakeServer)
        agent = self.runtime.agent(agent["id"])
        self.assertTrue(agent.get("deletedAt"))
        self.assertFalse(agent.get("inFlight"))
        self.assertNotIn("startAttempt", agent)
        self.assertEqual(len(self.slots()), baseline)
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id=?", (event_id,)).fetchone()[0],
                             "uncertain")

    def test_stopping_a_live_timed_out_turn_still_holds_its_slot(self):
        agent, baseline = self.running_chat()
        server = self.runtime.server
        original = server.call

        def call(method, params, timeout=60):
            if method == "turn/interrupt":
                raise RuntimeError("turn/interrupt response timed out; outcome unknown")
            return original(method, params, timeout)

        with patch.object(server, "call", side_effect=call):
            self.runtime.stop(agent["id"])
        current = self.runtime.agent(agent["id"])
        self.assertFalse(current.get("deletedAt"))
        self.assertTrue(current["inFlight"])
        self.assertEqual(current["turnId"], agent["turnId"])
        self.assertEqual(len(self.slots()), baseline + 1)

    def test_busy_steer_delete_releases_reserved_capacity(self):
        agent, baseline = self.running_chat()
        server = self.runtime.server
        gate = fixture.fixture.threading.Event()
        server.start_gate = gate
        self.runtime.send(agent["id"], "Steer before delete", "busy-steer-race", delivery="steer")
        self.runtime.dispatch(agent["id"])
        fixture.fixture.eventually(lambda: sum(method == "turn/start" for method, _ in server.calls) == 2)
        self.runtime.delete_conversation(agent["id"])
        self.assert_deleted_and_released(agent["id"], baseline)
        self.assertFalse((self.runtime.agent(agent["id"]).get("startAttempt") or {}).get("activeAtReservation"))
        gate.set()
        fixture.fixture.eventually(lambda: sum(method == "turn/interrupt" for method, _ in server.calls) >= 1)
        self.assert_deleted_and_released(agent["id"], baseline)

    def test_deleting_lead_settles_running_worker(self):
        lead = self.runtime.create({"name": "Lead", "cwd": self.temp.name, "prompt": "Lead", "maxAgents": 4})
        self.runtime.dispatch()
        fixture.fixture.eventually(lambda: self.runtime.agent(lead["id"]).get("turnId"))
        worker = self.runtime.create({"name": "Worker", "prompt": "Worker", "role": "reviewer"}, lead["id"], defer=True)
        baseline = 0
        self.runtime.send(worker["id"], "Run worker", "worker-run")
        self.runtime.dispatch()
        fixture.fixture.eventually(lambda: self.runtime.agent(lead["id"]).get("turnId")
                                   and self.runtime.agent(worker["id"]).get("turnId"))
        self.runtime.delete_conversation(lead["id"])
        self.assert_deleted_and_released(lead["id"], baseline)
        self.assert_deleted_and_released(worker["id"], baseline)

    def test_global_limit_slot_releases_after_three_deleted_turns(self):
        with patch.dict(fixture.fixture.os.environ, {"CODEX_CANVAS_CONCURRENCY": "3"}):
            running = [self.runtime.create({"name": f"Global {index}", "cwd": self.temp.name,
                "prompt": f"Run {index}"}, defer=True) for index in range(4)]
            for index, agent in enumerate(running):
                self.runtime.send(agent["id"], f"Run {index}", f"global-{index}")
            self.runtime.dispatch()
            fixture.fixture.eventually(lambda: sum(bool(self.runtime.agent(agent["id"]).get("turnId"))
                                                    for agent in running) == 3)
            self.assertFalse(self.runtime.agent(running[3]["id"]).get("turnId"))
            for agent in running[:3]:
                self.runtime.delete_conversation(agent["id"])
            self.assertEqual(len(self.slots()), 0)
            self.runtime.dispatch()
            fixture.fixture.eventually(lambda: self.runtime.agent(running[3]["id"]).get("turnId"))
            agent = self.runtime.agent(running[3]["id"])
            self.runtime.server.complete(agent["threadId"], agent["turnId"])
            fixture.fixture.eventually(lambda: not self.runtime.agent(agent["id"]).get("inFlight"))

    def test_deleted_worker_releases_per_root_limit_for_next_turn(self):
        root = self.runtime.create({"name": "Team", "cwd": self.temp.name, "prompt": "Team",
                                    "maxAgents": 5, "concurrency": 1})
        self.runtime.dispatch()
        fixture.fixture.eventually(lambda: self.runtime.agent(root["id"]).get("turnId"))
        worker1 = self.runtime.create({"name": "Worker one", "prompt": "One", "role": "reviewer"}, root["id"], defer=True)
        worker2 = self.runtime.create({"name": "Worker two", "prompt": "Two", "role": "reviewer"}, root["id"], defer=True)
        self.runtime.send(worker1["id"], "Run worker one", "root-one")
        self.runtime.dispatch()
        fixture.fixture.eventually(lambda: self.runtime.agent(worker1["id"]).get("turnId"))
        self.runtime.send(worker2["id"], "Run worker two", "root-two")
        self.runtime.dispatch()
        self.assertFalse(self.runtime.agent(worker2["id"]).get("turnId"))
        self.runtime.delete_conversation(worker1["id"])
        self.assertEqual([slot["id"] for slot in self.slots()], [root["id"]])
        self.runtime.dispatch()
        fixture.fixture.eventually(lambda: self.runtime.agent(worker2["id"]).get("turnId"))

    def test_idle_delete_and_repeated_delete_remain_safe(self):
        agent = self.runtime.create(
            {"name": "Idle delete", "cwd": self.temp.name, "prompt": "Unused task"}, defer=True
        )
        baseline = len(self.slots())
        sync_puts = []
        original_put = self.runtime.put

        def put(db, kind, record, **kwargs):
            if kind == "agents" and record["id"] == agent["id"] and kwargs.get("sync_rooms") is False:
                sync_puts.append(record.copy())
            return original_put(db, kind, record, **kwargs)

        with patch.object(self.runtime, "put", side_effect=put):
            self.assertEqual(self.runtime.delete_conversation(agent["id"])["deleted"], [agent["id"]])
            self.assertEqual(len(sync_puts), 1)  # Tombstone write only; settle skipped the unchanged row.
            self.assertEqual(self.runtime.delete_conversation(agent["id"])["deleted"], [agent["id"]])
        self.assertEqual(len(sync_puts), 2)  # A repeated delete adds no settle write.
        self.assert_deleted_and_released(agent["id"], baseline)

    def test_delete_settles_even_when_stop_raises(self):
        agent, baseline = self.running_chat()
        with patch.object(self.runtime, "stop", side_effect=RuntimeError("stop failed")):
            with self.assertRaisesRegex(RuntimeError, "stop failed"):
                self.runtime.delete_conversation(agent["id"])
        self.assert_deleted_and_released(agent["id"], baseline)


if __name__ == "__main__":
    unittest.main()

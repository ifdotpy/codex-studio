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
        self._start_futures = {}
        self._start_condition = fixture.fixture.threading.Condition()
        executor = self.runtime.delivery_executor()
        original_submit = executor.submit

        def track_start(function, *args, **kwargs):
            future = original_submit(function, *args, **kwargs)
            if getattr(function, "__name__", None) == "start" and args:
                agent_id = args[0]["id"]
                with self._start_condition:
                    self._start_futures.setdefault(agent_id, []).append(future)
                    self._start_condition.notify_all()
            return future

        submit_patch = patch.object(executor, "submit", side_effect=track_start)
        submit_patch.start()
        self.addCleanup(submit_patch.stop)
        self.addCleanup(lambda: self.runtime.close())

    def take_start(self, agent_id):
        with self._start_condition:
            while not self._start_futures.get(agent_id):
                self._start_condition.wait()
            return self._start_futures[agent_id].pop(0)

    def wait_start(self, agent_id):
        self.take_start(agent_id).result()

    def start_gate(self, server):
        entered = fixture.fixture.threading.Event()
        released = fixture.fixture.threading.Event()

        class Gate:
            def wait(self, timeout=None):
                entered.set()
                return released.wait()

            def set(self):
                released.set()

        server.start_gate = Gate()
        self.addCleanup(released.set)
        return entered, released

    def slots(self):
        with self.runtime.read_db() as db:
            return self.runtime.dispatch_active_slots(db)

    def attach_runtime_log(self):
        from codex_log_rotation import RotatingLog
        server = next(iter(self.runtime.servers.values()))
        server.log = RotatingLog(Path(self.temp.name) / "app-server.log")
        self.addCleanup(server.log.close)
        return server.log.path

    def running_chat(self):
        agent = self.runtime.create(
            {"name": "Delete while running", "cwd": self.temp.name, "prompt": "Start"},
            defer=True,
        )
        baseline = len(self.slots())
        self.runtime.send(agent["id"], "Run a turn", "delete-inflight-start")
        self.runtime.dispatch(agent["id"])
        self.wait_start(agent["id"])
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

    def test_tombstone_native_request_replies_do_not_create_pending_requests(self):
        agent, baseline = self.running_chat()
        self.runtime.delete_conversation(agent["id"])
        server = self.runtime.server
        requests = (
            ("item/fileChange/requestApproval", {"decision": "decline"}, False),
            ("item/permissions/requestApproval", {"permissions": {}, "scope": "turn"}, False),
            ("mcpServer/elicitation/request", {"action": "decline", "content": None}, False),
            ("item/tool/requestUserInput", None, True),
        )
        for index, (method, result, user_input) in enumerate(requests):
            request_id = f"late-request-{index}"
            server.request({"id": request_id, "method": method,
                "params": {"threadId": agent["threadId"], "turnId": agent["turnId"]}})
            response = server.responses[-1]
            self.assertEqual(response["id"], request_id)
            if user_input:
                self.assertIn("error", response)
                self.assertIn("unavailable", response["error"]["message"])
                self.assertNotIn("deleted", response["error"]["message"].lower())
            else:
                self.assertEqual(response["result"], result)
        self.assertEqual(self.runtime.agent(agent["id"])["status"], "paused")
        with self.runtime.db() as db:
            self.assertFalse(any(r.get("rpcId", "").startswith("late-request-")
                                 for r in self.runtime.records(db, "requests")))
        self.assertEqual(len(self.slots()), baseline)

    def test_archived_worker_late_request_is_declined_without_resurrection(self):
        root = self.runtime.create({"name": "Archive root", "cwd": self.temp.name,
                                    "prompt": "Root", "maxAgents": 4})
        self.runtime.dispatch()
        self.wait_start(root["id"])
        worker = self.runtime.create({"name": "Archived worker", "prompt": "Worker",
                                      "role": "reviewer"}, root["id"], defer=True)
        self.runtime.send(worker["id"], "Run worker", "archive-worker-run")
        self.runtime.dispatch()
        self.wait_start(worker["id"])
        archived = self.runtime.agent(worker["id"])
        now = fixture.fixture.time.time()
        with self.runtime.lock, self.runtime.db() as db:
            archived.update(deletedAt=now, autoWake=False, status="paused", epoch=archived["epoch"] + 1,
                            agentArchive={"at": now, "by": root["id"], "reason": "fixture archive",
                                          "epoch": archived["epoch"] + 1, "cleanupPending": False})
            self.runtime.put(db, "agents", archived)
        slots_before = len(self.slots())
        status_before = self.runtime.agent(worker["id"])["status"]
        self.runtime.server.request({"id": "archived-late", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": archived["threadId"], "turnId": archived["turnId"]}})
        self.assertEqual(self.runtime.server.responses[-1],
                         {"id": "archived-late", "result": {"decision": "decline"}})
        self.assertEqual(self.runtime.agent(worker["id"])["status"], status_before)
        self.assertEqual(len(self.slots()), slots_before)
        with self.runtime.db() as db:
            self.assertFalse(any(r.get("rpcId") == "archived-late"
                                 for r in self.runtime.records(db, "requests")))

    def test_delete_during_unanswered_start_interrupts_late_accepted_turn(self):
        agent, _ = self.running_chat()
        server = self.runtime.server
        server.complete(agent["threadId"], agent["turnId"])
        self.assertFalse(self.runtime.agent(agent["id"]).get("inFlight"))
        baseline = len(self.slots())
        entered, gate = self.start_gate(server)
        self.runtime.send(agent["id"], "Run while start is pending", "late-start-race")
        self.runtime.dispatch(agent["id"])
        start_future = self.take_start(agent["id"])
        entered.wait()
        self.assertEqual(sum(method == "turn/start" for method, _ in server.calls), 2)
        self.runtime.delete_conversation(agent["id"])
        self.assert_deleted_and_released(agent["id"], baseline)
        gate.set()
        start_future.result()
        self.assertTrue(any(method == "turn/interrupt" for method, _ in server.calls))
        accepted_turn = next(params["turnId"] for method, params in server.calls
                             if method == "turn/interrupt")
        self.assertEqual(sum(method == "turn/interrupt" and params["turnId"] == accepted_turn
                             for method, params in server.calls), 1)
        self.assert_deleted_and_released(agent["id"], baseline)
        self.assertTrue(server.active_turns.get(agent["threadId"]) is None)

    def test_delete_during_start_records_failed_late_interrupt(self):
        agent, _ = self.running_chat()
        server = self.runtime.server
        log_path = self.attach_runtime_log()
        server.complete(agent["threadId"], agent["turnId"])
        self.assertFalse(self.runtime.agent(agent["id"]).get("inFlight"))
        baseline = len(self.slots())
        entered, gate = self.start_gate(server)
        start_count = sum(method == "turn/start" for method, _ in server.calls)
        self.runtime.send(agent["id"], "Run while start is pending", "late-failed-interrupt")
        self.runtime.dispatch(agent["id"])
        start_future = self.take_start(agent["id"])
        entered.wait()
        self.assertGreater(sum(method == "turn/start" for method, _ in server.calls), start_count)
        self.runtime.delete_conversation(agent["id"])
        self.assert_deleted_and_released(agent["id"], baseline)
        original = server.call
        interrupt_calls = []

        def fail_interrupt(method, params, timeout=60):
            if method == "turn/interrupt":
                interrupt_calls.append(params.copy())
                raise RuntimeError("interrupt acknowledgement lost")
            return original(method, params, timeout)

        with patch.object(server, "call", side_effect=fail_interrupt):
            gate.set()
            start_future.result()
        self.assertTrue(interrupt_calls)
        self.assertTrue(self.runtime.agent(agent["id"]).get("error"))
        accepted_turn = interrupt_calls[0]["turnId"]
        self.assertEqual(sum(call["turnId"] == accepted_turn for call in interrupt_calls), 1)
        current = self.runtime.agent(agent["id"])
        self.assertTrue(current.get("deletedAt"))
        self.assertFalse(current.get("inFlight"))
        self.assertIsNone(current.get("turnId"))
        self.assertIn("interrupt acknowledgement unavailable", current["error"])
        self.assertEqual(len(self.slots()), baseline)
        diagnostic = log_path.read_text()
        self.assertIn("deleted_turn_interrupt_unconfirmed", diagnostic)

    def test_failed_start_after_delete_leaves_tombstone_settled(self):
        agent, _ = self.running_chat()
        server = self.runtime.server
        server.complete(agent["threadId"], agent["turnId"])
        self.assertFalse(self.runtime.agent(agent["id"]).get("inFlight"))
        baseline = len(self.slots())
        entered, gate = self.start_gate(server)
        start_count = sum(method == "turn/start" for method, _ in server.calls)
        self.runtime.send(agent["id"], "Run while start is pending", "late-failed-start")
        self.runtime.dispatch(agent["id"])
        start_future = self.take_start(agent["id"])
        entered.wait()
        self.assertGreater(sum(method == "turn/start" for method, _ in server.calls), start_count)
        self.runtime.delete_conversation(agent["id"])
        self.assert_deleted_and_released(agent["id"], baseline)
        server.fail_start = True
        failed = fixture.fixture.threading.Event()
        original = self.runtime.start_error

        def record_failure(*args, **kwargs):
            original(*args, **kwargs)
            failed.set()

        with patch.object(self.runtime, "start_error", side_effect=record_failure):
            gate.set()
            start_future.result()
            self.assertTrue(failed.is_set(), "late turn/start failure callback did not run")
        self.assert_deleted_and_released(agent["id"], baseline)

    def test_unanswered_steer_reservation_is_released_and_restart_keeps_evidence(self):
        agent, baseline = self.running_chat()
        server = self.runtime.server
        first_turn = agent["turnId"]
        entered, gate = self.start_gate(server)
        self.runtime.send(agent["id"], "Steer while busy", "late-steer-race", delivery="steer")
        self.runtime.dispatch(agent["id"])
        start_future = self.take_start(agent["id"])
        entered.wait()
        self.assertEqual(sum(method == "turn/start" for method, _ in server.calls), 2)
        self.runtime.delete_conversation(agent["id"])
        self.assert_deleted_and_released(agent["id"], baseline)
        attempt = self.runtime.agent(agent["id"]).get("startAttempt") or {}
        self.assertFalse(attempt.get("activeAtReservation"))
        event_id = attempt["events"][0]
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id=?", (event_id,)).fetchone()[0],
                             "dispatching")
        gate.set()
        start_future.result()
        self.assertGreaterEqual(sum(method == "turn/interrupt" for method, _ in server.calls), 1)
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
        entered, gate = self.start_gate(server)
        self.runtime.send(agent["id"], "Steer before delete", "busy-steer-race", delivery="steer")
        self.runtime.dispatch(agent["id"])
        start_future = self.take_start(agent["id"])
        entered.wait()
        self.assertEqual(sum(method == "turn/start" for method, _ in server.calls), 2)
        self.runtime.delete_conversation(agent["id"])
        self.assert_deleted_and_released(agent["id"], baseline)
        self.assertFalse((self.runtime.agent(agent["id"]).get("startAttempt") or {}).get("activeAtReservation"))
        gate.set()
        start_future.result()
        self.assertGreaterEqual(sum(method == "turn/interrupt" for method, _ in server.calls), 1)
        self.assert_deleted_and_released(agent["id"], baseline)

    def test_deleting_lead_settles_running_worker(self):
        lead = self.runtime.create({"name": "Lead", "cwd": self.temp.name, "prompt": "Lead", "maxAgents": 4})
        self.runtime.dispatch()
        self.wait_start(lead["id"])
        worker = self.runtime.create({"name": "Worker", "prompt": "Worker", "role": "reviewer"}, lead["id"], defer=True)
        baseline = 0
        self.runtime.send(worker["id"], "Run worker", "worker-run")
        self.runtime.dispatch()
        self.wait_start(worker["id"])
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
            for agent in running[:3]:
                self.wait_start(agent["id"])
            self.assertEqual(sum(bool(self.runtime.agent(agent["id"]).get("turnId"))
                                 for agent in running), 3)
            self.assertFalse(self.runtime.agent(running[3]["id"]).get("turnId"))
            for agent in running[:3]:
                self.runtime.delete_conversation(agent["id"])
            self.assertEqual(len(self.slots()), 0)
            self.runtime.dispatch()
            self.wait_start(running[3]["id"])
            agent = self.runtime.agent(running[3]["id"])
            self.runtime.server.complete(agent["threadId"], agent["turnId"])
            self.assertFalse(self.runtime.agent(agent["id"]).get("inFlight"))

    def test_deleted_worker_releases_per_root_limit_for_next_turn(self):
        root = self.runtime.create({"name": "Team", "cwd": self.temp.name, "prompt": "Team",
                                    "maxAgents": 5, "concurrency": 1})
        self.runtime.dispatch()
        self.wait_start(root["id"])
        worker1 = self.runtime.create({"name": "Worker one", "prompt": "One", "role": "reviewer"}, root["id"], defer=True)
        worker2 = self.runtime.create({"name": "Worker two", "prompt": "Two", "role": "reviewer"}, root["id"], defer=True)
        self.runtime.send(worker1["id"], "Run worker one", "root-one")
        self.runtime.dispatch()
        self.wait_start(worker1["id"])
        self.runtime.send(worker2["id"], "Run worker two", "root-two")
        self.runtime.dispatch()
        self.assertFalse(self.runtime.agent(worker2["id"]).get("turnId"))
        self.runtime.delete_conversation(worker1["id"])
        self.assertEqual([slot["id"] for slot in self.slots()], [root["id"]])
        self.runtime.dispatch()
        self.wait_start(worker2["id"])

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

    def test_settle_failure_is_logged_without_replacing_stop_exception(self):
        agent, baseline = self.running_chat()
        log_path = self.attach_runtime_log()
        original = self.runtime.put

        def fail_settle(db, kind, record, **kwargs):
            if (kind == "agents" and record["id"] == agent["id"] and record.get("deletedAt")
                    and not record.get("inFlight") and kwargs.get("sync_rooms") is False):
                raise RuntimeError("settle storage failed")
            return original(db, kind, record, **kwargs)

        with patch.object(self.runtime, "stop", side_effect=RuntimeError("original stop failure")), \
             patch.object(self.runtime, "put", side_effect=fail_settle):
            with self.assertRaisesRegex(RuntimeError, "original stop failure"):
                self.runtime.delete_conversation(agent["id"])
        self.assertTrue(self.runtime.agent(agent["id"]).get("deletedAt"))
        self.assertTrue(self.runtime.agent(agent["id"]).get("inFlight"))
        self.assertEqual(len(self.slots()), baseline + 1)
        diagnostic = log_path.read_text()
        self.assertIn("deleted_conversation_settle_failed", diagnostic)


if __name__ == "__main__":
    unittest.main()

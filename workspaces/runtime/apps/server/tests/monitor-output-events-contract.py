#!/usr/bin/env python3
"""Literal monitor output matching, bounded waits, and durable wake identities."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import base64
import importlib.util
import json
from pathlib import Path
import sys
import threading
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT
ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
spec = importlib.util.spec_from_file_location(
    "monitor_fixture", SERVER_TESTS_ROOT / "monitor-lifecycle-contract.py")
fixture = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = fixture
spec.loader.exec_module(fixture)


from settlement_contract_support import settlement_outcome


def request_result_outcome(record, result):
    return settlement_outcome(record["tool"], result, read_only=record.get("readOnly", False))


class MonitorOutputEventsContract(unittest.TestCase):
    setUp = fixture.MonitorLifecycleContract.setUp
    tearDown = fixture.MonitorLifecycleContract.tearDown
    monitor = fixture.MonitorLifecycleContract.monitor
    record = fixture.MonitorLifecycleContract.record

    def eventually(self, condition, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if condition():
                return
            time.sleep(0.02)
        self.assertTrue(condition(), "Condition did not become true")

    def output(self, key, value, stream="stdout", *, supervisor_handle=None, supervisor_sequence=None):
        self.eventually(lambda: bool(self.record(key).get("operation")))
        record = self.record(key)
        operation = record["operation"]
        self.runtime.output({
            "processId": key,
            "stream": stream,
            "deltaBase64": base64.b64encode(value).decode("ascii"),
        }, account_key=operation["accountKey"], connection_id=operation["connectionId"],
            supervisor_handle=supervisor_handle, supervisor_sequence=supervisor_sequence)

    def output_events(self, key):
        with self.runtime.db() as db:
            return [dict(row) for row in db.execute(
                "SELECT * FROM runtime_events WHERE id LIKE ? ORDER BY id",
                (f"monitor-output:{key}:%",),
            )]

    def test_literal_patterns_cross_chunks_per_stream_and_utf8(self):
        key = self.monitor(output_patterns=["ERROR", "aba", "é"])
        self.output(key, b"ER", "stdout")
        self.output(key, b"ROR", "stderr")
        self.output(key, b"ROR ababa", "stdout")
        with self.runtime.lock, self.runtime.db() as db:
            record = self.record(key)
            record["outputWakeAt"] = time.time() + 100
            self.runtime.put(db, "monitors", record)
        self.output(key, b"\xc3", "stdout")
        self.output(key, b"\xa9", "stdout")

        record = self.record(key)
        totals = record["outputMatchesTotal"]
        self.assertEqual(totals, {"stdout:0": 1, "stdout:1": 2, "stdout:2": 1})
        self.assertEqual(record["tail"][-6:], "ababaé")
        self.assertEqual(len(self.output_events(key)), 1)

        log_path = Path(record["log"])
        self.assertEqual(log_path.read_bytes(), b"ERRORROR ababa\xc3\xa9")
        self.server.finish(key)
        self.eventually(lambda: self.record(key)["status"] == "completed")
        events = self.output_events(key)
        self.assertEqual(len(events), 2)
        ids = [event["id"] for event in events]
        self.assertEqual(ids, [f"monitor-output:{key}:{record['epoch']}:1",
                               f"monitor-output:{key}:{record['epoch']}:2"])
        with self.runtime.db() as db:
            exit_event = db.execute(
                "SELECT text FROM runtime_events WHERE id=?", ("monitor:" + key,)
            ).fetchone()
        summary = json.loads(exit_event[0])["outputMatchSummary"]
        self.assertEqual(summary["matches"], [
            {"stream": "stdout", "pattern": "ERROR", "count": 1},
            {"stream": "stdout", "pattern": "aba", "count": 2},
            {"stream": "stdout", "pattern": "é", "count": 1},
        ])

    def test_cooldown_coalesces_repeats_and_renews_after_many_events(self):
        key = self.monitor(output_patterns=["X"])
        self.output(key, b"X")
        with self.runtime.lock, self.runtime.db() as db:
            record = self.record(key)
            record["outputWakeAt"] = time.time() - 2
            self.runtime.put(db, "monitors", record)
        self.output(key, b"XXXX")
        events = self.output_events(key)
        self.assertEqual(len(events), 2)
        self.assertEqual(json.loads(events[-1]["text"])["matches"], [
            {"stream": "stdout", "pattern": "X", "count": 4},
        ])

        with self.runtime.lock, self.runtime.db() as db:
            record = self.record(key)
            record["outputEventCount"] = 31
            record["outputEventSequence"] = 31
            record["outputWakeAt"] = time.time() - 2
            self.runtime.put(db, "monitors", record)
        for _ in range(33):
            with self.runtime.lock, self.runtime.db() as db:
                record = self.record(key)
                record["outputWakeAt"] = time.time() - 2
                self.runtime.put(db, "monitors", record)
            self.output(key, b"X")
        record = self.record(key)
        self.assertEqual(record["outputEventCount"], 64)
        self.assertEqual(record["outputEventSequence"], 64)
        self.assertEqual(len(record["outputEventIds"]), 32)
        self.assertTrue(record["outputEventIds"][-1].endswith(":64"))
        self.assertEqual(len(self.output_events(key)), 35)
        self.server.finish(key)
        self.eventually(lambda: self.record(key)["status"] == "completed")
        self.assertEqual(len(self.output_events(key)), 35)

    def test_default_has_no_output_wakes_and_event_replay_is_deduplicated(self):
        key = self.monitor()
        self.output(key, b"ERROR")
        self.assertEqual(self.output_events(key), [])

        patterned = self.monitor(output_patterns=["ERROR"])
        self.output(patterned, b"ERROR")
        event = self.output_events(patterned)[0]
        with self.runtime.lock, self.runtime.db() as db:
            monitor = self.record(patterned)
            agent = self.runtime.agent(monitor["agent"], db)
            payload = json.loads(event["text"])
            self.runtime._enqueue_monitor_output_event(
                db, agent, monitor, {"id": event["id"], "payload": payload})
        self.assertEqual(len(self.output_events(patterned)), 1)

        root = self.runtime.root
        self.runtime.close()
        self.runtime = fixture.Runtime(root, fixture.MonitorServer)
        with self.runtime.lock, self.runtime.db() as db:
            monitor = self.record(patterned)
            agent = self.runtime.agent(monitor["agent"], db)
            self.runtime._enqueue_monitor_output_event(
                db, agent, monitor,
                {"id": event["id"], "payload": json.loads(event["text"])})
        self.assertEqual(len(self.output_events(patterned)), 1)

    def test_wait_observation_reuses_monitor_and_does_not_launch(self):
        key = self.monitor(output_patterns=["READY"])
        commands_before = sum(method == "command/exec" for method, _ in self.server.calls)
        result = self.runtime.monitor(
            self.agent["id"], {"monitor_id": key, "wait_timeout_ms": 30},
            key="observe-call", epoch=self.agent["epoch"], account_key="default")
        self.assertTrue(result["waitExpired"])
        self.assertEqual(result["id"], key)
        self.assertEqual(result["status"], "running")
        commands_after = sum(method == "command/exec" for method, _ in self.server.calls)
        self.assertEqual(commands_after, commands_before)

    def test_supervisor_cursor_commits_with_output_before_ack_replay(self):
        key = self.monitor(output_patterns=["ERROR"])
        handle = "fixture-monitor-output-supervisor"
        self.output(key, b"ERROR", supervisor_handle=handle, supervisor_sequence=7)
        committed = self.record(key)
        log_size = Path(committed["log"]).stat().st_size
        # Replay after the output transaction commits but before the event cursor ACK.
        self.output(key, b"ERROR", supervisor_handle=handle, supervisor_sequence=7)
        replayed = self.record(key)
        with self.runtime.read_db() as db:
            cursor = db.execute("SELECT sequence FROM runtime_supervisor_cursor WHERE handle=?", (handle,)).fetchone()
        self.assertEqual(cursor[0], 7)
        self.assertEqual(replayed["bytes"], committed["bytes"])
        self.assertEqual(replayed["outputMatchesTotal"], committed["outputMatchesTotal"])
        self.assertEqual(Path(replayed["log"]).stat().st_size, log_size)
        self.assertEqual(len(self.output_events(key)), 1)

    def test_recovered_terminal_flushes_partial_output_and_wakes_waiter(self):
        from codex_monitor_recovery import persist_monitor_result, recover_monitor_results

        key = self.monitor(output_patterns=["READY"])
        self.output(key, b"READY")
        with self.runtime.lock, self.runtime.db() as db:
            monitor = self.record(key)
            monitor["outputWakeAt"] = time.time() + 100
            self.runtime.put(db, "monitors", monitor)
        self.output(key, b"READY")
        self.output(key, b"\xc3")
        waiter_result = []
        waiter = threading.Thread(target=lambda: waiter_result.append(self.runtime.observe_monitor(
            self.agent["id"], key, epoch=self.agent["epoch"], wait_timeout_ms=3000,
            account_key="default", after_sequence=1)))
        waiter.start()
        operation = self.record(key)["operation"]
        persist_monitor_result(self.runtime.root, key, {
            "code": 0, "error": None, "operation": operation, "finished": time.time()})
        with self.runtime.monitor_condition, self.runtime.db() as db:
            recovered = recover_monitor_results(self.runtime, db, keys=[key])
        waiter.join(1)
        self.assertFalse(waiter.is_alive(), "Recovered terminal output did not wake the observer")
        self.assertIn(key, recovered["restored"])
        self.assertEqual(waiter_result[0]["status"], "completed")
        self.assertTrue(waiter_result[0]["outputEvents"][-1]["final"])
        self.assertTrue(self.record(key)["tail"].endswith("�"))

    def test_expired_wait_deadline_rejects_launch_without_reserving_monitor(self):
        before = sum(method == "command/exec" for method, _ in self.server.calls)
        with self.assertRaisesRegex(ValueError, "expired before launch"):
            self.runtime.monitor(
                self.agent["id"], {"command": "must-not-run", "wait_timeout_ms": 100},
                key="expired-wait-deadline", approved=True, epoch=self.agent["epoch"],
                account_key="default", wait_deadline=time.monotonic() - 1)
        after = sum(method == "command/exec" for method, _ in self.server.calls)
        with self.runtime.db() as db:
            row = db.execute("SELECT 1 FROM runtime_monitors WHERE json_extract(record,'$.requestId')=?", ("expired-wait-deadline",)).fetchone()
        self.assertEqual(before, after)
        self.assertIsNone(row)

    def test_expired_observation_keeps_an_existing_launch_identity(self):
        args = {"command": "existing-command"}
        first = self.runtime.monitor(self.agent["id"], args, key="existing-launch",
                                     approved=False, epoch=self.agent["epoch"])
        replay = self.runtime.monitor(
            self.agent["id"], {**args, "wait_timeout_ms": 100}, key="existing-launch",
            approved=False, epoch=self.agent["epoch"], account_key="default",
            wait_deadline=time.monotonic() - 1)
        self.assertEqual(replay["id"], first["id"])
        self.assertEqual(replay["status"], "approval")
        self.assertTrue(replay["waitExpired"])
        self.assertEqual(sum(method == "command/exec" for method, _ in self.server.calls), 0)

    def test_wait_budget_expiring_on_runtime_lock_does_not_launch(self):
        deadline = time.monotonic() + 0.05
        errors = []
        self.runtime.lock.acquire()
        worker = threading.Thread(target=lambda: self._capture_monitor_error(
            errors, deadline, "expired-while-lock-held"))
        worker.start()
        time.sleep(0.1)
        self.runtime.lock.release()
        worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(errors), 1)
        self.assertIn("expired before launch", str(errors[0]))
        with self.runtime.db() as db:
            row = db.execute("SELECT 1 FROM runtime_monitors WHERE json_extract(record,'$.requestId')=?", ("expired-while-lock-held",)).fetchone()
        self.assertIsNone(row)
        self.assertEqual(sum(method == "command/exec" for method, _ in self.server.calls), 0)

    def _capture_monitor_error(self, errors, deadline, key):
        try:
            self.runtime.monitor(
                self.agent["id"], {"command": "must-not-run", "wait_timeout_ms": 100},
                key=key, approved=True, epoch=self.agent["epoch"], account_key="default",
                wait_deadline=deadline)
        except Exception as error:
            errors.append(error)

    def test_wait_pool_saturation_rejects_without_queueing_and_keeps_runtime_responsive(self):
        self.runtime.monitor_wait_slots = threading.BoundedSemaphore(0)
        message = {"id": "saturated-wait-rpc", "method": "item/tool/call", "params": {
            "threadId": self.agent["threadId"], "callId": "saturated-wait-call",
            "tool": "orchestration_monitor",
            "arguments": {"command": "must-not-run", "wait_timeout_ms": 1000}}}
        connection_id = self.runtime.connection_ids["default"]
        self.runtime.request(message, "default", connection_id)
        started = time.monotonic()
        self.runtime.agent(self.agent["id"])
        self.assertLess(time.monotonic() - started, 0.25)
        self.assertEqual(sum(method == "command/exec" for method, _ in self.server.calls), 0)
        self.assertEqual(len(self.server.responses), 1)
        outcome = request_result_outcome(
            {"tool": "orchestration_monitor", "readOnly": False}, self.server.responses[0]["result"])
        self.assertEqual(outcome, "not_applied")

    def test_launch_receipt_survives_owner_epoch_change_during_wait(self):
        original_observe = self.runtime.observe_monitor

        def change_epoch_then_observe(*args, **kwargs):
            self.assertTrue(self.server.command_wait_entered.wait(3))
            with self.runtime.lock, self.runtime.db() as db:
                agent = self.runtime.agent(self.agent["id"], db)
                agent["epoch"] += 1
                self.runtime.put(db, "agents", agent)
            return original_observe(*args, **kwargs)

        self.runtime.observe_monitor = change_epoch_then_observe
        receipt = self.runtime.monitor(
            self.agent["id"],
            {"command": "fixture-command", "timeout_ms": 1000, "wait_timeout_ms": 1000},
            key="launch-owner-change-during-wait", approved=True,
            epoch=self.agent["epoch"], account_key="default")
        self.assertEqual(receipt["status"], "unknown")
        self.assertTrue(receipt["id"])
        self.assertIn("observationError", receipt)
        self.assertEqual(request_result_outcome(
            {"tool": "orchestration_monitor", "readOnly": False},
            {"success": True}), "applied")
        self.assertEqual(sum(method == "command/exec" for method, _ in self.server.calls), 1)

    def test_wait_releases_runtime_lock_and_stale_epoch_fails_closed(self):
        key = self.monitor(output_patterns=["READY"])
        waiting = threading.Event()
        result = []

        def observe():
            waiting.set()
            result.append(self.runtime.observe_monitor(
                self.agent["id"], key, epoch=self.agent["epoch"],
                wait_timeout_ms=500, account_key="default"))

        worker = threading.Thread(target=observe)
        worker.start()
        self.assertTrue(waiting.wait(1))
        time.sleep(0.03)
        started = time.monotonic()
        self.runtime.agent(self.agent["id"])
        self.assertLess(time.monotonic() - started, 0.2)
        worker.join(2)
        self.assertFalse(worker.is_alive())
        self.assertTrue(result[0]["waitExpired"])
        with self.assertRaisesRegex(ValueError, "agent or epoch"):
            self.runtime.observe_monitor(
                self.agent["id"], key, epoch=self.agent["epoch"] + 1,
                wait_timeout_ms=0, account_key="default")
        with self.assertRaisesRegex(ValueError, "another agent or epoch"):
            self.runtime.observe_monitor(
                "different-owner", key, epoch=self.agent["epoch"],
                wait_timeout_ms=0, account_key="default")

    def test_wait_duration_is_not_launch_identity_but_patterns_are(self):
        request_id = "stable-monitor-wait-test"
        args = {"command": "fixture-command", "timeout_ms": 1000,
                "output_patterns": ["READY"]}
        first = self.runtime.monitor(
            self.agent["id"], args, key=request_id, approved=True,
            epoch=self.agent["epoch"], account_key="default")
        self.assertTrue(self.server.command_wait_entered.wait(3))
        second = self.runtime.monitor(
            self.agent["id"], {**args, "wait_timeout_ms": 20}, key=request_id,
            approved=True, epoch=self.agent["epoch"], account_key="default")
        self.assertEqual(first["id"], second["id"])
        self.assertTrue(second["waitExpired"])
        self.assertEqual(sum(method == "command/exec" for method, _ in self.server.calls), 1)
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.monitor(
                self.agent["id"], {**args, "output_patterns": ["DONE"]},
                key=request_id, approved=True, epoch=self.agent["epoch"], account_key="default")

    def test_invalid_observation_and_unbounded_patterns_fail(self):
        key = self.monitor(output_patterns=["READY"])
        for args in ({"monitor_id": key, "command": "true"}, {"wait_timeout_ms": 0}):
            with self.assertRaisesRegex(ValueError, "exactly one"):
                self.runtime.monitor(self.agent["id"], args, epoch=self.agent["epoch"])
        for args in ({"monitor_id": key, "output_patterns": ["READY"]},
                     {"monitor_id": key, "timeout_ms": 1000}):
            with self.assertRaisesRegex(ValueError, "observation accepts"):
                self.runtime.monitor(self.agent["id"], args, epoch=self.agent["epoch"])
        with self.assertRaisesRegex(ValueError, "wait_timeout_ms"):
            self.runtime.monitor(
                self.agent["id"], {"monitor_id": key, "wait_timeout_ms": 10001},
                epoch=self.agent["epoch"])
        for patterns in ([""], ["é" * 129], ["x"] * 17, ["x", "x"]):
            with self.assertRaises(ValueError):
                self.runtime.monitor(
                    self.agent["id"], {"command": "fixture-command", "output_patterns": patterns},
                    epoch=self.agent["epoch"])

    def test_tool_schema_exposes_exclusive_launch_and_observation_fields(self):
        from codex_runtime import TOOLS
        schema = next(tool["inputSchema"] for tool in TOOLS
                      if tool["name"] == "orchestration_monitor")
        self.assertEqual(schema["properties"]["wait_timeout_ms"], {
            "type": "integer", "minimum": 0, "maximum": 10000})
        self.assertEqual(schema["properties"]["output_patterns"]["maxItems"], 16)
        self.assertEqual(schema["properties"]["monitor_id"]["maxLength"], 200)
        self.assertNotIn("oneOf", schema)

    def test_every_tool_schema_stays_a_plain_object_for_provider_bridges(self):
        # The Claude bridge converts each schema with Zod and registers its
        # object shape. Zod rejects "not", and a top-level combinator has no shape.
        from codex_runtime import TOOLS

        def keywords(value):
            if isinstance(value, dict):
                yield from value
                for child in value.values():
                    yield from keywords(child)
            elif isinstance(value, list):
                for child in value:
                    yield from keywords(child)

        for tool in TOOLS:
            schema = tool["inputSchema"]
            with self.subTest(tool=tool["name"]):
                self.assertEqual(schema["type"], "object")
                self.assertLessEqual(
                    set(schema), {"type", "properties", "required", "additionalProperties"})
                self.assertNotIn("not", set(keywords(schema["properties"])))

    def test_preflight_rejections_are_saved_as_not_applied(self):
        from codex_tool_requests import request_read_only
        self.assertTrue(request_read_only("orchestration_monitor", {"monitor_id": "existing"}))
        self.assertFalse(request_read_only("orchestration_monitor", {"command": "true"}))
        record = {"tool": "orchestration_monitor", "readOnly": False}
        for message in ("wait_timeout_ms must be 0 to 10000",
                        "Supply exactly one of command or monitor_id",
                        "Each output pattern must be at most 256 UTF-8 bytes"):
            result = {"success": False, "contentItems": [{"type": "inputText", "text": message}]}
            self.assertEqual(request_result_outcome(record, result), "not_applied")
        epoch_error = {"success": False, "contentItems": [{
            "type": "inputText", "text": "The monitor owner epoch is no longer active"}]}
        self.assertEqual(request_result_outcome(
            {"tool": "orchestration_monitor", "readOnly": True}, epoch_error), "not_applied")
        self.assertEqual(request_result_outcome(record, epoch_error), "unknown")


if __name__ == "__main__":
    unittest.main(verbosity=2)

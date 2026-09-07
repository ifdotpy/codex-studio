#!/usr/bin/env python3
"""Panel feeds use command watches without model events or unbounded input."""

import base64
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
import uuid

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_panel_feed import MAX_FRAME_BYTES, PanelFeedConsumer
from codex_runtime import Runtime

spec = importlib.util.spec_from_file_location("runtime_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
eventually = fixture.eventually


class ConsumerContract(unittest.TestCase):
    def setUp(self):
        self.updates, self.statuses = [], []
        self.consumer = PanelFeedConsumer(
            lambda state, sequence: self.updates.append((state, sequence, time.monotonic())),
            lambda status, error, sequence: self.statuses.append((status, error, sequence)),
            interval_ms=60,
        )

    def tearDown(self):
        self.consumer.close()
        self.consumer.worker.join(2)
        self.assertFalse(self.consumer.worker.is_alive())

    def test_split_utf8_and_coalesced_frames(self):
        raw = '{"label":"Кипр"}\n'.encode()
        cut = raw.index('К'.encode()) + 1
        self.consumer.feed(raw[:cut])
        self.assertEqual(self.updates, [])
        self.consumer.feed(raw[cut:])
        eventually(lambda: len(self.updates) == 1)
        # All frames in one chunk replace the same bounded pending slot.
        self.consumer.feed(b'{"n":1}\n{"n":2}\n{"n":3}\n')
        eventually(lambda: len(self.updates) == 2)
        self.assertEqual(self.updates[0][0], {"label": "Кипр"})
        self.assertEqual(self.updates[1][:2], ({"n": 3}, 4))
        self.assertGreaterEqual(self.updates[1][2] - self.updates[0][2], .05)

    def test_slow_validation_does_not_block_output_or_accumulate_frames(self):
        entered, release = threading.Event(), threading.Event()
        def update(state, sequence):
            entered.set()
            release.wait(2)
            self.updates.append((state, sequence))
        self.consumer.update = update
        self.consumer.feed(b'{"n":0}\n')
        self.assertTrue(entered.wait(1))
        start = time.monotonic()
        for n in range(1, 1001):
            self.consumer.feed(json.dumps({"n": n}).encode() + b'\n')
        self.assertLess(time.monotonic() - start, .5)
        self.assertLessEqual(len(self.consumer.pending[1]), MAX_FRAME_BYTES)
        release.set()
        eventually(lambda: len(self.updates) == 2)
        self.assertEqual(self.updates[-1][0], {"n": 1000})

    def test_oversize_invalid_json_and_recovery(self):
        self.consumer.feed(b'x' * (MAX_FRAME_BYTES * 10))
        self.assertLessEqual(len(self.consumer.buffer), MAX_FRAME_BYTES)
        self.consumer.feed(b'\n')
        eventually(lambda: len(self.statuses) == 1)
        self.assertIn("exceeds", self.statuses[-1][1])
        for value in (b'[]\n', b'{"n":NaN}\n', b'broken\n', b'{"n":"\xff"}\n'):
            count = len(self.statuses)
            self.consumer.feed(value)
            eventually(lambda: len(self.statuses) > count)
        self.consumer.feed(b'{"valid":true}\n')
        eventually(lambda: len(self.updates) == 1)
        self.assertEqual(self.updates[-1][0], {"valid": True})

    def test_completion_flushes_latest_but_not_partial_frame(self):
        self.consumer.feed(b'{"n":1}\n{"n":')
        self.consumer.finish("completed")
        self.consumer.worker.join(2)
        self.assertEqual(self.updates[0][0], {"n": 1})
        self.assertEqual(self.statuses[-1][0], "completed")
        self.assertIn("incomplete", self.statuses[-1][1])

    def test_final_rejection_remains_visible_after_process_exit(self):
        self.consumer.feed(b"broken\n")
        self.consumer.finish("completed")
        self.consumer.worker.join(2)
        self.assertEqual(self.statuses[-1][0], "completed")
        self.assertTrue(self.statuses[-1][1])
        self.assertEqual(self.updates, [])

    def test_cancel_drops_pending_and_rejects_late_output(self):
        self.consumer.feed(b'{"n":1}\n')
        eventually(lambda: len(self.updates) == 1)
        self.consumer.feed(b'{"n":2}\n')
        self.consumer.suspend()
        self.consumer.feed(b'{"n":3}\n')
        self.consumer.finish("cancelled", discard=True)
        self.consumer.worker.join(2)
        self.assertEqual(len(self.updates), 1)
        self.assertEqual(self.statuses[-1][0], "cancelled")

    def test_validation_error_preserves_stream(self):
        def reject(state, sequence):
            if not state.get("fits"):
                raise ValueError("Panel content overflows 150px")
            self.updates.append((state, sequence))
        self.consumer.update = reject
        self.consumer.feed(b'{"fits":false}\n')
        eventually(lambda: len(self.statuses) == 1)
        self.assertIn("overflows", self.statuses[0][1])
        self.consumer.feed(b'{"fits":true}\n')
        eventually(lambda: len(self.updates) == 1)


class FeedServer(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.command_gates = {}

    def call(self, method, params, timeout=60):
        if method == "command/exec":
            self.calls.append((method, params))
            gate = self.command_gates.setdefault(params["processId"], threading.Event())
            gate.wait(8)
            return {"exitCode": 0}
        if method == "command/exec/terminate":
            self.calls.append((method, params))
            self.command_gates[params["processId"]].set()
            return {}
        return super().call(method, params, timeout)

    def close(self):
        for gate in self.command_gates.values():
            gate.set()
        super().close()


class FeedRuntime(Runtime):
    def __init__(self, *args):
        self.feed_updates, self.feed_statuses = [], []
        super().__init__(*args)

    def panel_feed_update(self, *args):
        self.feed_updates.append(args)

    def panel_feed_status(self, *args, **kwargs):
        self.feed_statuses.append((args, kwargs))


class MonitorContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.runtime = FeedRuntime(self.tmp.name, FeedServer)
        self.agent = self.runtime.create({"name": "Lead", "cwd": self.tmp.name, "prompt": "Test feed", "yolo_mode": True})
        eventually(lambda: self.runtime.agent(self.agent["id"])["status"] == "running")
        self.agent = self.runtime.agent(self.agent["id"])
        self.binding = {"panelVersion": 1, "statePath": "/live", "intervalMs": 1000}

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def reserve(self, request):
        key = str(uuid.uuid5(uuid.NAMESPACE_URL, request))
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "panels", {
                "id": self.agent["id"], "agent": self.agent["id"], "version": self.binding["panelVersion"],
                "format": "json-render", "spec": {"state": {"live": {}}}, "callbacks": [],
                "feed": {"monitorId": key, "epoch": self.agent["epoch"], "status": "starting", "statePath": self.binding["statePath"]},
            })
        return key

    def start(self, **options):
        options.setdefault("key", str(uuid.uuid4()))
        self.reserve(options["key"])
        result = self.runtime.monitor(self.agent["id"], {"command": "python3 observe.py", "timeout_ms": 5000},
                                      approved=True, panel_feed=self.binding, **options)
        eventually(lambda: result["id"] in self.runtime.server.command_gates)
        return result

    def emit(self, monitor, value, stream="stdout"):
        self.runtime.output({"processId": monitor["id"], "stream": stream,
                             "deltaBase64": base64.b64encode(value).decode()})

    def feed_events(self, key):
        with self.runtime.db() as db:
            return [dict(r) for r in db.execute("SELECT * FROM runtime_events WHERE kind='monitor_exit' AND id=?", ("monitor:" + key,))]

    def test_feed_keeps_native_shell_permissions_timeout_and_skips_model_wake(self):
        feed = self.start(key="feed")
        normal = self.runtime.monitor(self.agent["id"], {"command": "python3 observe.py", "timeout_ms": 5000}, approved=True)
        eventually(lambda: normal["id"] in self.runtime.server.command_gates)
        calls = {p["processId"]: p for m, p in self.runtime.server.calls if m == "command/exec"}
        feed_params = {k: v for k, v in calls[feed["id"]].items() if k != "processId"}
        normal_params = {k: v for k, v in calls[normal["id"]].items() if k != "processId"}
        self.assertEqual(feed_params, normal_params)
        self.assertTrue(feed_params["streamStdoutStderr"])
        self.assertEqual(feed_params["timeoutMs"], 5000)
        self.emit(feed, b'{"shouldNotRender":true}\n', "stderr")
        self.emit(feed, b'{"instances":')
        self.emit(feed, b'2}\n')
        eventually(lambda: len(self.runtime.feed_updates) == 1)
        self.assertEqual(self.runtime.feed_updates[0][4], {"instances": 2})
        self.runtime.server.command_gates[feed["id"]].set()
        eventually(lambda: any(args[4] == "completed" for args, _ in self.runtime.feed_statuses))
        self.assertEqual(self.feed_events(feed["id"]), [])

    def test_final_answer_does_not_stop_feed(self):
        feed = self.start()
        self.runtime.server.complete(self.agent["threadId"], self.agent["turnId"])
        eventually(lambda: not self.runtime.agent(self.agent["id"])["inFlight"])
        self.assertEqual(self.runtime.agent(self.agent["id"])["status"], "completed")
        self.emit(feed, b'{"afterFinal":true}\n')
        eventually(lambda: len(self.runtime.feed_updates) == 1)
        self.assertEqual(self.runtime.feed_updates[0][4], {"afterFinal": True})
        self.assertFalse(self.runtime.server.command_gates[feed["id"]].is_set())

    def test_worker_final_reaches_parent_while_feed_remains_active(self):
        parent = self.agent
        worker = self.runtime.create({"name": "Worker", "prompt": "Observe", "role": "reviewer"}, parent=parent["id"])
        eventually(lambda: self.runtime.agent(worker["id"])["status"] == "running")
        self.agent = self.runtime.agent(worker["id"])
        feed = self.start()
        self.runtime.server.complete(self.agent["threadId"], self.agent["turnId"])
        self.assertEqual(self.runtime.agent(worker["id"])["status"], "completed")
        with self.runtime.db() as db:
            result = db.execute("SELECT text FROM runtime_events WHERE agent=? AND kind='child_result'", (parent["id"],)).fetchone()
        self.assertIsNotNone(result)
        self.assertEqual(json.loads(result[0])["status"], "completed")
        self.assertFalse(self.runtime.server.command_gates[feed["id"]].is_set())

    def test_cancel_targets_only_feed_and_never_enqueues(self):
        feed = self.start()
        normal = self.runtime.monitor(self.agent["id"], {"command": "other", "timeout_ms": 5000}, approved=True)
        eventually(lambda: normal["id"] in self.runtime.server.command_gates)
        self.runtime.cancel_monitor(feed["id"], self.agent["id"])
        self.emit(feed, b'{"late":true}\n')
        eventually(lambda: any(args[4] == "cancelled" for args, _ in self.runtime.feed_statuses))
        self.assertFalse(self.runtime.server.command_gates[normal["id"]].is_set())
        self.assertEqual(self.runtime.feed_updates, [])
        self.assertEqual(self.feed_events(feed["id"]), [])

    def test_cancel_cannot_overtake_consumer_registration(self):
        key = self.reserve("cancel-at-registration")
        original_lock = self.runtime.lock
        runtime = self.runtime
        class CancelAtRegistrationUnlock:
            armed = True
            def __enter__(self):
                return original_lock.__enter__()
            def __exit__(self, *args):
                result = original_lock.__exit__(*args)
                if self.armed and not original_lock._is_owned() and threading.current_thread().name == "register-fixture":
                    self.armed = False
                    runtime.cancel_monitor(key)
                    eventually(lambda: key not in runtime.panel_feed_consumers)
                return result
        self.runtime.lock = CancelAtRegistrationUnlock()
        errors = []
        def register():
            try:
                self.runtime.monitor(self.agent["id"], {"command": "observe"}, key="cancel-at-registration", approved=True, panel_feed=self.binding)
            except Exception as error:
                errors.append(error)
        worker = threading.Thread(target=register, name="register-fixture")
        try:
            worker.start()
            worker.join(3)
            self.assertFalse(worker.is_alive())
            self.assertEqual(errors, [])
            self.assertNotIn(key, self.runtime.panel_feed_consumers)
            self.assertFalse(self.runtime.server.command_gates)
        finally:
            self.runtime.lock = original_lock

    def test_completion_during_cancel_does_not_recreate_consumer(self):
        feed = self.start()
        original_lock = self.runtime.lock
        runtime = self.runtime
        class FinishAtCancelUnlock:
            armed = True
            def __enter__(self):
                return original_lock.__enter__()
            def __exit__(self, *args):
                result = original_lock.__exit__(*args)
                if self.armed and not original_lock._is_owned() and threading.current_thread().name == "cancel-fixture":
                    self.armed = False
                    runtime.server.command_gates[feed["id"]].set()
                    eventually(lambda: feed["id"] not in runtime.panel_feed_consumers)
                return result
        self.runtime.lock = FinishAtCancelUnlock()
        errors = []
        def cancel():
            try:
                self.runtime.cancel_monitor(feed["id"], self.agent["id"])
            except Exception as error:
                errors.append(error)
        worker = threading.Thread(target=cancel, name="cancel-fixture")
        try:
            worker.start()
            worker.join(3)
            self.assertFalse(worker.is_alive())
            self.assertEqual(errors, [])
            self.assertNotIn(feed["id"], self.runtime.panel_feed_consumers)
        finally:
            self.runtime.lock = original_lock

    def test_restart_marks_lost_without_command_replay(self):
        feed = self.start()
        self.runtime.close()
        self.runtime = FeedRuntime(self.tmp.name, FeedServer)
        self.assertEqual(self.runtime.servers, {})
        self.assertTrue(any(args[1] == feed["id"] and args[4] == "lost" for args, _ in self.runtime.feed_statuses))
        self.assertEqual(self.feed_events(feed["id"]), [])

    def test_disconnect_invalidates_feed_without_model_event(self):
        feed = self.start()
        self.runtime.disconnected()
        self.emit(feed, b'{"late":true}\n')
        eventually(lambda: any(args[4] == "lost" for args, _ in self.runtime.feed_statuses))
        self.assertEqual(self.runtime.feed_updates, [])
        self.assertEqual(self.feed_events(feed["id"]), [])

    def pending_approval(self):
        key = self.reserve("approval-request")
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.agent["id"], db)
            actor["yoloMode"] = False
            self.runtime.put(db, "agents", actor)
        # Emulate an approved flag computed before the user disabled YOLO.
        monitor = self.runtime.monitor(self.agent["id"], {"command": "observe"}, key="approval-request", approved=True, panel_feed=self.binding)
        self.assertEqual(monitor["status"], "approval")
        self.assertNotIn(key, self.runtime.server.command_gates)
        with self.runtime.db() as db:
            request = next(r for r in self.runtime.records(db, "requests") if r.get("params", {}).get("monitorId") == key)
        return monitor, request

    def test_permission_change_requires_approval_before_feed_launch(self):
        monitor, request = self.pending_approval()
        self.runtime.answer(request["id"], {"decision": "accept"})
        eventually(lambda: monitor["id"] in self.runtime.server.command_gates)
        self.assertTrue(any(params.get("sandboxPolicy", {}).get("type") == "workspaceWrite"
                            for method, params in self.runtime.server.calls if method == "command/exec"))

    def test_declined_feed_approval_closes_consumer_without_model_event(self):
        monitor, request = self.pending_approval()
        self.runtime.answer(request["id"], {"decision": "decline"})
        eventually(lambda: any(args[1] == monitor["id"] and args[4] == "cancelled" for args, _ in self.runtime.feed_statuses))
        eventually(lambda: monitor["id"] not in self.runtime.panel_feed_consumers)
        self.assertNotIn(monitor["id"], self.runtime.server.command_gates)
        with self.runtime.db() as db:
            self.assertFalse(db.execute("SELECT 1 FROM runtime_events WHERE kind IN ('monitor_exit','monitor_cancelled')").fetchone())

    def test_replaced_reservation_cannot_create_monitor(self):
        self.reserve("replacement")
        with self.assertRaisesRegex(ValueError, "stopped or replaced"):
            self.runtime.monitor(self.agent["id"], {"command": "old"}, key="old", approved=True, panel_feed=self.binding)
        self.assertFalse(self.runtime.server.command_gates)
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db, "monitors"), [])

    def test_replacement_during_preparation_cannot_submit_command(self):
        entered, release = threading.Event(), threading.Event()
        prepare = self.runtime.prepare
        def held_prepare(actor):
            entered.set()
            release.wait(2)
            return prepare(actor)
        self.runtime.prepare = held_prepare
        key = self.reserve("old")
        self.runtime.monitor(self.agent["id"], {"command": "old"}, key="old", approved=True, panel_feed=self.binding)
        self.assertTrue(entered.wait(1))
        self.reserve("replacement")
        release.set()
        eventually(lambda: any(args[1] == key and args[4] == "failed" for args, _ in self.runtime.feed_statuses))
        self.assertFalse(self.runtime.server.command_gates)
        self.assertEqual(self.feed_events(key), [])

    def test_idempotency_includes_binding_and_interactive_is_rejected(self):
        feed = self.start(key="stable-request")
        self.assertEqual(self.start(key="stable-request")["id"], feed["id"])
        self.binding = {**self.binding, "panelVersion": 2}
        with self.assertRaisesRegex(ValueError, "different content"):
            self.start(key="stable-request")
        with self.assertRaisesRegex(ValueError, "separate stdout"):
            self.runtime.monitor(self.agent["id"], {"command": "bad", "interactive": True}, panel_feed=self.binding)


if __name__ == "__main__":
    unittest.main()

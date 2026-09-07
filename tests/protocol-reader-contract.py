#!/usr/bin/env python3
"""Pipe-response isolation and ordered callbacks. No Codex process or model."""

import json
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import AppServer, ResponseTimeout


class Pipe:
    def __init__(self):
        self.lines = queue.Queue()

    def __iter__(self):
        while (line := self.lines.get()) is not None:
            yield line


class Process:
    def __init__(self, *args, **kwargs):
        self.stdout = Pipe()
        self.stdin = self
        self.exited = threading.Event()
        self.writes = []
        self.condition = threading.Condition()

    def emit(self, value):
        self.stdout.lines.put(json.dumps(value) + "\n")

    def write(self, text):
        value = json.loads(text)
        with self.condition:
            self.writes.append(value)
            self.condition.notify_all()
        if value.get("method") == "initialize":
            self.emit({"id": value["id"], "result": {}})

    def flush(self):
        pass

    def poll(self):
        return 0 if self.exited.is_set() else None

    def terminate(self):
        if not self.exited.is_set():
            self.exited.set()
            self.stdout.lines.put(None)

    kill = terminate

    def wait(self, timeout=None):
        if not self.exited.wait(timeout):
            raise subprocess.TimeoutExpired("fake", timeout)
        return 0


class ReaderContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.release = threading.Event()
        self.entered = threading.Event()
        self.records = []
        self.server = None

    def tearDown(self):
        self.release.set()
        if self.server:
            self.server.close()
            self.assertTrue(self.server.join_callbacks(2))
        self.temp.cleanup()

    def start(self, limit=4096, clock_limit=128):
        def notification(message):
            if message["method"] == "blocked":
                self.entered.set()
                if not self.release.wait(3):
                    raise AssertionError("Fixture callback was not released")
            self.records.append(message["method"])
            self.assertIsInstance(message["_studioReceivedAt"], float)

        with patch("codex_runtime.subprocess.Popen", Process), patch.object(AppServer, "CALLBACK_QUEUE_LIMIT", limit), patch.object(AppServer, "CLOCK_QUEUE_LIMIT", clock_limit):
            self.server = AppServer(self.root, notification,
                lambda message: self.records.append(message["method"]),
                lambda: self.records.append("died"))
        return self.server, self.server.proc

    def block(self, proc):
        proc.emit({"method": "blocked"})
        self.assertTrue(self.entered.wait(1))

    def test_blocked_notification_does_not_block_rpc_clock_or_ordered_request(self):
        server, proc = self.start()
        self.block(proc)
        pending = server.submit("config/read", {})
        proc.emit({"method": "tool/request", "id": "tool-1"})
        proc.emit({"method": "currentTime/read", "id": "clock-1"})
        proc.emit({"id": pending[0], "result": {"config": "ready"}})
        self.assertEqual(server.wait(pending, 0.5), {"config": "ready"})
        with proc.condition:
            self.assertTrue(proc.condition.wait_for(
                lambda: any(m.get("id") == "clock-1" for m in proc.writes), 0.5))
        self.assertEqual(self.records, [])
        done = threading.Event()
        server.after_events(lambda: (self.records.append("completed"), done.set()))
        self.release.set()
        self.assertTrue(done.wait(1))
        self.assertEqual(self.records, ["blocked", "tool/request", "completed"])

    def test_timeout_retains_exact_future_and_late_callback_is_ordered(self):
        server, proc = self.start()
        self.block(proc)
        pending = server.submit("turn/start", {})
        with self.assertRaises(ResponseTimeout):
            server.wait(pending, 0)
        self.assertIs(server.pending[pending[0]], pending[2])
        done = threading.Event()
        server.on_result(pending, lambda future: (self.records.append(future.result()["id"]), done.set()))
        proc.emit({"method": "output"})
        proc.emit({"id": pending[0], "result": {"id": "late-receipt"}})
        self.assertEqual(server.wait(pending, 0.5), {"id": "late-receipt"})
        self.assertFalse(done.is_set())
        self.release.set()
        self.assertTrue(done.wait(1))
        self.assertEqual(self.records, ["blocked", "output", "late-receipt"])
        self.assertNotIn(pending[0], server.pending)

    def test_slow_result_callback_does_not_block_later_response(self):
        server, proc = self.start()
        pending = server.submit("thread/start", {})
        server.on_result(pending, lambda _: (self.entered.set(), self.release.wait(3)))
        proc.emit({"id": pending[0], "result": {}})
        self.assertTrue(self.entered.wait(1))
        second = server.submit("model/list", {})
        proc.emit({"id": second[0], "result": {"models": []}})
        self.assertEqual(server.wait(second, 0.5), {"models": []})

    def test_response_resolution_does_not_wait_for_stdin_writer_lock(self):
        server, proc = self.start()
        pending = server.submit("config/read", {})
        with server.write_lock:
            proc.emit({"id": pending[0], "result": {"config": "ready"}})
            self.assertEqual(server.wait(pending, 0.5), {"config": "ready"})

    def test_clock_behind_blocked_writer_does_not_block_config_response(self):
        server, proc = self.start()
        pending = server.submit("config/read", {})
        with server.write_lock:
            proc.emit({"id": "delayed-clock", "method": "currentTime/read"})
            proc.emit({"id": pending[0], "result": {"config": "ready"}})
            self.assertEqual(server.wait(pending, 0.5), {"config": "ready"})
            self.assertFalse(any(m.get("id") == "delayed-clock" for m in proc.writes))
        with proc.condition:
            self.assertTrue(proc.condition.wait_for(
                lambda: any(m.get("id") == "delayed-clock" for m in proc.writes), 0.5))
        response = next(m for m in proc.writes if m.get("id") == "delayed-clock")
        self.assertLessEqual(abs(response["result"]["currentTimeAt"] - time.time()), 1)

    def test_clock_reply_queue_saturation_fails_explicitly(self):
        server, proc = self.start(clock_limit=1)
        pending = server.submit("config/read", {})
        with server.write_lock:
            for index in range(3):
                proc.emit({"id": f"clock-{index}", "method": "currentTime/read"})
            with self.assertRaisesRegex(RuntimeError, "clock reply queue saturated.*outcome unknown"):
                server.wait(pending, 0.5)
            self.assertIsNotNone(proc.poll())
        self.assertTrue(server.join_callbacks(1))
        self.assertIn("clock reply queue saturated", (self.root / "app-server.log").read_text())

    def test_close_does_not_join_clock_writer_under_write_lock(self):
        server, proc = self.start()
        pending = server.submit("config/read", {})
        with server.write_lock:
            proc.emit({"id": "clock", "method": "currentTime/read"})
            proc.emit({"id": pending[0], "result": {}})
            server.wait(pending, 0.5)
            started = time.monotonic()
            server.close()
            self.assertLess(time.monotonic() - started, 0.5)
        self.assertTrue(server.join_callbacks(1))

    def test_disconnect_settles_waiters_before_callback_drain_then_dies_once(self):
        server, proc = self.start()
        self.block(proc)
        pending = server.submit("command/exec", {})
        proc.emit({"id": "req", "method": "tool/request"})
        proc.terminate()
        with self.assertRaisesRegex(RuntimeError, "disconnected; outcome unknown"):
            server.wait(pending, 0.5)
        self.assertFalse(server.pending)
        self.assertEqual(self.records, [])
        self.release.set()
        self.assertTrue(server.join_callbacks(1))
        self.assertEqual(self.records, ["blocked", "tool/request", "died"])
        server.close()
        server.close()
        self.assertEqual(self.records.count("died"), 1)
        late = []
        server.on_result(pending, lambda future: late.append(str(future.exception())))
        self.assertEqual(len(late), 1)

    def test_close_does_not_join_blocked_runtime_callback(self):
        server, proc = self.start()
        self.block(proc)
        started = time.monotonic()
        server.close()
        self.assertLess(time.monotonic() - started, 0.5)
        self.assertFalse(server.join_callbacks(0))
        self.release.set()
        self.assertTrue(server.join_callbacks(1))
        self.assertEqual(self.records, ["blocked"])

    def test_queue_saturation_is_explicit_and_preserves_accepted_order(self):
        server, proc = self.start(limit=2)
        self.block(proc)
        pending = server.submit("turn/start", {})
        proc.emit({"method": "accepted-1"})
        proc.emit({"method": "accepted-2"})
        proc.emit({"method": "rejected-3", "id": "overflow-request"})
        with self.assertRaisesRegex(RuntimeError, "callback queue saturated.*outcome unknown"):
            server.wait(pending, 0.5)
        self.assertIsNotNone(proc.poll())
        with self.assertRaisesRegex(RuntimeError, "offline"):
            server.submit("turn/start", {})
        self.release.set()
        self.assertTrue(server.join_callbacks(1))
        self.assertEqual(self.records, ["blocked", "accepted-1", "accepted-2", "died"])
        self.assertIn("callback queue saturated", (self.root / "app-server.log").read_text())

    def test_slow_queue_diagnostic_contains_identity_without_message_contents(self):
        server, proc = self.start()
        server.enqueue(server.request, {'id': 'slow', 'method': 'item/tool/call',
            '_studioReceivedAt': time.time() - 2,
            'params': {'threadId': 'thread', 'text': 'private payload'}})
        done = threading.Event()
        server.after_events(done.set)
        self.assertTrue(done.wait(1))
        log = (self.root / 'app-server.log').read_text()
        row = json.loads(next(line for line in log.splitlines() if 'callbackLatency' in line))
        self.assertGreaterEqual(row['queueDelayMs'], 2000)
        self.assertEqual(row['threadId'], 'thread')
        self.assertEqual(row['rpcId'], 'slow')
        self.assertNotIn('private payload', log)

    def test_delta_backlog_batches_without_crossing_request_or_item_boundaries(self):
        server, proc = self.start()
        self.block(proc)
        batches, order = [], []
        def notification(message):
            batches.append(message)
            order.append(message['params']['itemId'])
        server.notification = notification
        server.request = lambda message: order.append('request')
        samples = []
        for i in range(300):
            params = {'threadId': 't', 'turnId': 'turn', 'itemId': 'a', 'delta': f'{i} Привет\n'}
            samples.append(params)
            proc.emit({'method': 'item/agentMessage/delta', 'params': params})
        proc.emit({'method': 'item/tool/call', 'id': 'request'})
        for item in ['a', 'b']:
            proc.emit({'method': 'item/agentMessage/delta', 'params': {
                'threadId': 't', 'turnId': 'turn', 'itemId': item, 'delta': 'tail'}})
        marker = server.submit('marker', {})
        proc.emit({'id': marker[0], 'result': {}})
        server.wait(marker, 1)  # All preceding wire messages are now enqueued.
        done = threading.Event()
        server.after_events(done.set)
        self.release.set()
        self.assertTrue(done.wait(2))
        self.assertEqual(order, ['a', 'a', 'a', 'request', 'a', 'b'])
        self.assertEqual(''.join(m['params']['delta'] for m in batches[:3]),
                         ''.join(p['delta'] for p in samples))
        self.assertEqual([p for m in batches[:3] for p in m['_studioNotificationSamples']], samples)
        self.assertTrue(all(isinstance(m['_studioDispatchedAt'], float) for m in batches))


if __name__ == "__main__":
    unittest.main()

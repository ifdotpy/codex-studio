#!/usr/bin/env python3
"""Pipe-response isolation and ordered callbacks. No Codex process or model."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import json
import os
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
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import AppServer, ResponseTimeout
from codex_native_errors import NativeRpcError


class Pipe:
    def __init__(self):
        self.lines = queue.Queue()

    def __iter__(self):
        while (line := self.lines.get()) is not None:
            yield line


class Process:
    def __init__(self, *args, **kwargs):
        self.pid = os.getpid()
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

        with patch("codex_runtime.subprocess.Popen", Process), patch(
            "codex_runtime.provider_process_command", side_effect=lambda command: command
        ), patch.object(AppServer, "CALLBACK_QUEUE_LIMIT", limit), patch.object(AppServer, "CLOCK_QUEUE_LIMIT", clock_limit):
            self.server = AppServer(self.root, notification,
                lambda message: self.records.append(message["method"]),
                lambda: self.records.append("died"))
        return self.server, self.server.proc

    def block(self, proc):
        proc.emit({"method": "blocked"})
        self.assertTrue(self.entered.wait(1))

    def test_explicit_rpc_error_keeps_code_and_data(self):
        server, proc = self.start()
        pending = server.submit("turn/steer", {})
        data = {"codexErrorInfo": {"activeTurnNotSteerable": {"turnKind": "compact"}}}
        proc.emit({"id": pending[0], "error": {"code": -32600, "message": "Cannot steer", "data": data}})
        with self.assertRaises(NativeRpcError) as result:
            server.wait(pending, 0.5)
        self.assertEqual(result.exception.code, -32600)
        self.assertEqual(result.exception.data, data)
        self.assertNotIn(pending[0], server.pending)
        self.assertNotIsInstance(result.exception, ResponseTimeout)

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

    def test_tool_call_bypasses_blocked_notification_queue(self):
        server, proc = self.start()
        self.block(proc)
        handled = threading.Event()
        server.request = lambda message: handled.set()
        proc.emit({"method": "item/tool/call", "id": "tool-urgent",
                   "params": {"threadId": "worker", "tool": "orchestration_task"}})
        self.assertTrue(handled.wait(0.5))
        self.assertFalse(self.release.is_set())
        self.assertEqual(self.records, [])
        self.release.set()

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

    def test_terminal_output_burst_keeps_connection_and_receipts(self):
        server, proc = self.start(limit=64)
        self.block(proc)
        batches, order = [], []
        def notification(message):
            batches.append(message)
            order.append(message['params']['itemId'])
        server.notification = notification
        handled = threading.Event()
        server.request = lambda message: (order.append('request'), handled.set())
        samples = []
        for i in range(5000):
            params = {'threadId': 't', 'turnId': 'turn', 'itemId': 'command', 'delta': f'{i} Привет\n'}
            samples.append(params)
            proc.emit({'method': 'item/commandExecution/outputDelta', 'params': params})
        proc.emit({'method': 'item/tool/call', 'id': 'request'})
        for item in ['command', 'other']:
            proc.emit({'method': 'item/commandExecution/outputDelta', 'params': {
                'threadId': 't', 'turnId': 'turn', 'itemId': item, 'delta': 'tail'}})
        marker = server.submit('marker', {})
        proc.emit({'id': marker[0], 'result': {'ready': True}})
        self.assertEqual(server.wait(marker, 2), {'ready': True})
        self.assertIsNone(proc.poll())
        self.assertLess(server.callbacks.qsize(), 64)
        self.assertTrue(handled.wait(1))
        done = threading.Event()
        server.after_events(done.set)
        self.release.set()
        self.assertTrue(done.wait(2))
        before = batches[:-2]
        self.assertEqual(''.join(m['params']['delta'] for m in before), ''.join(p['delta'] for p in samples))
        self.assertEqual([p for m in before for p in m.get('_studioNotificationSamples', [m['params']])], samples)
        self.assertEqual(order[0], 'request')
        self.assertEqual(order[-2:], ['command', 'other'])
        self.assertTrue(all(len(m['params']['delta']) <= 65536 for m in before))
        self.assertTrue(all(len(m.get('_studioNotificationSamples', [])) <= 128 for m in before))
        self.assertIsNone(server.transport_error)

    def test_deep_queue_sheds_interleaved_fragments_and_keeps_connection(self):
        server, proc = self.start(limit=64)
        self.block(proc)
        seen = []
        server.notification = lambda message: seen.append(message)
        server.request = lambda message: seen.append(message)
        for i in range(100):
            # Different items never merge, so the queue grows until shedding starts.
            proc.emit({'method': 'item/commandExecution/outputDelta',
                       'params': {'threadId': 't', 'itemId': f'item-{i}', 'delta': f'{i}\n'}})
        for i in range(10):
            proc.emit({'method': 'item/started', 'params': {'threadId': 't', 'item': {'id': f'x-{i}'}}})
        marker = server.submit('marker', {})
        proc.emit({'id': marker[0], 'result': {'ready': True}})
        self.assertEqual(server.wait(marker, 2), {'ready': True})
        self.assertIsNone(proc.poll())
        self.assertIsNone(server.transport_error)
        shed_before = set(server._shed_items)
        self.assertTrue(shed_before)
        dropped = sorted(shed_before)[0][1]
        done = threading.Event()
        server.after_events(done.set)
        self.release.set()
        self.assertTrue(done.wait(3))
        started = [m['params']['item']['id'] for m in seen if m.get('method') == 'item/started']
        self.assertEqual(started, [f'x-{i}' for i in range(10)])
        self.assertLess(len([m for m in seen if m.get('method') == 'item/commandExecution/outputDelta']), 100)
        # With a shallow queue a shed item stays shed until it completes.
        seen.clear()
        proc.emit({'method': 'item/commandExecution/outputDelta', 'params': {'threadId': 't', 'itemId': dropped, 'delta': 'late'}})
        proc.emit({'method': 'item/completed', 'params': {'threadId': 't', 'item': {'id': dropped, 'aggregatedOutput': 'full'}}})
        proc.emit({'method': 'item/commandExecution/outputDelta', 'params': {'threadId': 't', 'itemId': dropped, 'delta': 'next'}})
        marker = server.submit('marker', {})
        proc.emit({'id': marker[0], 'result': {}})
        server.wait(marker, 1)  # All preceding wire messages are now enqueued.
        done = threading.Event()
        server.after_events(done.set)
        self.assertTrue(done.wait(2))
        self.assertEqual([(m['method'], m['params'].get('delta')) for m in seen],
                         [('item/completed', None), ('item/commandExecution/outputDelta', 'next')])
        self.assertIn('streamed fragments', (self.root / 'app-server.log').read_text())

    def test_interleaved_streams_coalesce_across_the_queue_in_thread_order(self):
        server, proc = self.start()
        self.block(proc)
        seen = []
        server.notification = lambda message: seen.append(message)
        server.request = lambda message: seen.append(message)
        sent = {}
        for i in range(100):
            for t in range(10):
                if t == 3 and i == 50:
                    proc.emit({'method': 'item/started', 'params': {'threadId': 't3', 'item': {'id': 'marker'}}})
                params = {'threadId': f't{t}', 'itemId': f'cmd-{t}', 'delta': f'{t}:{i};'}
                sent.setdefault(f't{t}', []).append(params)
                proc.emit({'method': 'item/commandExecution/outputDelta', 'params': params})
        marker = server.submit('marker', {})
        proc.emit({'id': marker[0], 'result': {}})
        server.wait(marker, 1)
        self.assertLessEqual(server.callbacks.qsize(), 12)
        done = threading.Event()
        server.after_events(done.set)
        self.release.set()
        self.assertTrue(done.wait(2))
        for t in range(10):
            thread = f't{t}'
            events = [m for m in seen if m['params'].get('threadId') == thread]
            fragments = [m for m in events if m['method'] == 'item/commandExecution/outputDelta']
            self.assertEqual(''.join(m['params']['delta'] for m in fragments), ''.join(p['delta'] for p in sent[thread]))
            self.assertEqual([p for m in fragments for p in m.get('_studioNotificationSamples', [m['params']])], sent[thread])
            self.assertTrue(all('_studioSlot' not in m for m in events))
        t3 = [m['method'] for m in seen if m['params'].get('threadId') == 't3']
        position = t3.index('item/started')
        before = [m for m in seen if m['params'].get('threadId') == 't3'][:position]
        self.assertEqual(''.join(m['params']['delta'] for m in before), ''.join(p['delta'] for p in sent['t3'][:50]))

    def test_latest_values_keep_keys_and_item_request_boundaries(self):
        server, proc = self.start()
        self.block(proc)
        seen = []
        server.notification = lambda message: seen.append((message['method'], message.get('params', {})))
        server.request = lambda message: seen.append((message['method'], message.get('params', {})))
        for value in range(50):
            proc.emit({'method': 'account/rateLimits/updated', 'params': {'rateLimits': {'usedPercent': value}}})
        proc.emit({'method': 'item/started', 'params': {'threadId': 'a', 'item': {'id': 'item'}}})
        proc.emit({'method': 'account/rateLimits/updated', 'params': {'rateLimits': {'usedPercent': 50}}})
        proc.emit({'method': 'item/tool/requestUserInput', 'id': 'tool', 'params': {'threadId': 'a'}})
        proc.emit({'method': 'account/rateLimits/updated', 'params': {'rateLimits': {'usedPercent': 51}}})
        proc.emit({'method': 'turn/diff/updated', 'params': {'threadId': 'a', 'turnId': 'one', 'value': 1}})
        proc.emit({'method': 'turn/diff/updated', 'params': {'threadId': 'a', 'turnId': 'two', 'value': 2}})
        proc.emit({'method': 'turn/diff/updated', 'params': {'threadId': 'a', 'turnId': 'one', 'value': 3}})
        marker = server.submit('marker', {})
        proc.emit({'id': marker[0], 'result': {}})
        server.wait(marker, 1)
        self.assertLessEqual(server.callbacks.qsize(), 10)
        done = threading.Event()
        server.after_events(done.set)
        self.release.set()
        self.assertTrue(done.wait(2))
        values = [(method, p.get('threadId'), p.get('turnId'), p.get('value', (p.get('rateLimits') or {}).get('usedPercent')))
                  for method, p in seen]
        self.assertEqual(values, [
            ('account/rateLimits/updated', None, None, 49),
            ('item/started', 'a', None, None),
            ('account/rateLimits/updated', None, None, 50),
            ('item/tool/requestUserInput', 'a', None, None),
            ('account/rateLimits/updated', None, None, 51),
            ('turn/diff/updated', 'a', 'one', 1),
            ('turn/diff/updated', 'a', 'two', 2),
            ('turn/diff/updated', 'a', 'one', 3),
        ])

    def test_every_token_usage_notice_is_delivered(self):
        # Each notice is one request's usage; the budget and analytics count all of them.
        server, proc = self.start()
        self.block(proc)
        seen = []
        server.notification = lambda message: seen.append(message['params']['value'])
        for value in range(30):
            proc.emit({'method': 'thread/tokenUsage/updated', 'params': {'threadId': 'a', 'value': value}})
        marker = server.submit('marker', {})
        proc.emit({'id': marker[0], 'result': {}})
        server.wait(marker, 1)
        done = threading.Event()
        server.after_events(done.set)
        self.release.set()
        self.assertTrue(done.wait(2))
        self.assertEqual(seen, list(range(30)))

    def test_tool_bypass_closes_latest_slot_without_delaying_tool(self):
        server, proc = self.start()
        self.block(proc)
        values, tool_done = [], threading.Event()
        server.notification = lambda message: values.append(message['params']['value'])
        server.request = lambda message: tool_done.set()
        proc.emit({'method': 'thread/tokenUsage/updated', 'params': {'threadId': 'a', 'value': 1}})
        proc.emit({'method': 'item/tool/call', 'id': 'tool', 'params': {'threadId': 'a'}})
        proc.emit({'method': 'thread/tokenUsage/updated', 'params': {'threadId': 'a', 'value': 2}})
        marker = server.submit('marker', {})
        proc.emit({'id': marker[0], 'result': {}})
        server.wait(marker, 1)
        self.assertTrue(tool_done.wait(1))
        self.assertEqual(server.callbacks.qsize(), 2)
        done = threading.Event()
        server.after_events(done.set)
        self.release.set()
        self.assertTrue(done.wait(2))
        self.assertEqual(values, [1, 2])

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
        threading.Thread(target=lambda: (server.tool_requests.join(), done.set()), daemon=True).start()
        self.assertTrue(done.wait(30))
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
        handled = threading.Event()
        server.request = lambda message: (order.append('request'), handled.set())
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
        self.assertTrue(handled.wait(1))
        done = threading.Event()
        server.after_events(done.set)
        self.release.set()
        self.assertTrue(done.wait(2))
        self.assertEqual(order, ['request', 'a', 'a', 'a', 'a', 'b'])
        self.assertEqual(''.join(m['params']['delta'] for m in batches[:3]),
                         ''.join(p['delta'] for p in samples))
        self.assertEqual([p for m in batches[:3] for p in m['_studioNotificationSamples']], samples)
        self.assertTrue(all(isinstance(m['_studioDispatchedAt'], float) for m in batches))


if __name__ == "__main__":
    unittest.main()

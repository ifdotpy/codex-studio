#!/usr/bin/env python3
"""Delayed configuration, native output, and response delivery use exact receipts."""
import concurrent.futures
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('response_fixture', ROOT / 'tests/monitor-lifecycle-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import ResponseTimeout, Runtime

eventually = fixture.fixture.eventually
CONFIG = {'config': {'features': {'shell_snapshot': False}}}


class DelayedConfigurationServer(fixture.MonitorServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.delayed = []
        self.hold_resume = False
        self.command_threads = []

    def submit(self, method, params):
        if method == 'config/read' or (method == 'thread/resume' and self.hold_resume):
            future = concurrent.futures.Future()
            self.calls.append((method, params))
            self.delayed.append((method, params, future))
            return future
        if method == 'command/exec':
            self.command_threads.append(threading.current_thread())
        return super().submit(method, params)

    def wait(self, submitted, timeout=60):
        if any(submitted is future for _, _, future in self.delayed):
            raise ResponseTimeout('Native response timed out; outcome unknown')
        return super().wait(submitted, timeout)

    def close(self):
        for _, _, future in self.delayed:
            if not future.done():
                future.set_exception(RuntimeError('Fixture disconnected'))
        super().close()


class HarnessResponseContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.runtime = Runtime(self.root, DelayedConfigurationServer)
        self.runtime.preparation_wait_seconds = .01
        a = self.runtime.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': 'Wait'})
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        self.agent = self.runtime.agent(a['id'])
        self.server = self.runtime.server

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def count(self, method):
        return sum(m == method for m, _ in self.server.calls)

    def record(self, key):
        with self.runtime.db() as db:
            return json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?', (key,)).fetchone()[0])

    def pending_monitor(self):
        monitor = self.runtime.monitor(self.agent['id'], {'command': 'fixture-command', 'timeout_ms': 1000}, approved=True)
        eventually(lambda: self.record(monitor['id']).get('configurationPending'))
        return monitor['id']

    def config_futures(self):
        return [f for m, _, f in self.server.delayed if m == 'config/read']

    def message(self, call='status', tool='orchestration_status', args=None):
        return {'id': 701, 'params': {'threadId': self.agent['threadId'], 'turnId': self.agent['turnId'],
                'callId': call, 'tool': tool, 'arguments': args or {}}}

    def test_null_native_output_retains_streamed_tail_on_completion(self):
        command = {'id': 'native-command', 'type': 'commandExecution', 'command': 'fixture', 'aggregatedOutput': None}
        def event(method, **params):
            self.runtime.notification({'method': method, 'params': {'threadId': self.agent['threadId'],
                'turnId': self.agent['turnId'], **params}})
        event('item/started', item=command)
        event('item/commandExecution/outputDelta', itemId=command['id'], delta='first\n')
        event('item/commandExecution/outputDelta', itemId=command['id'], delta='second\n')
        key = self.agent['id'] + ':' + command['id']
        self.assertEqual(self.runtime.task_detail(key)['tail'], 'first\nsecond\n')
        event('item/completed', item={**command, 'exitCode': 0})
        self.assertEqual(self.runtime.task_detail(key)['tail'], 'first\nsecond\n')
        item = next(i for i in self.runtime.transcript(self.agent['id'])['items'] if i['id'] == key)
        self.assertEqual(json.loads(item['text'])['aggregatedOutput'], 'first\nsecond\n')

    def test_failed_reply_preserves_receipt_without_content_in_diagnostic(self):
        message = self.message('rename', 'orchestration_title', {'title': 'Private project title'})
        connection = self.runtime.connection_ids['default']
        with patch.object(self.runtime, 'reply', side_effect=OSError(32, 'PRIVATE ERROR DETAIL')):
            self.runtime.dynamic(message, 'default', connection)
        with self.runtime.db() as db:
            receipt = json.loads(db.execute('SELECT result FROM runtime_tool_results WHERE id=?',
                (self.agent['threadId'] + ':rename',)).fetchone()[0])
        self.assertTrue(receipt['success'])
        raw = (self.root / 'runtime-errors.log').read_text()
        diagnostic = json.loads(raw)
        self.assertEqual(diagnostic['requestId'], 701)
        self.assertEqual(diagnostic['callId'], 'rename')
        self.assertEqual(diagnostic['connectionId'], connection)
        self.assertEqual(diagnostic['errno'], 32)
        self.assertNotIn('Private project title', raw)
        self.assertNotIn('PRIVATE ERROR DETAIL', raw)
        self.runtime.rename(self.agent['id'], 'User override')
        self.runtime.dynamic({**message, 'id': 702}, 'default', connection)
        self.assertEqual(self.runtime.agent(self.agent['id'])['name'], 'User override')
        self.assertEqual(self.server.responses[-1]['result'], receipt)

    def test_late_config_submits_same_command_once(self):
        key = self.pending_monitor()
        self.assertEqual(self.record(key)['status'], 'starting')
        self.assertEqual(self.count('command/exec'), 0)
        self.config_futures()[0].set_result(CONFIG)
        eventually(lambda: key in self.server.commands)
        self.assertEqual(self.count('config/read'), 1)
        self.assertEqual(self.count('command/exec'), 1)
        self.assertFalse(self.record(key)['configurationPending'])
        self.assertIsNone(self.record(key)['error'])
        self.server.finish(key)
        eventually(lambda: self.record(key)['status'] == 'completed')

    def test_cancel_before_config_ack_does_not_submit(self):
        key = self.pending_monitor()
        self.runtime.cancel_monitor(key)
        self.config_futures()[0].set_result(CONFIG)
        self.runtime.pool.submit(lambda: None).result(2)
        self.assertEqual(self.record(key)['status'], 'cancelled')
        self.assertFalse(self.record(key)['configurationPending'])
        self.assertEqual(self.count('command/exec'), 0)

    def test_stopped_epoch_ignores_late_config(self):
        key = self.pending_monitor()
        self.runtime.stop(self.agent['id'])
        self.config_futures()[0].set_result(CONFIG)
        self.runtime.pool.submit(lambda: None).result(2)
        self.assertEqual(self.count('command/exec'), 0)
        self.assertEqual(self.record(key)['status'], 'cancelled')

    def test_disconnect_ignores_late_config(self):
        self.pending_monitor()
        self.runtime.disconnected('default', self.runtime.connection_ids['default'])
        self.config_futures()[0].set_result(CONFIG)
        self.runtime.pool.submit(lambda: None).result(2)
        self.assertEqual(self.count('command/exec'), 0)

    def test_current_permission_policy_used_after_config_ack(self):
        key = self.pending_monitor()
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.agent['id'], db)
            a['yoloMode'] = False
            self.runtime.put(db, 'agents', a)
        self.config_futures()[0].set_result(CONFIG)
        eventually(lambda: key in self.server.commands)
        params = next(p for m, p in self.server.calls if m == 'command/exec')
        self.assertNotEqual(params['sandboxPolicy']['type'], 'dangerFullAccess')

    def test_config_failure_is_terminal_without_command(self):
        key = self.pending_monitor()
        self.config_futures()[0].set_exception(RuntimeError('Configuration rejected'))
        eventually(lambda: self.record(key)['status'] == 'failed')
        self.assertFalse(self.record(key)['configurationPending'])
        self.assertIsNone(self.record(key).get('exitCode'))
        self.assertEqual(self.count('command/exec'), 0)

    def test_seventeen_deferred_commands_leave_dynamic_tool_pool_available(self):
        keys = [self.pending_monitor() for _ in range(17)]
        for future in self.config_futures():
            future.set_result(CONFIG)
        eventually(lambda: len(self.server.commands) == 17)
        self.runtime.pool.submit(self.runtime.dynamic, self.message()).result(3)
        self.assertTrue(self.server.responses[-1]['result']['success'])
        self.assertEqual(set(self.server.commands), set(keys))
        with self.runtime.lock:
            self.assertTrue(set(self.server.command_threads).issubset(self.runtime.monitor_threads))
        self.assertTrue(all(not future.done() for future in self.server.commands.values()))

    def test_late_preparation_restarts_monitor_in_owned_thread(self):
        self.server.complete(self.agent['threadId'], self.agent['turnId'])
        self.runtime.loaded.discard(self.agent['id'])
        self.server.hold_resume = True
        monitor = self.runtime.monitor(self.agent['id'], {'command': 'fixture-command', 'timeout_ms': 1000}, approved=True)
        eventually(lambda: any(m == 'thread/resume' for m, _, _ in self.server.delayed))
        _, params, future = next(e for e in self.server.delayed if e[0] == 'thread/resume')
        future.set_result({'thread': {'id': params['threadId']}, 'model': params['model'],
                          'sandbox': {'type': 'readOnly'}, 'approvalPolicy': 'on-request'})
        eventually(lambda: self.record(monitor['id']).get('configurationPending'))
        self.config_futures()[0].set_result(CONFIG)
        eventually(lambda: monitor['id'] in self.server.commands)
        with self.runtime.lock:
            self.assertIn(self.server.command_threads[0], self.runtime.monitor_threads)
        self.runtime.pool.submit(lambda: True).result(2)
        self.assertEqual(self.count('thread/resume'), 1)
        self.assertEqual(self.count('command/exec'), 1)


if __name__ == '__main__':
    unittest.main()

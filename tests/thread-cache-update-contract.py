#!/usr/bin/env python3
"""Run the cache regressions against the exact live-code update boundary."""
import ast
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import types
import unittest
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import codex_thread_cache_update as update
from codex_efficiency_update import fingerprint

spec = importlib.util.spec_from_file_location('cache_contract', Path(__file__).with_name('missing-thread-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
root = Path(__file__).resolve().parents[1]
def source(revision):
    return subprocess.check_output(['git', 'show', revision + ':scripts/codex_runtime.py'], cwd=root, text=True)
notification_source = source(update.NOTIFICATION_REVISION)
previous_source = source('d1bd9b225173862d199f602bc875e95ed7cebeb9')

class LiveUpdateContract(fixture.MissingThreadContract):
    def install_previous(self):
        for name in (*update.BASE, 'prepare_locked'):
            old = getattr(self.runtime, name).__func__
            code = update.method_code(ast.parse(notification_source if name == 'notification' else previous_source), name)
            scope = old.__globals__.copy()
            scope.pop('NativeRpcError', None)
            function = types.FunctionType(code, scope, name, old.__defaults__)
            function.__kwdefaults__ = old.__kwdefaults__
            setattr(self.runtime, name, types.MethodType(function, self.runtime))
        self.assertEqual({n: fingerprint(getattr(self.runtime, n)) for n in update.BASE}, update.BASE)
        self.assertEqual(fingerprint(self.runtime.prepare_locked), update.PREPARE)

    def setUp(self):
        super().setUp()
        self.install_previous()
        self.update_result = update.apply(self.runtime, notification_source)

    def test_update_preserves_connections_and_sends_nothing(self):
        self.install_previous()
        servers, connections = dict(self.runtime.servers), dict(self.runtime.connection_ids)
        calls = list(self.server.calls)
        before = self.runtime.agent(self.key)
        result = update.apply(self.runtime, notification_source)
        self.assertEqual(result['status'], 'applied')
        self.assertEqual(self.runtime.servers, servers)
        self.assertEqual(self.runtime.connection_ids, connections)
        self.assertEqual(self.server.calls, calls)
        self.assertEqual(self.runtime.agent(self.key), before)
        self.assertEqual(update.apply(self.runtime, notification_source)['status'], 'already_applied')

    def test_normal_turn_notifications_still_complete_delivery(self):
        agent = self.runtime.create({'name': 'Fresh lead', 'cwd': self.temp.name, 'prompt': 'One task'})
        fixture.fixture.eventually(lambda: self.runtime.agent(agent['id'])['status'] == 'running')
        current = self.runtime.agent(agent['id'])
        self.server.complete(current['threadId'], current['turnId'])
        fixture.fixture.eventually(lambda: self.runtime.agent(agent['id'])['status'] == 'completed')
        with self.runtime.db() as db:
            statuses = [r[0] for r in db.execute('SELECT status FROM runtime_events WHERE agent=?', (agent['id'],))]
        self.assertEqual(statuses, ['delivered'])

    def test_unknown_method_is_rejected_before_any_replacement(self):
        self.install_previous()
        self.runtime.start_error = types.MethodType(lambda self, *a: None, self.runtime)
        before = {n: getattr(self.runtime, n) for n in update.BASE}
        with self.assertRaisesRegex(RuntimeError, 'Unknown or mixed'):
            update.apply(self.runtime, notification_source)
        self.assertEqual({n: getattr(self.runtime, n) for n in update.BASE}, before)

    def test_legacy_reader_rejection_invalidates_exact_thread(self):
        self.rejection(RuntimeError(json.dumps({'code': -32600, 'message': 'thread not found: ' + self.thread})))
        self.assertNotIn(self.key, self.runtime.loaded)
        self.assertEqual(self.runtime.delivery_receipt('input')['status'], 'failed')
        self.assertFalse(any(m == 'turn/start' for m, _ in self.server.calls))

    def test_legacy_lookalike_error_is_not_native_rejection(self):
        for error in (RuntimeError('thread not found: x'), RuntimeError('{"code": -1, "message": "thread not found: x"}'),
                      RuntimeError('turn/start response timed out; outcome unknown')):
            self.assertIs(update.normalize_legacy_missing_thread(error), error)

if __name__ == '__main__':
    unittest.main()

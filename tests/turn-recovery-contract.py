#!/usr/bin/env python3
"""Lost terminal notifications, exact identities, and non-destructive recovery."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class RecoveryServer(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.native = None
        self.read_error = None
        self.before_apply = None
        self.read_entered = threading.Event()
        self.read_gate = None

    def call(self, method, params, timeout=60):
        if method == 'thread/turns/list':
            self.calls.append((method, params))
            return {'data': self.native.get('turns', [])[:params['limit']]}
        if method == 'thread/read':
            self.calls.append((method, params))
            self.read_entered.set()
            if self.read_gate:
                assert self.read_gate.wait(3)
            if self.read_error:
                raise self.read_error
            result = dict(self.native)
            if not params['includeTurns']:
                result.pop('turns', None)
            return {'thread': result}
        return super().call(method, params, timeout)

    def after_events(self, callback):
        if self.before_apply:
            self.before_apply()
        callback()


class TurnRecoveryContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), RecoveryServer)
        self.server = self.runtime.connect()
        a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Work'})
        self.key = a['id']
        fixture.eventually(lambda: bool(self.runtime.agent(self.key).get('turnId')))
        self.a = self.runtime.agent(self.key)
        self.turn = self.a['turnId']
        self.server.native = {'id': self.a['threadId'], 'status': {'type': 'idle'}, 'turns': [
            {'id': self.turn, 'status': 'completed', 'items': [
                {'id': 'answer', 'type': 'agentMessage', 'text': 'Full final answer', 'phase': 'final_answer'}]}]}

    def tearDown(self):
        if self.server.read_gate:
            self.server.read_gate.set()
        self.runtime.close()
        self.temp.cleanup()

    def test_lost_completion_repairs_partial_text_and_is_idempotent(self):
        self.server.notify({'method': 'item/agentMessage/delta', 'params': {
            'threadId': self.a['threadId'], 'turnId': self.turn, 'itemId': 'answer', 'delta': 'par'}})
        result = self.runtime.reconcile_turn(self.key)
        self.assertEqual(result['outcome'], 'completed')
        a = self.runtime.agent(self.key)
        self.assertFalse(a['inFlight'])
        self.assertIsNone(a['turnId'])
        self.assertEqual(a['lastAnswer'], 'Full final answer')
        self.assertEqual(a['turnRecovery']['source'], 'native_thread_read')
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'skipped')
        with self.runtime.db() as db:
            row = json.loads(db.execute('SELECT record FROM runtime_items WHERE id=?', (self.key + ':answer',)).fetchone()[0])
            self.assertFalse(row['streaming'])
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_completed_turns').fetchone()[0], 1)

    def test_unloaded_interrupted_thread_clears_preparation_cache(self):
        self.assertIn(self.key, self.runtime.loaded)
        self.server.native['status']['type'] = 'notLoaded'
        self.server.native['turns'][0]['status'] = 'interrupted'
        self.assertEqual(self.runtime.reconcile_turn(self.key)['outcome'], 'interrupted')
        self.assertNotIn(self.key, self.runtime.loaded)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'interrupted')
        self.assertFalse(any(name == 'turn/interrupt' for name, _ in self.server.calls))

    def test_failed_turn_preserves_native_error(self):
        self.server.native['turns'][0].update(status='failed', error={'message': 'native failure'})
        self.runtime.reconcile_turn(self.key)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'failed')
        self.assertEqual(self.runtime.agent(self.key)['error'], {'message': 'native failure'})

    def test_active_turn_never_reads_full_history_or_changes_state(self):
        self.server.native['status']['type'] = 'active'
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'active_or_unknown')
        self.assertEqual([p['includeTurns'] for n, p in self.server.calls if n == 'thread/read'], [False])
        self.assertEqual(self.runtime.agent(self.key)['turnId'], self.turn)

    def test_read_timeout_is_not_completion_or_mutation_retry(self):
        before = list(self.server.calls)
        self.server.read_error = TimeoutError('unknown outcome')
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
        self.assertTrue(self.runtime.agent(self.key)['inFlight'])
        self.assertEqual([n for n, _ in self.server.calls[len(before):]], ['thread/read'])

    def test_missing_exact_turn_and_unknown_status_are_not_completion(self):
        for turn_id, status in [('other-turn', 'completed'), (self.turn, 'inProgress')]:
            self.server.native['turns'][0].update(id=turn_id, status=status)
            self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
            self.assertTrue(self.runtime.agent(self.key)['inFlight'])

    def test_native_thread_identity_must_match(self):
        self.server.native['id'] = 'foreign-thread'
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
        self.assertTrue(self.runtime.agent(self.key)['inFlight'])

    def test_queued_notifications_are_applied_before_identity_check(self):
        def newer():
            with self.runtime.lock, self.runtime.db() as db:
                a = self.runtime.agent(self.key, db)
                a['turnId'] = 'new-turn'
                self.runtime.put(db, 'agents', a)
        self.server.before_apply = newer
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'superseded')
        self.assertEqual(self.runtime.agent(self.key)['turnId'], 'new-turn')

    def test_stop_and_account_connection_replacement_win(self):
        for field, value in [('epoch', 999), ('accountKey', 'foreign'), ('deletedAt', 1)]:
            with self.subTest(field=field):
                with self.runtime.lock, self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.a)
                def changed():
                    with self.runtime.lock, self.runtime.db() as db:
                        a = self.runtime.agent(self.key, db); a[field] = value
                        self.runtime.put(db, 'agents', a)
                self.server.before_apply = changed
                self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'superseded')
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.a)
        self.server.before_apply = lambda: self.runtime.connection_ids.update(default='replacement')
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'superseded')

    def test_monitor_and_detached_command_are_not_stopped(self):
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'monitors', {'id': 'monitor', 'agent': self.key, 'status': 'running'})
            self.runtime.put(db, 'tasks', {'id': self.key + ':command', 'agent': self.key, 'turnId': self.turn,
                'kind': 'command', 'processId': 'live-process', 'status': 'running'})
        self.runtime.reconcile_turn(self.key)
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db, 'monitors')[0]['status'], 'running')
            self.assertEqual(self.runtime.records(db, 'tasks')[0]['status'], 'running')
        self.assertFalse(any(n in {'command/exec/terminate', 'turn/interrupt'} for n, _ in self.server.calls))

    def test_existing_pending_message_delivered_once(self):
        self.runtime.send(self.key, 'Next task', message_id='next-task')
        self.runtime.reconcile_turn(self.key)
        fixture.eventually(lambda: self.runtime.agent(self.key).get('turnId') not in (None, self.turn))
        self.runtime.reconcile_turn(self.key)  # Native evidence only names the old turn.
        starts = [p for n, p in self.server.calls if n == 'turn/start']
        self.assertEqual(len(starts), 2)
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE id='next-task'").fetchone()[0], 1)

    def test_storage_failure_can_recover_later_without_replaying_commands(self):
        import sqlite3
        original = self.runtime.notification
        self.runtime.notification = lambda *args: (_ for _ in ()).throw(sqlite3.OperationalError('disk I/O error'))
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
        self.assertTrue(self.runtime.agent(self.key)['inFlight'])
        self.runtime.notification = original
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'reconciled')
        self.assertEqual(len([n for n, _ in self.server.calls if n == 'turn/start']), 1)

    def test_replacement_connection_invalidates_old_preparation_cache(self):
        import concurrent.futures
        self.assertIn(self.key, self.runtime.loaded)
        self.runtime.connection_ids['default'] = 'replacement'
        future = self.runtime.prepare_locked(self.runtime.agent(self.key))
        self.assertIsInstance(future, concurrent.futures.Future)
        future.result(timeout=3)
        self.assertEqual(len([n for n, _ in self.server.calls if n == 'thread/resume']), 1)
        self.assertEqual(self.runtime.preparations[self.key]['connectionId'], 'replacement')

    def test_scheduler_recovers_without_another_user_message(self):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            a['activity'] = {'phase': 'writing', 'at': time.time() - 130}
            a['lastEvent'] = '2020-01-01T00:00:00Z'
            self.runtime.put(db, 'agents', a)
        self.runtime.changed.set()
        fixture.eventually(lambda: not self.runtime.agent(self.key)['inFlight'])
        self.assertEqual(self.runtime.agent(self.key)['status'], 'completed')

    def test_live_installer_preserves_connections_and_uses_real_dispatch(self):
        import types
        import codex_runtime
        from codex_turn_recovery_update import apply, BASE_DISPATCH, BASE_PREPARE
        from codex_efficiency_update import fingerprint
        source = Path(codex_runtime.__file__).read_text().replace('            self.queue_turn_recovery(agents)\n', '')
        source = source.replace('''        if previous and previous.get("connectionId") != self.connection_ids.get(a.get("accountKey", "default")):
            # Disconnect persistence can fail when storage is unavailable. An old
            # process's loaded cache and preparation future cannot survive replacement.
            self.loaded.discard(a["id"])
            previous = None
''', '')
        module = compile(source, codex_runtime.__file__, 'exec', dont_inherit=True)
        cls = next(c for c in module.co_consts if isinstance(c, types.CodeType) and c.co_name == 'Runtime')
        code = next(c for c in cls.co_consts if isinstance(c, types.CodeType) and c.co_name == 'dispatch')
        self.runtime.dispatch = types.MethodType(types.FunctionType(code, self.runtime.dispatch.__func__.__globals__), self.runtime)
        self.assertEqual(fingerprint(self.runtime.dispatch), BASE_DISPATCH)
        prepare = next(c for c in cls.co_consts if isinstance(c, types.CodeType) and c.co_name == 'prepare_locked')
        self.runtime.prepare_locked = types.MethodType(types.FunctionType(prepare, self.runtime.prepare_locked.__func__.__globals__), self.runtime)
        self.assertEqual(fingerprint(self.runtime.prepare_locked), BASE_PREPARE)
        connections = dict(self.runtime.servers)
        pool = self.runtime.recovery_pool
        self.assertEqual(apply(self.runtime)['status'], 'applied')
        self.assertEqual(apply(self.runtime)['status'], 'already_applied')
        self.assertEqual(self.runtime.servers, connections)
        self.assertIs(self.runtime.recovery_pool, pool)
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            a['activity'] = {'at': time.time() - 130}
            a['lastEvent'] = '2020-01-01T00:00:00Z'
            self.runtime.put(db, 'agents', a)
        self.runtime.dispatch()
        fixture.eventually(lambda: not self.runtime.agent(self.key)['inFlight'])

    def test_installer_rejects_unknown_dispatch_without_changes(self):
        import types
        from codex_turn_recovery_update import apply
        self.runtime.dispatch = types.MethodType(lambda self: None, self.runtime)
        before = self.runtime.dispatch
        with self.assertRaisesRegex(RuntimeError, 'Unknown scheduler'):
            apply(self.runtime)
        self.assertIs(self.runtime.dispatch, before)

    def test_scheduler_probes_one_at_a_time_without_holding_runtime_lock(self):
        self.server.native['status']['type'] = 'active'
        self.server.read_gate = threading.Event()
        with self.runtime.lock:
            self.runtime.queue_turn_recovery([self.a], force_id=self.key)
            self.runtime.queue_turn_recovery([self.a], force_id=self.key)
        self.assertTrue(self.server.read_entered.wait(2))
        self.assertTrue(self.runtime.lock.acquire(timeout=.2))
        self.runtime.lock.release()
        self.assertEqual(len([n for n, _ in self.server.calls if n == 'thread/read']), 1)
        self.server.read_gate.set()
        fixture.eventually(lambda: not self.runtime._turn_recovery_busy)
        self.runtime.queue_turn_recovery([self.a])
        self.assertEqual(len([n for n, _ in self.server.calls if n == 'thread/read']), 1)


if __name__ == '__main__':
    unittest.main()

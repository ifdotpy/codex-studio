#!/usr/bin/env python3
"""Claude session controls against isolated SQLite and a fake native provider."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing, contextmanager
import importlib.util
import json
from pathlib import Path
import queue
import sqlite3
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_claude_controls import action, retire_idle_bridge


class Runtime(f.Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.05)
            self.changed.clear()


class Server(f.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.state = {'settings': {}, 'turns': [{'id': 't1', 'status': 'completed'}, {'id': 't2', 'status': 'completed'}], 'tasks': []}
        self.fail = False
        self.queue = []
        self.receipts = {}
        self.forks = 0
        self.before_reply = None

    def call(self, method, params, timeout=60):
        if method == 'claude/state':
            return copy.deepcopy(self.state)
        if method == 'thread/queue/list':
            return {'items': self.queue}
        if method == 'claude/settings':
            self.state['settings'] = params['settings']
            return {'settings': params['settings']}
        if method == 'thread/rollback':
            if params['requestId'] not in self.receipts:
                self.forks += 1
                self.receipts[params['requestId']] = {'threadId': params['threadId'], 'nativeId': 'fork', 'removedTurns': 1}
                self.state['turns'] = self.state['turns'][:1]
            if self.before_reply:
                self.before_reply()
            if self.fail:
                raise RuntimeError('lost response')
            return self.receipts[params['requestId']]
        return super().call(method, params, timeout)


class Controls(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.rt = Runtime(Path(self.tmp.name), Server)
        agent = self.rt.new_lead({})
        self.key = agent['id']
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.key, db)
            agent.update(cwd=self.tmp.name, provider='claude', threadId='logical', status='completed', autoWake=False, inFlight=False)
            self.rt.put(db, 'agents', agent)
            self.rt.item(db, self.key, 'old', 'assistant', 'keep', turnId='t1')
            self.rt.item(db, self.key, 'new', 'assistant', 'remove', turnId='t2')
        get = self.rt.accounts.get
        self.account_patch = patch.object(self.rt.accounts, 'get', side_effect=lambda key: {**get(key), 'provider': 'claude', 'status': 'ready'})
        self.account_patch.start()
        self.server = self.rt.connect()

    def tearDown(self):
        self.account_patch.stop()
        self.rt.close()
        self.tmp.cleanup()

    def call(self, kind, **body):
        return action(self.rt, {'id': self.key, 'action': kind, **body})

    def records(self):
        with self.rt.db() as db:
            return [json.loads(r[0]) for r in db.execute('SELECT record FROM runtime_items WHERE agent=? ORDER BY created', (self.key,))]

    @contextmanager
    def path_gate(self, kind, **body):
        entered, release = threading.Event(), threading.Event()
        resolve = Path.resolve

        def blocked(path, *args, **kwargs):
            if str(path) == self.tmp.name and not entered.is_set():
                entered.set()
                if not release.wait(3):
                    raise TimeoutError('The fixture path gate expired')
            return resolve(path, *args, **kwargs)

        with patch.object(Path, 'resolve', blocked), ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(self.call, kind, **body)
            try:
                self.assertTrue(entered.wait(3), 'The real action must reach its path preflight')
                yield future, release
            finally:
                release.set()

    def test_slow_path_checks_release_the_runtime_lock_for_both_controls(self):
        with self.rt.db() as db:
            db.execute('CREATE TABLE fixture_control_probe(id TEXT PRIMARY KEY)')
        for kind, body in [('settings', {'settings': {}, 'request_id': 'path-settings'}),
                           ('rollback', {'turn_id': 't2', 'request_id': 'path-rollback'})]:
            with self.subTest(kind=kind), self.path_gate(kind, **body) as (future, release):
                acquired = self.rt.lock.acquire(blocking=False)
                if acquired:
                    self.rt.lock.release()
                self.assertTrue(acquired, 'Path.resolve must not hold Runtime.lock')
                with closing(sqlite3.connect(self.rt.db_path, timeout=0)) as writer, writer:
                    writer.execute('INSERT INTO fixture_control_probe VALUES (?)', (kind,))
                release.set()
                future.result(timeout=3)
        self.assertEqual(self.server.forks, 1)

    def test_epoch_change_during_path_checks_rejects_the_action(self):
        with self.path_gate('rollback', turn_id='t2', request_id='epoch') as (future, release):
            with self.rt.lock, self.rt.db() as db:
                agent = self.rt.agent(self.key, db)
                agent['epoch'] += 1
                self.rt.put(db, 'agents', agent)
            release.set()
            with self.assertRaisesRegex(ValueError, 'session changed'):
                future.result(timeout=3)
        self.assertEqual(self.server.forks, 0)
        self.assertTrue(all('afterRestore' not in row for row in self.records()))

    def test_source_change_during_path_checks_rejects_the_action(self):
        with self.path_gate('settings', settings={}, request_id='source') as (future, release):
            with self.rt.lock, self.rt.db() as db:
                agent = self.rt.agent(self.key, db)
                agent['threadId'] = 'replacement-thread'
                self.rt.put(db, 'agents', agent)
            release.set()
            with self.assertRaisesRegex(ValueError, 'session changed'):
                future.result(timeout=3)
        self.assertEqual(self.server.state['settings'], {})
        self.assertIsNone(self.rt.agent(self.key).get('workspaceOperation'))

    def test_new_queued_input_during_path_checks_blocks_rollback(self):
        with self.path_gate('rollback', turn_id='t2', request_id='queue-race') as (future, release):
            with self.rt.lock, self.rt.db() as db:
                agent = self.rt.agent(self.key, db)
                agent['autoWake'] = True
                self.rt.put(db, 'agents', agent)
                self.rt.enqueue(db, agent, 'user', 'queued', 'path-queued')
            release.set()
            with self.assertRaisesRegex(ValueError, 'queued messages'):
                future.result(timeout=3)
        self.assertEqual(self.server.forks, 0)

    def test_busy_workspace_during_path_checks_blocks_settings(self):
        peer = self.rt.new_lead({'cwd': self.tmp.name})
        with self.path_gate('settings', settings={'thinking': True}, request_id='busy-race') as (future, release):
            with self.rt.lock, self.rt.db() as db:
                peer = self.rt.agent(peer['id'], db)
                peer.update(cwd=self.tmp.name, inFlight=True)
                self.rt.put(db, 'agents', peer)
            release.set()
            with self.assertRaisesRegex(ValueError, 'Workspace activity changed'):
                future.result(timeout=3)
        self.assertEqual(self.server.state['settings'], {})

    def test_completed_receipt_retry_skips_path_checks(self):
        result = self.call('rollback', turn_id='t2', request_id='completed-path')
        with patch.object(Path, 'resolve', side_effect=AssertionError('A receipt retry must not resolve paths')):
            self.assertEqual(self.call('rollback', turn_id='t2', request_id='completed-path'), result)
        self.assertEqual(self.server.forks, 1)

    def test_workspace_operation_entity_tracks_real_rollback_producer(self):
        entered, release = threading.Event(), threading.Event()
        def pause_before_reply():
            entered.set()
            if not release.wait(3):
                raise TimeoutError('rollback receipt gate expired')
        self.server.before_reply = pause_before_reply
        with ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(self.call, 'rollback', turn_id='t2', request_id='entity-rollback')
            try:
                self.assertTrue(entered.wait(3))
                with self.rt.db() as db:
                    payload = db.execute(
                        "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                        (self.key,)).fetchone()[0]
                self.assertEqual(json.loads(payload)['value']['workspaceOperation'], 'branch')
            finally:
                release.set()
            future.result(timeout=3)
        self.assertIsNone(self.rt.agent(self.key).get('workspaceOperation'))
        with self.rt.db() as db:
            payload = db.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (self.key,)).fetchone()[0]
        self.assertIsNone(json.loads(payload)['value']['workspaceOperation'])

    def test_rollback_receipt_and_hidden_history(self):
        result = self.call('rollback', turn_id='t2', request_id='r')
        self.assertEqual(result['threadId'], 'logical')
        self.assertNotIn('afterRestore', self.records()[0])
        self.assertEqual(self.records()[1]['afterRestore'], 'claude-control:r')
        self.assertEqual(self.records()[1]['text'], 'remove')
        self.assertEqual(self.rt.agent(self.key)['status'], 'paused')
        self.assertEqual(self.call('rollback', turn_id='t2', request_id='r'), result)
        self.assertEqual(self.server.forks, 1)
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.call('rollback', turn_id='t1', request_id='r')

    def test_lost_response_preserves_rows_and_recovers_same_request(self):
        self.server.fail = True
        with self.assertRaisesRegex(RuntimeError, 'lost response'):
            self.call('rollback', turn_id='t2', request_id='r')
        self.assertTrue(all('afterRestore' not in row for row in self.records()))
        self.assertEqual(self.call('state')['controlOperation']['requestId'], 'r')
        self.server.fail = False
        self.call('rollback', turn_id='t2', request_id='r')
        self.assertEqual(self.server.forks, 1)
        self.assertIn('afterRestore', self.records()[1])

    def test_queued_local_and_native_messages_block_rollback(self):
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.key, db); agent['autoWake'] = True; self.rt.put(db, 'agents', agent)
            self.rt.enqueue(db, agent, 'user', 'queued', 'event')
        with self.assertRaisesRegex(ValueError, 'queued messages'):
            self.call('rollback', turn_id='t2', request_id='r')
        with self.rt.db() as db:
            db.execute("UPDATE runtime_events SET status='cancelled' WHERE id='event'")
        self.server.queue = [{'id': 'steer'}]
        with self.assertRaisesRegex(ValueError, 'queued Claude input'):
            self.call('rollback', turn_id='t2', request_id='r2')
        self.assertEqual(self.server.forks, 0)

    def test_new_message_during_native_call_stays_queued(self):
        def enqueue():
            with self.rt.lock, self.rt.db() as db:
                agent = self.rt.agent(self.key, db); agent['autoWake'] = True; self.rt.put(db, 'agents', agent)
                self.rt.enqueue(db, agent, 'user', 'new message', 'later')
        self.server.before_reply = enqueue
        self.call('rollback', turn_id='t2', request_id='r')
        with self.rt.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='later'").fetchone()[0], 'pending')

    def test_settings_validation_and_no_thread(self):
        with self.assertRaisesRegex(ValueError, 'integer'):
            self.call('settings', settings={'autoCompactWindow': True})
        self.call('settings', settings={'permissionMode': 'plan', 'thinking': False})
        self.assertEqual(self.rt.agent(self.key)['claudeOptions']['permissionMode'], 'plan')
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.key, db); agent['threadId'] = None; self.rt.put(db, 'agents', agent)
        self.assertEqual(self.call('state')['turns'], [])
        self.call('settings', settings={'permissionMode': 'default'})

    def set_lazy_transfer(self):
        self.rt.accounts.get.side_effect = lambda account: {'provider': 'claude', 'status': 'ready'}
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.key, db)
            agent.update(accountKey='destination', accountTransferId='transfer',
                         lazyAccountTransfer={'id': 'transfer', 'sourceAccountKey': 'default',
                                              'sourceThreadId': 'source-thread'})
            self.rt.put(db, 'agents', agent)

    def test_lazy_transfer_state_reads_original_claude_profile(self):
        self.set_lazy_transfer()
        calls = []
        def native(method, params, timeout=60):
            calls.append((method, params))
            self.assertEqual(params['threadId'], 'source-thread')
            return copy.deepcopy(self.server.state)
        with patch.object(self.rt, 'connect', side_effect=lambda account: self.server if account == 'default' else self.fail('Destination has no source session')):
            with patch.object(self.server, 'call', side_effect=native):
                state = self.call('state')
        self.assertEqual(state['turns'], self.server.state['turns'])
        self.assertEqual(len(calls), 1)
        self.assertEqual(self.rt.agent(self.key)['accountTransferId'], 'transfer')

    def test_lazy_transfer_from_codex_has_no_claude_session_state(self):
        self.set_lazy_transfer()
        self.account_patch.stop()
        with patch.object(self.rt.accounts, 'get', side_effect=lambda key: {'provider': 'codex' if key == 'default' else 'claude'}):
            with patch.object(self.rt, 'connect', side_effect=AssertionError('No Claude source session')):
                state = self.call('state')
        self.assertEqual(state['turns'], [])
        self.assertEqual(state['tasks'], [])

    def test_transfer_blocks_session_mutations_before_native_access(self):
        self.set_lazy_transfer()
        for kind, body in [('settings', {'settings': {}, 'request_id': 'settings'}),
                           ('rollback', {'turn_id': 't2', 'request_id': 'rollback'}),
                           ('stop_task', {'task_id': 'task'})]:
            with self.subTest(kind=kind), patch.object(self.rt, 'connect', side_effect=AssertionError('Transfer owns the session')):
                with self.assertRaisesRegex(ValueError, 'account transfer'):
                    self.call(kind, **body)

    def test_idle_bridge_upgrade_preserves_tasks_and_invalidates_before_close(self):
        self.server.initialize_result = {'capabilities': {'claudeVersion': 2}}
        self.server.provider_options = {}
        self.server.pending = {}
        self.server.callbacks = queue.Queue()
        self.server.state['tasks'] = [{'task_id': 'background'}]
        self.assertFalse(retire_idle_bridge(self.rt, 'default', {'claudeOptions': {'customModels': []}}, self.server))
        self.server.state['tasks'] = []
        previous = self.rt.connection_ids['default']
        def close():
            self.assertNotEqual(self.rt.connection_ids['default'], previous)
            self.server.closed = True
        self.server.close = close
        self.assertTrue(retire_idle_bridge(self.rt, 'default', {'claudeOptions': {'customModels': []}}, self.server))
        self.assertNotIn('default', self.rt.servers)

    def test_idle_supervised_upgrade_closes_exact_process_before_replacement(self):
        from types import SimpleNamespace
        self.server.initialize_result = {'capabilities': {'claudeVersion': 14}}
        self.server.provider_options = {}
        self.server.pending = {}
        self.server.callbacks = queue.Queue()
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(handle='account:default', generation=2, call=lambda action: {'pid': 123, 'returnCode': None})
        import sqlite3
        with sqlite3.connect(self.rt.root / 'supervisor.sqlite3') as journal:
            journal.execute('CREATE TABLE handles(id TEXT,pid INTEGER,generation INTEGER,signature TEXT,closed_at REAL)')
            journal.execute('CREATE TABLE child_identities(handle TEXT,pid INTEGER,start_time TEXT)')
            journal.execute("INSERT INTO handles VALUES('account:default',123,2,'exact',NULL)")
            journal.execute("INSERT INTO child_identities VALUES('account:default',123,'verified')")
        journal.close()
        previous = self.rt.connection_ids['default']
        def close_native(*args):
            self.assertNotEqual(self.rt.connection_ids['default'], previous)
            self.assertNotIn('default', self.rt.servers)
            self.assertEqual(args, (self.rt.root, 'account:default', 123, 'verified', 'exact'))
            return {'closed': True}
        with patch('codex_process_supervisor.process_start_time', return_value='verified'), \
                patch('codex_process_supervisor.admin_close_handle', side_effect=close_native) as close:
            self.assertTrue(retire_idle_bridge(self.rt, 'default', {}, self.server))
            close.assert_called_once()
        self.assertNotIn('default', self.rt._native_tools_retiring)

    def test_unsupported_close_restores_the_original_idle_connection(self):
        from types import SimpleNamespace
        import sqlite3
        self.server.initialize_result = {'capabilities': {'claudeVersion': 14}}
        self.server.provider_options = {}
        self.server.pending = {}
        self.server.callbacks = queue.Queue()
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(handle='account:default', generation=2,
            call=lambda action: {'pid': 123, 'returnCode': None})
        journal = sqlite3.connect(self.rt.root / 'supervisor.sqlite3')
        try:
            journal.executescript("CREATE TABLE handles(id TEXT,pid INTEGER,generation INTEGER,signature TEXT,closed_at REAL);"
                "CREATE TABLE child_identities(handle TEXT,pid INTEGER,start_time TEXT);"
                "INSERT INTO handles VALUES('account:default',123,2,'exact',NULL);"
                "INSERT INTO child_identities VALUES('account:default',123,'verified');")
        finally:
            journal.close()
        previous = self.rt.connection_ids['default']
        with patch('codex_process_supervisor.process_start_time', return_value='verified'), \
                patch('codex_process_supervisor.admin_close_handle', side_effect=RuntimeError('Unknown supervisor action')):
            self.assertFalse(retire_idle_bridge(self.rt, 'default', {}, self.server))
        self.assertEqual(self.rt.connection_ids['default'], previous)
        self.assertIs(self.rt.servers['default'], self.server)
        self.assertNotIn('default', self.rt._native_tools_retiring)
        self.assertFalse(self.server.closed)

    def test_older_supervisor_keeps_idle_bridge_and_connection(self):
        from types import SimpleNamespace
        self.server.initialize_result = {'capabilities': {'claudeVersion': 14}}
        self.server.provider_options = {}
        self.server.pending = {}
        self.server.callbacks = queue.Queue()
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(handle='account:default', generation=2,
            call=lambda action: (_ for _ in ()).throw(RuntimeError('Unknown supervisor action')))
        previous = self.rt.connection_ids['default']
        self.assertFalse(retire_idle_bridge(self.rt, 'default', {}, self.server))
        self.assertEqual(self.rt.connection_ids['default'], previous)
        self.assertIs(self.rt.servers['default'], self.server)

    def test_version_17_bridge_retires_only_after_active_work_finishes(self):
        self.server.initialize_result = {'capabilities': {'claudeVersion': 17}}
        self.server.provider_options = {}
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.key, db)
            agent.update(status='running', inFlight=True)
            self.rt.put(db, 'agents', agent)
        self.assertFalse(retire_idle_bridge(self.rt, 'default', {'claudeOptions': {}}, self.server))
        self.assertIs(self.rt.servers['default'], self.server)
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.key, db)
            agent.update(status='completed', inFlight=False)
            self.rt.put(db, 'agents', agent)
        self.assertTrue(retire_idle_bridge(self.rt, 'default', {'claudeOptions': {}}, self.server))
        self.assertNotIn('default', self.rt.servers)

    def test_current_bridge_version_stays_and_checks_native_tasks(self):
        self.server.initialize_result = {'capabilities': {'claudeVersion': 23}}
        self.server.provider_options = {}
        self.assertFalse(retire_idle_bridge(self.rt, 'default', {'claudeOptions': {}}, self.server))
        self.server.state['tasks'] = [{'task_id': 'background'}]
        self.assertFalse(retire_idle_bridge(self.rt, 'default', {'claudeOptions': {'customModels': []}}, self.server))

    def test_version_22_same_options_retires_only_after_idle_is_proven(self):
        self.server.initialize_result = {'capabilities': {'claudeVersion': 22}}
        self.server.provider_options = {}
        account = {'claudeOptions': {}}
        previous = self.rt.connection_ids['default']
        self.server.state['tasks'] = [{'task_id': 'background'}]
        self.assertFalse(retire_idle_bridge(self.rt, 'default', account, self.server))
        self.server.state['tasks'] = []
        self.server.queue = [{'id': 'native-steer'}]
        self.assertFalse(retire_idle_bridge(self.rt, 'default', account, self.server))
        self.server.queue = []
        self.server.pending = {'native-request': object()}
        self.assertFalse(retire_idle_bridge(self.rt, 'default', account, self.server))
        self.server.pending = {}
        with self.rt.lock, self.rt.db() as db:
            self.rt.put(db, 'monitors', {'id': 'version-monitor', 'agent': self.key, 'status': 'running'})
        self.assertFalse(retire_idle_bridge(self.rt, 'default', account, self.server))
        self.assertEqual(self.rt.connection_ids['default'], previous)
        self.assertIs(self.rt.servers['default'], self.server)
        self.assertFalse(self.server.closed)
        with self.rt.db() as db:
            db.execute("DELETE FROM runtime_monitors WHERE id='version-monitor'")
        self.assertTrue(retire_idle_bridge(self.rt, 'default', account, self.server))
        self.assertNotEqual(self.rt.connection_ids['default'], previous)
        self.assertNotIn('default', self.rt.servers)
        self.assertTrue(self.server.closed)

    def test_bridge_upgrade_waits_for_studio_monitors(self):
        self.server.initialize_result = {'capabilities': {'claudeVersion': 2}}
        self.server.provider_options = {}
        for status in ('approval', 'starting', 'running'):
            with self.subTest(status=status), self.rt.lock, self.rt.db() as db:
                self.rt.put(db, 'monitors', {'id': 'monitor', 'agent': self.key, 'status': status})
            self.assertFalse(retire_idle_bridge(self.rt, 'default', {}, self.server))
            self.assertIs(self.rt.servers['default'], self.server)


if __name__ == '__main__':
    unittest.main()

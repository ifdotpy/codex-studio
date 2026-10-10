#!/usr/bin/env python3
"""A saved tool response releases the exact waiter without a second operation."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
from contextlib import closing, contextmanager
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_tool_response_recovery import recover, response_operation_id, tick


class SavedResponseRecovery(unittest.TestCase):
    from codex_runtime import Runtime
    agent_connection = Runtime.agent_connection
    server_for = Runtime.server_for

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-tool-response-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.lock = threading.RLock()
        self.start_lock = threading.RLock()
        self.closed = False
        self.agent_record = {'id': 'lead', 'provider': 'claude', 'accountKey': 'default',
                             'epoch': 0, 'threadId': 'thread', 'turnId': 'turn',
                             'status': 'running', 'autoWake': True, 'inFlight': True,
                             'activeTools': [{'id': 'toolu_native', 'type': 'mcpToolCall',
                                              'name': 'mcp__studio__orchestration_task'}]}
        self.rpc = 'claude:521a0f98-44ab-487d-b64c-eb5ef5fc14b2'
        self.result = {'success': True, 'contentItems': [{'type': 'inputText', 'text': 'accepted'}]}
        self.record = {'id': 'exact-request', 'agent': 'lead', 'threadId': 'thread', 'turnId': 'turn',
                       'epoch': 0, 'accountKey': 'default', 'connectionId': 'old-backend',
                       'rpcId': self.rpc, 'tool': 'orchestration_task', 'stage': 'completed',
                       'outcome': 'applied', 'created': 10, 'updated': time.time() - 60,
                       'result': copy.deepcopy(self.result)}
        self.waiter = threading.Event()
        self.responses = []
        self.operation_calls = 1  # The original accept already committed.
        self.servers = {'default': SimpleNamespace(supervisor_mode=True, write_lock=threading.RLock(),
                         proc=SimpleNamespace(root=self.root, handle='account:default', generation=2))}
        self.connection_ids = {'default': 'new-backend'}
        with closing(sqlite3.connect(self.root / 'canvas.sqlite3')) as db, db:
            db.execute('CREATE TABLE runtime_tool_requests(id TEXT PRIMARY KEY,record TEXT)')
            db.execute('CREATE INDEX actor ON runtime_tool_requests('
                       "json_extract(record,'$.agent'), json_extract(record,'$.updated') DESC)")
        with closing(sqlite3.connect(self.root / 'supervisor.sqlite3')) as db, db:
            db.executescript('CREATE TABLE handles(id TEXT,pid INTEGER,generation INTEGER,closed_at REAL);'
                             'CREATE TABLE child_identities(handle TEXT,pid INTEGER,start_time TEXT);'
                             'CREATE TABLE operations(handle TEXT,native_id TEXT,operation_id TEXT);')
            db.execute("INSERT INTO handles VALUES ('account:default',123,2,NULL)")
            db.execute("INSERT INTO child_identities VALUES ('account:default',123,'1.000000')")
        self.save()
        self.addCleanup(patch.stopall)
        patch('codex_process_supervisor.process_start_matches', return_value=True).start()

    @contextmanager
    def db(self):
        with closing(sqlite3.connect(self.root / 'canvas.sqlite3')) as db, db:
            yield db

    read_db = db

    def agent(self, key, db=None):
        self.assertEqual(key, 'lead')
        return copy.deepcopy(self.agent_record)

    def connection_current(self, account, connection):
        return self.connection_ids.get(account) == connection

    def reply(self, message, account, connection, *, operation_id):
        self.assertTrue(self.connection_current(account, connection))
        self.assertEqual(operation_id, response_operation_id(self, account, message['id']))
        with closing(sqlite3.connect(self.root / 'supervisor.sqlite3')) as db, db:
            if db.execute('SELECT 1 FROM operations WHERE operation_id=?', (operation_id,)).fetchone():
                return {'accepted': True, 'duplicate': True}
            db.execute('INSERT INTO operations VALUES (?,?,?)', ('account:default', message['id'], operation_id))
        self.responses.append(copy.deepcopy(message))
        if message['id'] == self.rpc:
            self.waiter.set()
        return {'accepted': True, 'duplicate': False}

    def put(self, db, table, record):
        self.assertEqual(table, 'tool_requests')
        db.execute('UPDATE runtime_tool_requests SET record=? WHERE id=?', (json.dumps(record), record['id']))

    def save(self):
        with self.db() as db:
            db.execute('INSERT OR REPLACE INTO runtime_tool_requests VALUES (?,?)',
                       (self.record['id'], json.dumps(self.record)))

    def no_response(self):
        recover(self, 'lead')
        self.assertFalse(self.waiter.is_set())
        self.assertEqual(self.responses, [])
        self.assertEqual(self.operation_calls, 1)

    def test_surviving_child_receives_saved_result_once_and_waiter_continues(self):
        result = recover(self, 'lead')
        self.assertEqual(result['status'], 'tool_response_delivered')
        self.assertTrue(self.waiter.is_set())
        self.assertEqual(self.responses, [{'id': self.rpc, 'result': self.result}])
        self.assertEqual(self.operation_calls, 1)
        recover(self, 'lead')
        self.assertEqual(len(self.responses), 1)
        self.assertEqual(self.agent_record['activeTools'][0]['id'], 'toolu_native')

    def test_unknown_outcome_returns_unknown_without_repeating_accept(self):
        self.record.update(stage='interrupted', outcome='unknown', result={'success': False,
                           'contentItems': [{'type': 'inputText', 'text': 'Tool outcome unknown'}]})
        self.save()
        recover(self, 'lead')
        self.assertEqual(self.responses[0]['result'], self.record['result'])
        with self.db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_tool_requests').fetchone()[0])
        self.assertEqual(saved['outcome'], 'unknown')
        self.assertEqual(self.operation_calls, 1)

    def test_external_result_remains_complete_after_delivery_metadata_write(self):
        from codex_payloads import EXTERNALIZE_THRESHOLD, ensure_payload_schema, externalize_record, resolve_record
        full = {'success': True, 'contentItems': [{'type': 'inputText', 'text': 'x' * (EXTERNALIZE_THRESHOLD + 1)}]}
        with self.db() as db:
            ensure_payload_schema(db)
            self.record = externalize_record(self.root, db, 'tool_requests', {**self.record, 'result': full})
        self.assertIn('result', self.record['_payloadBlobs'])
        self.save()
        original = self.put
        def store(db, table, record):
            return original(db, table, externalize_record(self.root, db, table, record))
        with patch.object(self, 'put', side_effect=store):
            recover(self, 'lead')
        self.assertEqual(self.responses[0]['result'], full)
        with self.db() as db:
            raw = json.loads(db.execute('SELECT record FROM runtime_tool_requests').fetchone()[0])
        self.assertEqual(resolve_record(self.root, raw)['result'], full)
        self.assertEqual(raw['nativeDelivery']['stage'], 'written')

    def test_accepted_write_with_unknown_stdin_outcome_is_not_repeated(self):
        with closing(sqlite3.connect(self.root / 'supervisor.sqlite3')) as db, db:
            db.execute('INSERT INTO operations VALUES (?,?,?)', ('account:default', self.rpc, 'old-random-id'))
        self.no_response()

    def test_acceptance_race_never_reports_unknown_write_as_delivery(self):
        from codex_tool_response_recovery import _native_proof
        def proof(*args, **kwargs):
            result = _native_proof(*args, **kwargs)
            with closing(sqlite3.connect(self.root / 'supervisor.sqlite3')) as db, db:
                db.execute('INSERT INTO operations SELECT ?,?,? WHERE NOT EXISTS '
                           '(SELECT 1 FROM operations WHERE native_id=?)',
                           ('account:default', self.rpc, 'legacy-random-write', self.rpc))
            return result
        with patch('codex_tool_response_recovery._native_proof', side_effect=proof):
            result = recover(self, 'lead')
        self.assertEqual(result['status'], 'superseded')
        self.assertFalse(self.waiter.is_set())
        with self.db() as db:
            receipt = json.loads(db.execute('SELECT record FROM runtime_tool_requests').fetchone()[0])
        self.assertNotIn('nativeDelivery', receipt)

    def test_current_long_tool_without_durable_result_remains_active(self):
        self.record.update(stage='running', outcome='pending')
        self.record.pop('result')
        self.save()
        self.no_response()

    def test_recent_completion_leaves_time_for_normal_response(self):
        self.record['updated'] = time.time()
        self.save()
        self.no_response()

    def test_actor_scope_stop_and_native_rpc_identity_guards(self):
        original = copy.deepcopy(self.agent_record)
        for change in ({'autoWake': False}, {'epoch': 1}, {'threadId': 'other'},
                       {'turnId': 'other'}, {'status': 'paused'}, {'deletedAt': 1},
                       {'provider': 'codex'}, {'activeTools': [{'type': 'commandExecution', 'name': 'sleep'}]}):
            with self.subTest(change=change):
                self.agent_record = {**original, **change}
                self.no_response()
        self.agent_record = original
        self.record['rpcId'] = 'not-the-original-rpc'
        self.save()
        self.no_response()

    def test_new_native_child_cannot_receive_legacy_response(self):
        with closing(sqlite3.connect(self.root / 'supervisor.sqlite3')) as db, db:
            db.execute("UPDATE child_identities SET start_time='11.000000'")
        self.no_response()

    def test_saved_generation_mismatch_blocks_response(self):
        self.record['supervisor'] = {'stateDir': str(self.root), 'handle': 'account:default', 'generation': 1}
        self.save()
        self.no_response()

    def test_unproven_native_process_blocks_response(self):
        with patch('codex_process_supervisor.process_start_matches', return_value=False):
            self.no_response()

    def test_change_while_loading_payload_blocks_response(self):
        from codex_payloads import resolve_record
        def load(*args, **kwargs):
            self.agent_record['epoch'] += 1
            return resolve_record(*args, **kwargs)
        with patch('codex_payloads.resolve_record', side_effect=load):
            self.no_response()

    def test_receipt_change_while_loading_payload_blocks_response(self):
        from codex_payloads import resolve_record
        def load(*args, **kwargs):
            self.record['result'] = {'success': False}
            self.save()
            return resolve_record(*args, **kwargs)
        with patch('codex_payloads.resolve_record', side_effect=load):
            self.no_response()

    def test_result_delivery_record_failure_does_not_send_a_second_response(self):
        with patch.object(self, 'put', side_effect=sqlite3.OperationalError('database is locked')):
            with self.assertRaises(sqlite3.OperationalError):
                recover(self, 'lead')
        recover(self, 'lead')
        self.assertEqual(len(self.responses), 1)
        self.assertEqual(self.operation_calls, 1)

    def test_native_wait_does_not_block_unrelated_runtime_operations(self):
        entered, release = threading.Event(), threading.Event()
        original = self.reply
        def waiting_reply(*args, **kwargs):
            entered.set()
            if not release.wait(2):
                raise AssertionError('The fixture reply did not finish')
            return original(*args, **kwargs)
        worker = threading.Thread(target=lambda: recover(self, 'lead'), daemon=True)
        with patch.object(self, 'reply', side_effect=waiting_reply):
            try:
                worker.start()
                self.assertTrue(entered.wait(1))
                self.assertTrue(self.lock.acquire(timeout=.15), 'A native wait holds Runtime.lock')
                self.lock.release()
                self.assertFalse(self.start_lock.acquire(blocking=False),
                                 'The exact native handle can change during the write')
            finally:
                release.set()
                worker.join(2)
        self.assertFalse(worker.is_alive())
        self.assertTrue(self.waiter.is_set())

    def test_busy_native_write_skips_recovery_without_joining_its_queue(self):
        entered, release = threading.Event(), threading.Event()
        def writer():
            with self.servers['default'].write_lock:
                entered.set()
                release.wait(2)
        worker = threading.Thread(target=writer, daemon=True)
        try:
            worker.start()
            self.assertTrue(entered.wait(1))
            started = time.monotonic()
            self.no_response()
            self.assertLess(time.monotonic() - started, .15)
        finally:
            release.set()
            worker.join(2)

    def test_timer_limits_one_job_per_account_and_retry_time(self):
        workers = []
        class Worker:
            def __init__(self, **kwargs):
                workers.append(kwargs)
            def start(self):
                pass
        with patch('codex_tool_response_recovery.threading.Thread', Worker):
            tick(self, [self.agent_record] * 30)
            tick(self, [self.agent_record])
        self.assertEqual(len(workers), 1)


if __name__ == '__main__':
    unittest.main()

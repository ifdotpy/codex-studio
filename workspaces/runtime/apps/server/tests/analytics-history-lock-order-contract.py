#!/usr/bin/env python3
"""History budgets precede analytics writes and remain exact after retries."""

from test_isolation import isolate_supervisor_environment

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from server_test_waits import HANG_GUARD_SECONDS
isolate_supervisor_environment()
from runtime_test_environment import make_runtime

import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_analytics
import codex_analytics_history as history
import codex_runtime
import codex_sqlite
import codex_rename


class Runtime(fixture.Runtime):
    def schedule(self):
        pass


class HistoryLockOrder(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-history-lock-order-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.runtime = make_runtime(Runtime, self.root / 'state', fixture.FakeServer)
        self.addCleanup(self.runtime.close)
        self.agent = self.runtime.create({'name': 'History', 'cwd': str(self.root), 'prompt': ''},
                                         draft=True, defer=True)
        self.thread = '01a07781-5d19-7390-bc74-c12094143962'
        with self.runtime.lock, self.runtime.db() as db:
            self.agent = self.runtime.agent(self.agent['id'], db)
            self.agent.update(threadId=self.thread, created=1)
            self.runtime.put(db, 'agents', self.agent)
        self.home = self.root / 'native'
        (self.home / 'sessions').mkdir(parents=True)
        self.profile = patch.object(self.runtime.accounts, 'home', return_value=self.home)
        self.profile.start()
        self.addCleanup(self.profile.stop)
        self.path = self.home / 'sessions' / ('rollout-' + self.thread + '.jsonl')
        records = [{'type': 'session_meta', 'payload': {'id': self.thread}}]
        for response, amount, total in [('zero-response', 0, 0), ('exact-response', 100, 100)]:
            records.append({'type': 'token_usage_record', 'payload': {
                'thread_id': self.thread, 'turn_id': 'turn-one', 'response_id': response,
                'usage': {'total_tokens': amount}, 'thread_token_usage': {'total_tokens': total}}})
        self.path.write_text(''.join(json.dumps({'timestamp': '2026-09-06T18:00:00Z', **record}) + '\n'
                                    for record in records))

    def usage(self):
        with self.runtime.read_db() as db:
            budget = json.loads(db.execute('SELECT record FROM runtime_budget WHERE id=?',
                                           (self.agent['id'],)).fetchone()[0])
            rows = [tuple(row) for row in db.execute('SELECT kind,tokens FROM runtime_budget_usage ORDER BY tokens')]
        with self.runtime.analytics_db() as db:
            usage = [json.loads(row[0]) for row in db.execute('SELECT record FROM analytics_usage ORDER BY seq')]
            checkpoint = json.loads(db.execute('SELECT record FROM analytics_history WHERE agent=?',
                                               (self.agent['id'],)).fetchone()[0])
        return budget, rows, usage, checkpoint

    def test_wait_for_runtime_writer_does_not_hold_analytics_writer(self):
        entered = threading.Event()
        errors, captures = [], []
        original = history.budget_capture
        original_execute = codex_sqlite.InstrumentedConnection.execute
        def execute(db, sql, parameters=()):
            if threading.current_thread() is worker and sql == 'BEGIN IMMEDIATE':
                self.assertFalse(self.runtime.lock._is_owned())
                entered.set()
            return original_execute(db, sql, parameters)
        def capture(*args, **kwargs):
            self.assertTrue(self.runtime.lock._is_owned())
            captures.append(kwargs.get('source'))
            return original(*args, **kwargs)
        def step():
            try:
                self.runtime.analytics_history_step()
            except Exception as error:
                errors.append(error)
        main = sqlite3.connect(self.runtime.db_path)
        main.execute('BEGIN IMMEDIATE')
        worker = threading.Thread(target=step)
        try:
            with patch.object(history, 'budget_capture', side_effect=capture), patch.object(
                    codex_analytics, 'budget_capture', side_effect=capture) as analytics_capture, patch.object(
                    codex_sqlite.InstrumentedConnection, 'execute', execute):
                worker.start()
                self.assertTrue(entered.wait(HANG_GUARD_SECONDS))
                self.assertTrue(self.runtime.lock.acquire(timeout=HANG_GUARD_SECONDS),
                                'History writer admission must leave runtime state accessible')
                self.runtime.lock.release()
                analytics = sqlite3.connect(self.runtime.analytics_db_path, timeout=.05)
                try:
                    analytics.execute('BEGIN IMMEDIATE')
                finally:
                    analytics.rollback()
                    analytics.close()
                main.rollback()
                worker.join(HANG_GUARD_SECONDS)
                self.assertFalse(worker.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(analytics_capture.call_count, 0)
        finally:
            main.rollback()
            main.close()
            if worker.ident is not None:
                worker.join(3)
        budget, rows, usage, checkpoint = self.usage()
        self.assertEqual(captures, ['rollout', 'rollout'])
        self.assertEqual(budget['spent'], 100)
        self.assertEqual(rows, [('response', 0), ('response', 100)])
        self.assertEqual({row['responseId'] for row in usage}, {'zero-response', 'exact-response'})
        self.assertEqual(checkpoint['status'], 'current')
        self.assertEqual(checkpoint['coverage'], 'availableRecords')

    def test_one_durable_budget_commit_per_page_under_runtime_lock(self):
        commits, captures = [], []
        original_capture = history.budget_capture
        original_exit = codex_sqlite.InstrumentedConnection.__exit__

        def capture(db, *args, **kwargs):
            self.assertTrue(self.runtime.lock._is_owned())
            self.assertTrue(db.in_transaction)
            self.assertEqual(db.execute('PRAGMA synchronous').fetchone()[0], 2)
            captures.append(db)
            return original_capture(db, *args, **kwargs)

        def exit_scope(db, *args):
            if captures and db is captures[0] and db.in_transaction:
                self.assertTrue(self.runtime.lock._is_owned(),
                                'The page commit must retain whole-record lifecycle protection')
                commits.append(db)
            return original_exit(db, *args)

        with patch.object(history, 'budget_capture', side_effect=capture), patch.object(
                codex_sqlite.InstrumentedConnection, '__exit__', exit_scope):
            self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(len(captures), 2)
        self.assertIs(captures[0], captures[1])
        self.assertEqual(commits, [captures[0]])
        budget, rows, usage, checkpoint = self.usage()
        self.assertEqual(budget['spent'], 100)
        self.assertEqual(rows, [('response', 0), ('response', 100)])
        self.assertEqual(len(usage), 2)
        self.assertEqual(checkpoint['offset'], self.path.stat().st_size)

    def test_pending_whole_record_rename_precedes_history_budget_application(self):
        attempted = threading.Event()
        errors, captures = [], []
        original_acquire = self.runtime.lock.acquire
        original_capture = history.budget_capture
        original_put = self.runtime.put

        def acquire(*args, **kwargs):
            result = original_acquire(*args, **kwargs)
            if threading.current_thread() is worker and kwargs.get('blocking') is False and not result:
                attempted.set()
            return result

        def capture(*args, **kwargs):
            captures.append(True)
            attempted.set()
            return original_capture(*args, **kwargs)

        def step():
            try:
                self.runtime.analytics_history_step()
            except Exception as error:
                errors.append(error)

        worker = threading.Thread(target=step)

        def pending_rename(db, table, record, **kwargs):
            # Real _set_name has read the old budget into a whole agent record.
            # History must release its writer claim when this lock is occupied,
            # so rename can persist that record before the new budget is applied.
            worker.start()
            self.assertTrue(attempted.wait(HANG_GUARD_SECONDS))
            self.assertEqual(captures, [])
            self.assertEqual(record['tokensUsed'], 0)
            return original_put(db, table, record, **kwargs)

        try:
            with patch.object(self.runtime.lock, 'acquire', side_effect=acquire), patch.object(
                    history, 'budget_capture', side_effect=capture):
                with self.runtime.lock, self.runtime.db() as db:
                    with patch.object(self.runtime, 'put', side_effect=pending_rename):
                        codex_rename._set_name(self.runtime, db, self.agent['id'], 'Renamed during history')
                worker.join(HANG_GUARD_SECONDS)
                self.assertFalse(worker.is_alive())
                self.assertEqual(errors, [])
        finally:
            if worker.ident is not None:
                worker.join(HANG_GUARD_SECONDS)
        self.assertEqual(len(captures), 2)
        budget, rows, usage, checkpoint = self.usage()
        self.assertEqual(budget['spent'], 100)
        self.assertEqual(rows, [('response', 0), ('response', 100)])
        self.assertEqual({row['responseId'] for row in usage}, {'zero-response', 'exact-response'})
        with self.runtime.read_db() as db:
            current = self.runtime.agent(self.agent['id'], db)
        self.assertEqual(current['name'], 'Renamed during history')
        self.assertEqual(current['tokensUsed'], 100)
        self.assertEqual(checkpoint['status'], 'current')
        self.runtime.analytics_history_step()
        self.assertEqual(self.usage()[1], rows)

    def test_failed_budget_page_rolls_back_every_receipt_and_checkpoint(self):
        original = history.budget_capture
        captures = []

        def fail_second(db, *args, **kwargs):
            captures.append(True)
            result = original(db, *args, **kwargs)
            if len(captures) == 2:
                raise RuntimeError('Fixture budget page failure')
            return result

        with patch.object(history, 'budget_capture', side_effect=fail_second):
            with self.assertRaisesRegex(RuntimeError, 'budget page failure'):
                self.runtime.analytics_history_step()
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 0)
            self.assertIsNone(db.execute('SELECT record FROM runtime_budget WHERE id=?',
                                        (self.agent['id'],)).fetchone())
            agent = self.runtime.agent(self.agent['id'], db)
            self.assertEqual(agent['tokensUsed'], self.agent['tokensUsed'])
            self.assertNotIn('tokenUsageAccounting', agent)
        with self.runtime.analytics_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM analytics_usage').fetchone()[0], 0)
            self.assertEqual(db.execute('SELECT count(*) FROM analytics_history').fetchone()[0], 0)
        self.assertTrue(self.runtime.analytics_history_step())
        budget, rows, usage, checkpoint = self.usage()
        self.assertEqual(budget['spent'], 100)
        self.assertEqual(rows, [('response', 0), ('response', 100)])
        self.assertEqual(len(usage), 2)
        self.assertEqual(checkpoint['status'], 'current')

    def test_lost_analytics_write_reimports_exact_budget_receipts_once(self):
        original = self.runtime.analytics_event
        def fail(db, agent, method, payload, **kwargs):
            if method == 'thread/tokenUsage/updated':
                raise RuntimeError('Fixture analytics failure after budget commit')
            return original(db, agent, method, payload, **kwargs)
        with patch.object(self.runtime, 'analytics_event', side_effect=fail):
            with self.assertRaisesRegex(RuntimeError, 'after budget commit'):
                self.runtime.analytics_history_step()
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 2)
        with self.runtime.analytics_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM analytics_usage').fetchone()[0], 0)
            self.assertEqual(db.execute('SELECT count(*) FROM analytics_history').fetchone()[0], 0)
        self.assertTrue(self.runtime.analytics_history_step())
        # The next step copies the newly stored analytics receipts to the ledger.
        writer_sql = []
        original_connect = codex_runtime.sqlite_connect
        def connect(*args, **options):
            db = original_connect(*args, **options)
            if options.get('site') == 'Runtime.db':
                db.set_trace_callback(writer_sql.append)
            return db
        with patch.object(codex_runtime, 'sqlite_connect', side_effect=connect):
            self.assertTrue(self.runtime.analytics_history_step())
        self.assertFalse(any(sql.lower().startswith('insert into runtime_budget values')
                             for sql in writer_sql))
        self.assertFalse(any(sql.lower().startswith('update runtime_agents set record=json_set')
                             for sql in writer_sql))
        self.assertFalse(self.runtime.analytics_history_step())
        budget, rows, usage, checkpoint = self.usage()
        self.assertEqual(budget['spent'], 100)
        self.assertEqual(rows, [('response', 0), ('response', 100)])
        self.assertEqual(len(usage), 2)
        self.assertEqual(checkpoint['offset'], self.path.stat().st_size)

    def test_existing_usage_migration_keeps_exact_charge_identity_and_coverage(self):
        record = {'threadId': self.thread, 'turnId': 'older-turn', 'at': 2,
                  'responseId': 'older-response', 'rawTokenUsageRecord': {'response_id': 'older-response'},
                  'requestUsage': {'totalTokens': 35}, 'total': {'totalTokens': 35},
                  'last': {'totalTokens': 35}, 'timestampSource': 'record'}
        with self.runtime.analytics_db() as db:
            db.execute('INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
                       ('old', self.agent['id'], self.agent['id'], self.thread, 'older-turn', 2, json.dumps(record)))
        self.assertTrue(self.runtime.analytics_history_step())
        self.runtime.analytics_history_step()
        self.runtime.analytics_history_step()
        budget, rows, usage, checkpoint = self.usage()
        self.assertEqual(budget['spent'], 135)
        self.assertEqual(rows, [('response', 0), ('response', 35), ('response', 100)])
        self.assertEqual(len(usage), 3)
        self.assertEqual(checkpoint['coverage'], 'availableRecords')
        with self.runtime.analytics_db() as db:
            migration = json.loads(db.execute('SELECT value FROM analytics_meta WHERE key=?',
                ('budgetUsageMigrationV1:' + self.agent['id'],)).fetchone()[0])
        self.assertEqual(migration['cursor'], migration['end'])

    def test_account_or_thread_change_before_capture_does_not_import_old_scope(self):
        original = history.rollout_actions
        for change in ({'threadId': 'replacement-thread'}, {'accountKey': 'replacement-account'}):
            with self.subTest(change=change):
                changed = []
                with self.runtime.lock, self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.agent)
                def actions(record, *args):
                    values = original(record, *args)
                    if record.get('type') == 'token_usage_record' and not changed:
                        with self.runtime.lock, self.runtime.db() as db:
                            agent = self.runtime.agent(self.agent['id'], db)
                            agent.update(change)
                            self.runtime.put(db, 'agents', agent)
                        changed.append(True)
                    return values
                with patch.object(history, 'rollout_actions', side_effect=actions):
                    self.assertFalse(self.runtime.analytics_history_step())
                self.assertEqual(changed, [True])
                with self.runtime.read_db() as db:
                    self.assertEqual(db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 0)
                with self.runtime.analytics_db() as db:
                    self.assertEqual(db.execute('SELECT count(*) FROM analytics_usage').fetchone()[0], 0)
                    self.assertEqual(db.execute('SELECT count(*) FROM analytics_history').fetchone()[0], 0)


if __name__ == '__main__':
    unittest.main()

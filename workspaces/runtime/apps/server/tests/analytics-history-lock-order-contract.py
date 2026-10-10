#!/usr/bin/env python3
"""History budgets precede analytics writes and remain exact after retries."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

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


class Runtime(fixture.Runtime):
    def schedule(self):
        pass


class HistoryLockOrder(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-history-lock-order-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.runtime = Runtime(self.root / 'state', fixture.FakeServer)
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
        entered, release = threading.Event(), threading.Event()
        errors, captures = [], []
        original = history.budget_capture
        def capture(*args, **kwargs):
            captures.append(kwargs.get('source'))
            entered.set()
            if not release.wait(2):
                raise TimeoutError('Fixture budget capture was not released')
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
                    codex_analytics, 'budget_capture', side_effect=capture) as analytics_capture:
                worker.start()
                self.assertTrue(entered.wait(1))
                analytics = sqlite3.connect(self.runtime.analytics_db_path, timeout=.05)
                try:
                    analytics.execute('BEGIN IMMEDIATE')
                finally:
                    analytics.rollback()
                    analytics.close()
                main.rollback()
                release.set()
                worker.join(2)
                self.assertFalse(worker.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(analytics_capture.call_count, 0)
        finally:
            main.rollback()
            main.close()
            release.set()
            if worker.ident is not None:
                worker.join(3)
        budget, rows, usage, checkpoint = self.usage()
        self.assertEqual(captures, ['rollout', 'rollout'])
        self.assertEqual(budget['spent'], 100)
        self.assertEqual(rows, [('response', 0), ('response', 100)])
        self.assertEqual({row['responseId'] for row in usage}, {'zero-response', 'exact-response'})
        self.assertEqual(checkpoint['status'], 'current')
        self.assertEqual(checkpoint['coverage'], 'availableRecords')

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
        self.assertTrue(self.runtime.analytics_history_step())
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

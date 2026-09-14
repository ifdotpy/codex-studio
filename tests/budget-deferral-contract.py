#!/usr/bin/env python3
"""Exact unsubmitted budget waits. No server or model calls."""
import contextlib
import copy
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_budget import budget_admission, defer_budget_start, claim_budget_wait
from codex_native_action_receipts import find, reserve, outcome


class Runtime:
    def __init__(self, path):
        self.path = path
        self.lock = threading.RLock()
        self.changed = threading.Event()
        self.closed = False
        with self.db() as db:
            db.execute('CREATE TABLE runtime_agents (id TEXT PRIMARY KEY,record TEXT)')
            db.execute('CREATE TABLE runtime_events (id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT)')

    @contextlib.contextmanager
    def db(self):
        db = sqlite3.connect(self.path)
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def agent(self, key, db):
        return json.loads(db.execute('SELECT record FROM runtime_agents WHERE id=?', (key,)).fetchone()[0])

    def records(self, db, table):
        return [json.loads(r[0]) for r in db.execute('SELECT record FROM runtime_' + table)]

    def put(self, db, table, a):
        db.execute('INSERT OR REPLACE INTO runtime_' + table + ' VALUES (?,?)', (a['id'], json.dumps(a)))


class DeferralTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.r = Runtime(Path(self.temp.name) / 'db')
        self.a = {'id': 'a', 'rootId': 'a', 'threadId': 'thread', 'accountKey': 'other-account', 'epoch': 0,
                  'created': 0, 'tokensUsed': 10, 'tokenBudget': 1, 'status': 'starting', 'inFlight': True, 'autoWake': True,
                  'turnId': None, 'startAttempt': {'id': 'attempt', 'epoch': 0, 'accountKey': 'other-account',
                    'submitted': False, 'events': ['event']}}
        with self.r.db() as db:
            self.r.put(db, 'agents', self.a)
            db.execute("INSERT INTO runtime_events VALUES ('event','a','user','Keep this exact input','dispatching',1,0,NULL,NULL)")

    def tearDown(self):
        self.temp.cleanup()

    def denial(self):
        with self.r.db() as db:
            a = self.r.agent('a', db)
            try:
                budget_admission(self.r, db, a)
            except ValueError as error:
                return error
        self.fail('Expected a budget denial')

    def agent(self):
        with self.r.db() as db:
            return self.r.agent('a', db)

    def update(self, change):
        with self.r.db() as db:
            a = self.r.agent('a', db)
            change(a)
            self.r.put(db, 'agents', a)

    def allow(self):
        self.update(lambda a: a.update(tokenBudget=None))

    def claim(self):
        with self.r.lock, self.r.db() as db:
            a = self.r.agent('a', db)
            budget_admission(self.r, db, a)
            return claim_budget_wait(self.r, db, a)

    def action(self):
        with self.r.db() as db:
            a = self.r.agent('a', db)
            a['startAttempt'] = {'id': 'attempt', 'epoch': 0, 'threadId': 'thread', 'action': 'review', 'events': [],
                                 'submitted': False, 'actionRequestId': 'request',
                                 'actionIdentity': {'accountKey': 'other-account', 'threadId': 'thread', 'epoch': 0}}
            find(db, 'request', 'a', 'review', {})
            reserve(db, 'request', a, 'review', {}, 'attempt')
            self.r.put(db, 'agents', a)

    def test_deferral_preserves_the_attempt_input_and_reservation(self):
        error = self.denial()
        self.assertTrue(defer_budget_start(self.r, 'a', 'attempt', error))
        a = self.agent()
        self.assertEqual(a['status'], 'queued')
        self.assertFalse(a['inFlight'])
        self.assertEqual(a['startAttempt']['id'], 'attempt')
        with self.r.db() as db:
            row = db.execute('SELECT * FROM runtime_events').fetchone()
            self.assertEqual(row['status'], 'pending')
            self.assertEqual(row['text'], 'Keep this exact input')
        at = a['budgetStartWait']['at']
        self.assertTrue(defer_budget_start(self.r, 'a', 'attempt', error))
        self.assertEqual(self.agent()['budgetStartWait']['at'], at)
        self.allow()
        job = self.claim()
        self.assertEqual(job['kind'], 'turn')
        self.assertEqual(job['attempt']['id'], 'attempt')
        self.assertEqual([r['id'] for r in job['rows']], ['event'])
        self.assertNotIn('budgetStartWait', self.agent())
        self.assertIsNone(self.claim())

    def test_action_deferral_keeps_exact_receipt_and_nondefault_account(self):
        self.action()
        self.assertTrue(defer_budget_start(self.r, 'a', 'attempt', self.denial()))
        a = self.agent()
        self.assertEqual(a['budgetActionWait']['actionRequestId'], 'request')
        self.allow()
        job = self.claim()
        self.assertEqual(job['kind'], 'action')
        self.assertEqual(job['attempt']['id'], 'attempt')
        self.assertEqual(job['attempt']['actionRequestId'], 'request')
        with self.r.db() as db:
            self.assertEqual(outcome(db, 'request')['status'], 'pending')

    def test_last_completed_turn_does_not_block_a_new_unsubmitted_wait(self):
        self.update(lambda a: a.update(turnId='previous', lastCompletedTurn='previous', lastCompletedTurnStatus='completed'))
        self.assertTrue(defer_budget_start(self.r, 'a', 'attempt', self.denial()))

    def test_unknown_submitted_and_observed_requests_cannot_wait(self):
        error = self.denial()
        self.assertFalse(defer_budget_start(self.r, 'a', 'attempt', error, unknown=True))
        for field, value in (('submitted', True), ('turnId', 'native'), ('observedTurnId', 'native')):
            original = self.agent()
            self.update(lambda a: a['startAttempt'].update({field: value}))
            self.assertFalse(defer_budget_start(self.r, 'a', 'attempt', error))
            with self.r.db() as db:
                self.r.put(db, 'agents', original)
        self.assertNotIn('budgetStartWait', self.agent())

    def test_changed_identity_or_explicit_stop_cannot_wait(self):
        error = self.denial()
        for field, value in (('epoch', 1), ('accountKey', 'different'), ('threadId', 'different'), ('autoWake', False), ('deletedAt', 1)):
            original = self.agent()
            self.update(lambda a: a.update({field: value}))
            self.assertFalse(defer_budget_start(self.r, 'a', 'attempt', error))
            with self.r.db() as db:
                self.r.put(db, 'agents', original)
        self.assertFalse(defer_budget_start(self.r, 'a', 'different', error))
        self.assertFalse(defer_budget_start(self.r, 'a', 'attempt', ValueError(str(error))))

    def test_delivered_event_does_not_revert_to_pending(self):
        error = self.denial()
        with self.r.db() as db:
            db.execute("UPDATE runtime_events SET status='delivered',turn_id='native'")
        self.assertFalse(defer_budget_start(self.r, 'a', 'attempt', error))
        with self.r.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events').fetchone()[0], 'delivered')

    def test_newer_attempt_retires_wait_without_overwrite(self):
        self.assertTrue(defer_budget_start(self.r, 'a', 'attempt', self.denial()))
        self.update(lambda a: a['startAttempt'].update(id='newer'))
        self.allow()
        self.assertIsNone(self.claim())
        self.assertEqual(self.agent()['startAttempt']['id'], 'newer')
        self.assertEqual(self.agent()['lastBudgetWait']['status'], 'superseded')

    def test_submitted_action_receipt_is_not_failed_by_stale_wait(self):
        self.action()
        self.assertTrue(defer_budget_start(self.r, 'a', 'attempt', self.denial()))
        self.update(lambda a: a['startAttempt'].update(submitted=True))
        self.allow()
        self.assertIsNone(self.claim())
        with self.r.db() as db:
            self.assertEqual(outcome(db, 'request')['status'], 'pending')
        self.assertTrue(self.agent()['startAttempt']['submitted'])

    def test_restart_keeps_durable_wait_and_same_action(self):
        self.action()
        self.assertTrue(defer_budget_start(self.r, 'a', 'attempt', self.denial()))
        self.r = object.__new__(Runtime)
        self.r.path = Path(self.temp.name) / 'db'
        self.r.lock, self.r.changed, self.r.closed = threading.RLock(), threading.Event(), False
        self.allow()
        self.assertEqual(self.claim()['attempt']['actionRequestId'], 'request')


if __name__ == '__main__':
    unittest.main()

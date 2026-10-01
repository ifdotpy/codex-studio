#!/usr/bin/env python3
"""Recover a transferred thread that never received a native turn."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import threading
import time
import unittest

spec = importlib.util.spec_from_file_location(
    'account_transfer_fixture', Path(__file__).with_name('account-transfer-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_account_transfer import AccountTransfers


class EmptyTransferredThreadRecovery(unittest.TestCase):
    def setUp(self):
        self.t = fixture.TransferContract('test_transfer_keeps_identity_history_queue_and_settings')
        self.t.setUp()
        self.t.drive_lazy_for_tests = False
        self.rt = self.t.runtime
        self.aid = self.t.lead_agent['id']
        self.t.set_agent(self.aid, threadId=None, error=None)
        self.op = self.t.start_transfer()
        errors = []
        worker = threading.Thread(target=self._start_transferred_thread, args=(errors,), daemon=True)
        worker.start()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.assertEqual(self.t.pending[0][0], 'thread/start')
        self.t.complete_fork()
        worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])
        self.old_thread = self.rt.agent(self.aid)['threadId']
        self.assertEqual(self.t.receipt(self.op['id'])['status'], 'completed')

    def tearDown(self):
        self.t.tearDown()

    def _start_transferred_thread(self, errors):
        try:
            self.t.store.before_start(self.rt.agent(self.aid))
        except Exception as error:
            errors.append(str(error))

    def _fail_native_history_reads(self, calls):
        def call(method, params, timeout=60):
            calls.append((method, params))
            if method in {'thread/read', 'thread/turns/list'}:
                raise RuntimeError(f'no rollout found for thread id {self.old_thread}')
            if method == 'thread/start':
                return {'thread': {'id': 'replacement-thread'}}
            raise AssertionError('Unexpected native method: ' + method)
        self.t.target_server.call = call

    def _event(self, key, status, kind='user'):
        with self.rt.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                       (key, self.aid, kind, 'saved input', status, time.time(),
                        self.rt.agent(self.aid)['epoch'], None, None))

    def test_restart_with_loaded_cleared_starts_once_and_preserves_events(self):
        self.t.set_agent(self.aid, status='failed', lastCompletedTurn='older-source-turn',
                         error=f'no rollout found for thread id {self.old_thread}')
        with self.rt.db() as db:
            db.execute('INSERT INTO runtime_completed_turns VALUES (?)',
                       (self.aid + ':older-source-turn',))
        self._event('failed-input', 'failed')
        self._event('failed-agent-message', 'failed', kind='agent_message')
        self._event('pending-input', 'pending')
        self.rt.loaded.discard(self.aid)
        calls = []
        self._fail_native_history_reads(calls)

        restarted_store = AccountTransfers(self.rt)
        result = restarted_store.recover_empty_transferred_thread(self.aid)
        repeated = restarted_store.recover_empty_transferred_thread(self.aid)

        self.assertEqual(result['status'], 'recovered')
        self.assertEqual(result['threadId'], 'replacement-thread')
        self.assertEqual(result['failedInputIds'], ['failed-input'])
        self.assertEqual(result['resendableFailedInputIds'], ['failed-input'])
        self.assertFalse(result['replayed'])
        self.assertEqual(repeated, result)
        self.assertEqual([method for method, _ in calls].count('thread/start'), 1)
        self.assertEqual(self.rt.agent(self.aid)['status'], 'failed')
        self.assertIn(self.aid, self.rt.loaded)
        with self.rt.db() as db:
            statuses = {row[0]: row[1] for row in db.execute(
                "SELECT id,status FROM runtime_events WHERE id IN ('failed-input','pending-input')")}
            agent = self.rt.agent(self.aid, db)
            transfer = self.t.store.get(db, self.op['id'])
        self.assertEqual(statuses, {'failed-input': 'failed', 'pending-input': 'pending'})
        self.assertEqual(agent['accountHistory'][-1]['targetThreadId'], 'replacement-thread')
        self.assertEqual(transfer['members'][self.aid]['emptyThreadRecovery']['replacementThreadId'],
                         'replacement-thread')
        start_params = next(params for method, params in calls if method == 'thread/start')
        self.assertEqual(start_params, agent['emptyTransferRecovery']['nativeParams'])

    def test_completed_turn_is_never_replaced(self):
        self.t.set_agent(self.aid, status='failed', error=f'no rollout found for thread id {self.old_thread}')
        with self.rt.lock, self.rt.db() as db:
            record = {'id':'completed-item', 'threadId':self.old_thread,
                      'turnId':'completed-turn', 'turnStatus':'completed'}
            db.execute('INSERT INTO runtime_items(id,agent,record,created) VALUES (?,?,?,?)',
                       ('completed-item', self.aid, __import__('json').dumps(record), time.time()))
        calls = []
        self._fail_native_history_reads(calls)
        with self.assertRaisesRegex(ValueError, 'completed turn'):
            self.t.store.recover_empty_transferred_thread(self.aid)
        self.assertEqual(calls, [])
        self.assertEqual(self.rt.agent(self.aid)['threadId'], self.old_thread)
        self.assertFalse(self.rt.agent(self.aid).get('emptyTransferRecovery'))

    def test_both_native_missing_rollout_errors_are_thread_scoped(self):
        from codex_account_transfer import AccountTransfers
        self.assertTrue(AccountTransfers.missing_rollout_error(
            f'no rollout found for thread id {self.old_thread}', self.old_thread))
        self.assertTrue(AccountTransfers.missing_rollout_error(
            f'invalid paginated history lineage for {self.old_thread}: missing source rollout',
            self.old_thread))
        self.assertFalse(AccountTransfers.missing_rollout_error(
            'no rollout found for thread id another-thread', self.old_thread))

    def test_missing_source_rollout_rebinds_to_target_without_replaying_input(self):
        from codex_agent_management import _missing_transferred_history
        aid = self.aid
        source_home = Path(self.rt.accounts.home('default'))
        source_home.mkdir(parents=True, exist_ok=True)
        rollout = source_home / 'sessions' / 'missing.jsonl'
        state_db = sqlite3.connect(source_home / 'state_5.sqlite')
        state_db.execute('CREATE TABLE threads(id TEXT, rollout_path TEXT, history_mode TEXT)')
        state_db.execute('INSERT INTO threads VALUES (?,?,?)', (self.old_thread, str(rollout), 'paginated'))
        state_db.commit()
        state_db.close()
        history_db = sqlite3.connect(source_home / 'thread_history_1.sqlite')
        history_db.execute('CREATE TABLE thread_turns(thread_id TEXT, turn_id TEXT)')
        history_db.execute('CREATE TABLE thread_items(thread_id TEXT, item_id TEXT)')
        history_db.execute('CREATE TABLE thread_realtime_items(thread_id TEXT, item_id TEXT)')
        history_db.commit()
        history_db.close()
        self.assertEqual(AccountTransfers.source_history_missing(source_home, self.old_thread)['nativeTurns'], 0)

        error = f"[Errno 2] No such file or directory: '{rollout}'"
        transfer_id = 'missing-source-transfer'
        transfer = {'id':transfer_id, 'leadId':aid, 'status':'completed',
                    'targetAccountKey':self.t.other_key, 'members':{aid:{
                        'phase':'completed', 'sourceAccountKey':'default',
                        'sourceThreadId':self.old_thread, 'targetSettings':{}, 'error':error}}}
        agent = self.rt.agent(aid)
        agent.update(accountKey='default', threadId=self.old_thread, status='failed', error=error)
        for key in ('lazyAccountTransfer', 'accountTransferId'):
            agent.pop(key, None)
        with self.rt.lock, self.rt.db() as db:
            self.rt.put(db, 'agents', agent)
            db.execute('INSERT INTO runtime_account_transfers VALUES (?,?)',
                       (transfer_id, json.dumps(transfer)))
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                       ('failed-source-input', aid, 'user', 'saved input', 'failed', time.time(),
                        agent['epoch'], None, None))
        with self.rt.db() as db:
            detected = _missing_transferred_history(db, self.rt.agent(aid, db), self.rt)
        self.assertEqual(detected['status'], 'history_missing')
        self.assertFalse(detected['replayed'])

        def source_call(method, params, timeout=60):
            if method in {'thread/read', 'thread/turns/list'}:
                raise RuntimeError(f'no rollout found for thread id {self.old_thread}')
            raise AssertionError('Unexpected source native method: ' + method)
        self.rt.connect('default').call = source_call
        target_calls = []
        def target_call(method, params, timeout=60):
            target_calls.append((method, params))
            if method == 'thread/start':
                return {'thread':{'id':'fresh-target-thread'}}
            raise AssertionError('Unexpected target native method: ' + method)
        self.rt.connect(self.t.other_key).call = target_call

        result = AccountTransfers(self.rt).recover_empty_transferred_thread(aid)
        self.assertEqual(result['status'], 'recovered')
        self.assertFalse(result['replayed'])
        self.assertEqual([method for method, _ in target_calls], ['thread/start'])
        recovered = self.rt.agent(aid)
        self.assertEqual(recovered['accountKey'], self.t.other_key)
        self.assertEqual(recovered['threadId'], 'fresh-target-thread')
        self.assertIn('Source native history is missing', recovered['error'])
        self.assertNotIn(str(rollout), recovered['error'])
        self.assertTrue(recovered['accountHistory'][-1]['sourceHistoryMissing']['rolloutMissing'])
        with self.rt.db() as db:
            transfer = self.t.store.get(db, transfer_id)
            status = db.execute("SELECT status FROM runtime_events WHERE id='failed-source-input'").fetchone()[0]
        member = transfer['members'][aid]
        self.assertTrue(member['sourceHistoryMissing']['rolloutMissing'])
        self.assertNotIn('error', member)
        self.assertEqual(status, 'failed')

    def test_source_history_proof_rejects_native_turn_rows(self):
        source_home = Path(self.rt.accounts.home('default'))
        source_home.mkdir(parents=True, exist_ok=True)
        rollout = source_home / 'sessions' / 'missing.jsonl'
        state_db = sqlite3.connect(source_home / 'state_5.sqlite')
        state_db.execute('CREATE TABLE threads(id TEXT, rollout_path TEXT, history_mode TEXT)')
        state_db.execute('INSERT INTO threads VALUES (?,?,?)', (self.old_thread, str(rollout), 'paginated'))
        state_db.commit()
        state_db.close()
        history_db = sqlite3.connect(source_home / 'thread_history_1.sqlite')
        history_db.execute('CREATE TABLE thread_turns(thread_id TEXT, turn_id TEXT)')
        history_db.execute('CREATE TABLE thread_items(thread_id TEXT, item_id TEXT)')
        history_db.execute('INSERT INTO thread_turns VALUES (?,?)', (self.old_thread, 'turn'))
        history_db.commit()
        history_db.close()
        self.assertIsNone(AccountTransfers.source_history_missing(source_home, self.old_thread))


if __name__ == '__main__':
    unittest.main()

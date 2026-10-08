#!/usr/bin/env python3
"""Recover a transferred thread that never received a native turn."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
from pathlib import Path
import sqlite3
import threading
import time
import unittest
import uuid
from unittest.mock import patch

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
        if self._testMethodName in {
                'test_reverse_provider_transfer_of_proven_empty_start_uses_one_new_start',
                'test_reverse_provider_transfer_when_native_reads_the_empty_thread_metadata',
                'test_imported_empty_start_proof_requires_its_exact_valid_archive',
                'test_saved_result_rechecks_ordinary_portable_archive_source',
                'test_retry_reconciles_stale_empty_proof_before_archive_publication',
                'test_ready_receipt_blocks_when_checked_empty_source_changes'}:
            claude_home = self.t.root / 'claude-home'
            claude_home.mkdir()
            with self.rt.accounts.lock:
                self.rt.accounts.data['accounts']['claude-fixture'] = {
                    'id':'claude-fixture', 'provider':'claude', 'home':str(claude_home),
                    'label':'Claude fixture', 'status':'ready'}
            self.t.set_agent(self.aid, provider='claude', accountKey='claude-fixture')
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

    def _write_empty_source_native_state(self):
        source_home = Path(self.rt.accounts.home(self.t.other_key))
        source_home.mkdir(parents=True, exist_ok=True)
        state_db = sqlite3.connect(source_home / 'state_5.sqlite')
        state_db.execute('CREATE TABLE threads(id TEXT, rollout_path TEXT, history_mode TEXT)')
        state_db.execute('INSERT INTO threads VALUES (?,?,?)',
                         (self.old_thread, str(source_home / 'sessions' / 'not-materialized.jsonl'), 'paginated'))
        state_db.commit()
        state_db.close()
        history_db = sqlite3.connect(source_home / 'thread_history_1.sqlite')
        for table, column in (('thread_turns', 'turn_id'), ('thread_items', 'item_id'),
                              ('thread_realtime_items', 'item_id')):
            history_db.execute(f'CREATE TABLE {table}(thread_id TEXT, {column} TEXT)')
        history_db.commit()
        history_db.close()

    def _install_imported_archive_receipt(self):
        """Model a completed Claude-to-Codex start that imported an archive."""
        from codex_portable_history import export_history

        source_thread = 'prior-claude-thread'
        prior_transfer_id = str(uuid.uuid4())
        source = {**self.rt.agent(self.aid), 'accountKey':'claude-fixture',
                  'threadId':source_thread}

        class SourceHistory:
            def call(_self, method, params, timeout=60):
                if method == 'thread/read':
                    return {'thread':{'id':source_thread, 'status':{'type':'idle'},
                                      'updatedAt':1, 'historyVersion':1}}
                if method == 'thread/turns/list':
                    return {'data':[]}
                raise AssertionError('Unexpected archive method: ' + method)

        archive = export_history(self.rt, source, prior_transfer_id, SourceHistory())
        self.assertEqual(archive['source']['threadId'], source_thread)
        self.assertEqual(archive['counts']['native_thread'], 1)
        with self.rt.lock, self.rt.db() as db:
            transfer = copy.deepcopy(self.t.store.get(db, self.op['id']))
            transfer['id'] = prior_transfer_id
            member = transfer['members'][self.aid]
            member.update(sourceAccountKey='claude-fixture', sourceThreadId=source_thread,
                          nativeMethod='thread/start', portableHistory=archive,
                          phase='completed', result={'thread':{'id':self.old_thread}})
            transfer.update(status='completed', targetAccountKey=self.t.other_key)
            self.rt.put(db, 'account_transfers', transfer)
            agent = self.rt.agent(self.aid, db)
            agent.update(accountKey=self.t.other_key, threadId=self.old_thread,
                         portableHistory=archive)
            history = agent['accountHistory'][-1]
            history.update(accountKey='claude-fixture', threadId=source_thread,
                            targetAccountKey=self.t.other_key, targetThreadId=self.old_thread,
                            transferId=prior_transfer_id, portableHistory=archive)
            self.rt.put(db, 'agents', agent)
        return archive

    def test_reverse_provider_transfer_of_proven_empty_start_uses_one_new_start(self):
        # First hop imported an archive onto a new native thread. The reverse
        # transfer must preserve that ancestry when the rollout is still absent.
        def unreadable(method):
            if method == 'thread/read':
                raise RuntimeError(f'invalid paginated history lineage for {self.old_thread}: missing source rollout')
            return {'data': []}
        self._reverse_transfer_of_empty_start(unreadable)

    def test_reverse_provider_transfer_when_native_reads_the_empty_thread_metadata(self):
        # Observed with native 0.161.0 on an unloaded thread that has a state
        # row and no rollout: the metadata read succeeds, the paginated history
        # read fails, and the background command list reports no such thread.
        def metadata_only(method):
            if method == 'thread/read':
                return {'thread':{'id':self.old_thread, 'status':{'type':'notLoaded'},
                                  'historyMode':'paginated', 'updatedAt':1}}
            if method == 'thread/backgroundTerminals/list':
                raise RuntimeError(json.dumps({'code':-32600, 'message':'thread not found: ' + self.old_thread}))
            return {'data': []}
        calls = self._reverse_transfer_of_empty_start(
            metadata_only, stale_member={'archiveSourceThread':{'id':self.old_thread, 'updatedAt':1}})
        self.assertNotIn('thread/turns/list', [method for method, _ in calls])
        self.assertGreaterEqual([method for method, _ in calls].count('thread/backgroundTerminals/list'), 1)

    def test_empty_source_background_jobs_accepts_only_the_unloaded_thread_answer(self):
        class Source:
            def __init__(self, error): self.error = error
            def call(self, method, params, timeout=60): raise RuntimeError(self.error)
        jobs = AccountTransfers.empty_source_background_jobs(
            Source('thread not found: ' + self.old_thread), self.old_thread)
        self.assertEqual(jobs, {'data':[], 'nextCursor':None})
        for error in ('thread not found: another-thread', 'Codex app-server is offline'):
            with self.assertRaisesRegex(RuntimeError, error):
                AccountTransfers.empty_source_background_jobs(Source(error), self.old_thread)

    def _reverse_transfer_of_empty_start(self, native_answer, stale_member=None):
        prior_archive = self._install_imported_archive_receipt()
        self._write_empty_source_native_state()
        self.t.pending.clear()
        self.t.drive_lazy_for_tests = False
        source = self.rt.connect(self.t.other_key)
        calls = []
        def source_call(method, params, timeout=60):
            calls.append((method, params))
            if method in {'thread/read', 'thread/queue/list', 'thread/backgroundTerminals/list'}:
                return native_answer(method)
            raise AssertionError('Unexpected native method: ' + method)
        source.call = source_call
        target = self.rt.connect('claude-fixture')
        target.pending = []
        target.callbacks = self.t.target_server.callbacks
        target.clock_replies = self.t.target_server.clock_replies
        target_calls = []
        def submit(method, params):
            from concurrent.futures import Future
            target_calls.append((method, params))
            future = Future()
            return future
        target.submit = submit
        target.wait = lambda _future, timeout=60: {'thread':{'id':'reverse-target-thread'}}
        target.after_events = lambda cb: cb()
        original_catalog = self.rt.catalog
        original_settings = self.t.store.destination_settings
        original_params = self.rt.new_thread_params
        self.rt.catalog = lambda _account: original_catalog(self.t.other_key)
        self.t.store.destination_settings = lambda *_args: {}
        self.rt.new_thread_params = lambda *_args, **_kwargs: {'cwd':str(self.t.root), 'model':'default'}
        try:
            op = self.t.store.request(self.aid, 'claude-fixture', str(uuid.uuid4()))
            self.t.current_transfer_id = op['id']
            self.assertEqual(self.t.receipt(op['id'])['members'][self.aid]['phase'], 'lazy')
            with self.rt.lock, self.rt.db() as db:
                blocked = self.t.store.get(db, op['id'])
                member = blocked['members'][self.aid]
                member.update(phase='blocked', error='invalid paginated history lineage: missing source rollout',
                              **(stale_member or {}))
                member.pop('nativeMethod', None)
                self.t.store.save(db, blocked)
            self.t.store.action(op['id'], 'retry')
            self.assertEqual(self.t.receipt(op['id'])['members'][self.aid]['phase'], 'lazy')
            self.assertNotIn('nativeMethod', self.t.receipt(op['id'])['members'][self.aid])
            self.t.store.move_lazy(op['id'], self.aid)
            method, params = target_calls[0]
            self.assertEqual(method, 'thread/start')
            self.assertNotIn('threadId', params)
            self.assertEqual(self.t.receipt(op['id'])['members'][self.aid]['sourceEmptyProof']['threadId'],
                             self.old_thread)
            self.assertEqual(self.t.runtime.agent(self.aid)['accountKey'], 'claude-fixture')
            self.assertEqual([m for m, _ in calls].count('thread/read'), 1)
            self.assertEqual([m for m, _ in target_calls].count('thread/start'), 1)
            descriptor = self.t.runtime.agent(self.aid)['portableHistory']
            self.assertEqual(descriptor['source']['threadId'], self.old_thread)
            records = [json.loads(line) for line in Path(descriptor['path']).read_text().splitlines()]
            self.assertIn(prior_archive,
                          [row['archive'] for row in records if row['kind'] == 'ancestor'])
            marker = next(row for row in records if row['kind'] == 'native_unmaterialized')
            self.assertEqual(marker['threadId'], self.old_thread)
            self.assertEqual(marker['emptyProof']['nativeHistory']['nativeTurns'], 0)
            self.assertFalse(any(method == 'turn/start' for method, _ in calls + target_calls))
            return calls
        finally:
            self.rt.catalog = original_catalog
            self.t.store.destination_settings = original_settings
            self.rt.new_thread_params = original_params

    def test_imported_empty_start_proof_requires_its_exact_valid_archive(self):
        archive = self._install_imported_archive_receipt()
        self._write_empty_source_native_state()
        expected = f'invalid paginated history lineage for {self.old_thread}: missing source rollout'
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.aid, db)
            proof = self.t.store.native_empty_transfer_proof(
                db, agent, self.old_thread, expected, source_account_key=self.t.other_key)
        self.assertIsNotNone(proof)

        with self.rt.lock, self.rt.db() as db:
            transfer = self.t.store.get(db, archive['transferId'])
            transfer['members'][self.aid].pop('portableHistory')
            self.t.store.save(db, transfer)
            self.assertIsNone(self.t.store.native_empty_transfer_proof(
                db, self.rt.agent(self.aid, db), self.old_thread, expected,
                source_account_key=self.t.other_key))
            wrong_archive = {**archive, 'sha256':'wrong'}
            transfer['members'][self.aid]['portableHistory'] = wrong_archive
            self.t.store.save(db, transfer)
            agent = self.rt.agent(self.aid, db)
            agent['accountHistory'][-1]['portableHistory'] = wrong_archive
            self.rt.put(db, 'agents', agent)
            self.assertIsNone(self.t.store.native_empty_transfer_proof(
                db, self.rt.agent(self.aid, db), self.old_thread, expected,
                source_account_key=self.t.other_key))
            transfer['members'][self.aid]['portableHistory'] = archive
            self.t.store.save(db, transfer)
            agent = self.rt.agent(self.aid, db)
            agent['accountHistory'][-1]['portableHistory'] = archive
            self.rt.put(db, 'agents', agent)

        rollout = Path(self.rt.accounts.home(self.t.other_key)) / 'sessions' / 'not-materialized.jsonl'
        rollout.parent.mkdir(parents=True, exist_ok=True)
        rollout.write_text('{"type":"session_meta"}\n')
        with self.rt.db() as db:
            self.assertIsNone(self.t.store.native_empty_transfer_proof(
                db, self.rt.agent(self.aid, db), self.old_thread, expected,
                source_account_key=self.t.other_key))

    def test_saved_result_rechecks_ordinary_portable_archive_source(self):
        key = str(uuid.uuid4())
        with self.rt.lock, self.rt.db() as db:
            transfer = copy.deepcopy(self.t.store.get(db, self.op['id']))
            transfer.update(id=key, status='pending', targetAccountKey='claude-fixture')
            member = transfer['members'][self.aid]
            member.update(phase='ready', result={'thread':{'id':'saved-target-thread'}},
                          archiveSourceThread={'id':self.old_thread, 'updatedAt':1,
                                               'historyVersion':1},
                          portableHistory={'source':{'threadId':self.old_thread}},
                          targetSettings={}, settings={})
            self.rt.put(db, 'account_transfers', transfer)
            agent = self.rt.agent(self.aid, db)
            agent.update(accountKey='claude-fixture', accountTransferId=key,
                         lazyAccountTransfer={'id':key, 'sourceAccountKey':self.t.other_key,
                                              'sourceThreadId':self.old_thread})
            self.rt.put(db, 'agents', agent)
        with patch.object(self.t.store, 'assert_settings'), \
                patch.object(self.t.store, 'archive_source_current', return_value=False) as check_archive:
            with self.assertRaisesRegex(RuntimeError, 'source changed after history export'):
                self.t.store.move_lazy(key, self.aid)
        check_archive.assert_called_once_with(key, self.aid)
        self.assertEqual(self.t.receipt(key)['members'][self.aid]['result']['thread']['id'],
                         'saved-target-thread')

    def test_empty_transfer_proof_rejects_nonempty_or_ambiguous_native_state(self):
        self._write_empty_source_native_state()
        expected = f'invalid paginated history lineage for {self.old_thread}: missing source rollout'
        with self.rt.db() as db:
            agent = self.rt.agent(self.aid, db)
            proof = self.t.store.native_empty_transfer_proof(db, agent, self.old_thread, expected)
        self.assertIsNotNone(proof)
        source_home = Path(self.rt.accounts.home(self.t.other_key))
        history_db = sqlite3.connect(source_home / 'thread_history_1.sqlite')
        history_db.execute('INSERT INTO thread_items VALUES (?,?)', (self.old_thread, 'item'))
        history_db.commit()
        history_db.close()
        with self.rt.db() as db:
            self.assertIsNone(self.t.store.native_empty_transfer_proof(
                db, self.rt.agent(self.aid, db), self.old_thread, expected))
        history_db = sqlite3.connect(source_home / 'thread_history_1.sqlite')
        history_db.execute('DELETE FROM thread_items')
        history_db.commit()
        history_db.close()
        with self.rt.lock, self.rt.db() as db:
            transfer = self.t.store.get(db, self.op['id'])
            transfer['members'][self.aid]['nativeMethod'] = 'thread/fork'
            self.t.store.save(db, transfer)
            self.assertIsNone(self.t.store.native_empty_transfer_proof(
                db, self.rt.agent(self.aid, db), self.old_thread, expected))

    def test_retry_reconciles_stale_empty_proof_before_archive_publication(self):
        self._write_empty_source_native_state()
        source_home = Path(self.rt.accounts.home(self.t.other_key))
        rollout = source_home / 'sessions' / 'not-materialized.jsonl'
        self.t.pending.clear()
        with self.rt.lock, self.rt.db() as db:
            before = self.rt.agent(self.aid, db)
            stale = self.t.store.native_empty_transfer_proof(
                db, before, self.old_thread,
                f'invalid paginated history lineage for {self.old_thread}: missing source rollout',
                source_account_key=self.t.other_key)
        self.assertIsNotNone(stale)
        rollout.parent.mkdir(parents=True, exist_ok=True)
        rollout.write_text('{"type":"session_meta"}\n')

        source = self.rt.connect(self.t.other_key)
        native = {'id':self.old_thread, 'status':{'type':'notLoaded'}, 'path':str(rollout),
                  'updatedAt':1, 'historyVersion':1}
        calls = []
        def source_call(method, params, timeout=60):
            calls.append((method, params))
            if method == 'thread/read': return {'thread':dict(native)}
            if method in {'thread/queue/list', 'thread/backgroundTerminals/list', 'thread/turns/list'}:
                return {'data': []}
            raise AssertionError('Unexpected native method: ' + method)
        source.call = source_call
        target = self.rt.connect('claude-fixture')
        target_calls = []
        def submit(method, params):
            from concurrent.futures import Future
            target_calls.append((method, params))
            return Future()
        target.submit = submit
        target.wait = lambda _future, timeout=60: {'thread':{'id':'materialized-target-thread'}}
        original_catalog = self.rt.catalog
        original_settings = self.t.store.destination_settings
        original_params = self.rt.new_thread_params
        self.rt.catalog = lambda _account: original_catalog(self.t.other_key)
        self.t.store.destination_settings = lambda *_args: {}
        self.rt.new_thread_params = lambda *_args, **_kwargs: {'cwd':str(self.t.root), 'model':'default'}
        try:
            op = self.t.store.request(self.aid, 'claude-fixture', str(uuid.uuid4()))
            self.t.current_transfer_id = op['id']
            with self.rt.lock, self.rt.db() as db:
                blocked = self.t.store.get(db, op['id'])
                member = blocked['members'][self.aid]
                member.update(phase='blocked', error='Native source read was temporarily unavailable',
                              sourceEmptyProof=stale)
                member.pop('nativeMethod', None)
                self.t.store.save(db, blocked)
            self.t.store.action(op['id'], 'retry')
            self.t.store.move_lazy(op['id'], self.aid)

            saved = self.t.receipt(op['id'])['members'][self.aid]
            self.assertEqual(saved['phase'], 'completed')
            self.assertNotIn('sourceEmptyProof', saved)
            descriptor = self.rt.agent(self.aid)['portableHistory']
            self.assertEqual(descriptor['source']['threadId'], self.old_thread)
            self.assertEqual(descriptor['counts']['native_thread'], 1)
            self.assertNotIn('native_unmaterialized', descriptor['counts'])
            self.assertEqual([method for method, _ in target_calls], ['thread/start'])
            self.assertGreaterEqual([method for method, _ in calls].count('thread/turns/list'), 1)
        finally:
            self.rt.catalog = original_catalog
            self.t.store.destination_settings = original_settings
            self.rt.new_thread_params = original_params

    def test_ready_receipt_blocks_when_checked_empty_source_changes(self):
        self._write_empty_source_native_state()
        expected = f'invalid paginated history lineage for {self.old_thread}: missing source rollout'
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.aid, db)
            proof = self.t.store.native_empty_transfer_proof(
                db, agent, self.old_thread, expected, source_account_key=self.t.other_key)
        self.assertIsNotNone(proof)
        op = self.t.store.request(self.aid, 'claude-fixture', str(uuid.uuid4()))
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.aid, db)
            source = {**agent, 'accountKey':self.t.other_key, 'threadId':self.old_thread}
            from codex_portable_history import export_history
            descriptor = export_history(self.rt, source, op['id'], None, native_empty_proof=proof)
            saved = self.t.store.get(db, op['id'])
            member = saved['members'][self.aid]
            member.update(phase='ready', result={'thread':{'id':'saved-destination-thread'}},
                          sourceEmptyProof=proof, portableHistory=descriptor)
            self.t.store.save(db, saved)
        rollout = Path(self.rt.accounts.home(self.t.other_key)) / 'sessions' / 'not-materialized.jsonl'
        rollout.parent.mkdir(parents=True, exist_ok=True)
        rollout.write_text('{"type":"session_meta"}\n')

        self.assertFalse(self.t.store.archive_source_current(op['id'], self.aid))
        member = self.t.receipt(op['id'])['members'][self.aid]
        self.assertEqual(member['phase'], 'blocked')
        self.assertTrue(member['archiveInvalidated'])
        self.assertEqual(member['result']['thread']['id'], 'saved-destination-thread')
        self.assertFalse(self.t.runtime.agent(self.aid)['accountTransfer']['canRetry'])
        self.t.store.action(op['id'], 'cancel')
        cancelled = self.t.receipt(op['id'])
        restored = self.t.runtime.agent(self.aid)
        self.assertEqual(cancelled['status'], 'cancelled')
        self.assertEqual(restored['accountKey'], self.t.other_key)
        self.assertNotIn('accountTransferId', restored)
        self.assertEqual(cancelled['members'][self.aid]['result']['thread']['id'], 'saved-destination-thread')


if __name__ == '__main__':
    unittest.main()

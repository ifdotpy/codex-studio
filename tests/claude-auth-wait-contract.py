#!/usr/bin/env python3
"""A local auth probe timeout can continue exact unsubmitted input, without replay."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
import os
from pathlib import Path
import threading
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('claude_auth_fixture', ROOT / 'tests/claude-provider-contract.py')
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_claude_auth_wait as auth_wait
import codex_runtime
from codex_source import source_function

TIMEOUT = {'status': 'error', 'accountId': None, '_authErrorKind': 'timeout',
           'error': 'Cannot read Claude Code sign-in status'}


class AuthWait(f.ClaudeProvider):
    def setUp(self):
        baseline = os.environ.get('STUDIO_AUTH_WAIT_BASELINE')
        if baseline:
            for name in ('start', 'start_error', 'dispatch_all'):
                function, _ = source_function(Path(baseline).read_bytes(), ('Runtime', name),
                                              vars(codex_runtime), baseline)
                replacement = patch.object(codex_runtime.Runtime, name, function)
                replacement.start()
                self.addCleanup(replacement.stop)
        super().setUp()
        self.agent = self.runtime.new_lead({'cwd': str(self.root), 'account_key': 'claude-local'})
        self.agent = self.runtime.prepare(self.agent)
        self.ids = ['auth-original-a', 'auth-original-b']

    def starts(self):
        return [params for server in self.runtime.servers.values()
                for method, params in server.calls if method == 'turn/start']

    def owned_task(self):
        parent = self.runtime.new_lead({'cwd': str(self.root), 'account_key': 'claude-local'})
        with self.runtime.lock, self.runtime.db() as db:
            parent.update(autoWake=True, status='waiting')
            self.runtime.put(db, 'agents', parent)
            current = self.runtime.agent(self.agent['id'], db)
            current.update(parentId=parent['id'], rootId=parent['id'], isLead=False, autoWake=True)
            self.runtime.put(db, 'agents', current)
        task = self.runtime.work_action(parent['id'], {'action': 'create', 'title': 'The original task'},
                                        actor=parent['id'])
        task = self.runtime.work_action(self.agent['id'], {'action': 'claim', 'task_id': task['id']},
                                        actor=self.agent['id'])
        return parent, task

    def fail_auth(self, metadata=None, assets=None):
        with patch('codex_claude.auth_metadata', return_value=metadata or TIMEOUT):
            for key in self.ids:
                self.runtime.send(self.agent['id'], 'Exact input ' + key, key, manual=False,
                                  assets=assets if key == self.ids[0] else None)
            self.runtime.dispatch()
            f.f.f.eventually(lambda: self.runtime.agent(self.agent['id'])['status'] in {'waiting', 'failed'})
        self.assertEqual(self.runtime.agent(self.agent['id'])['status'],
                         'waiting' if (metadata or TIMEOUT).get('_authErrorKind') == 'timeout' else 'failed')
        self.assertEqual(self.starts(), [])

    def receipt(self):
        with self.runtime.read_db() as db:
            rows = db.execute("SELECT record FROM runtime_usage_resumes WHERE agent=? "
                              "AND json_extract(record,'$.cause')='claude_auth_probe'", (self.agent['id'],)).fetchall()
        self.assertEqual(len(rows), 1)
        return json.loads(rows[0][0])

    def due(self):
        receipt = self.receipt()
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("UPDATE runtime_usage_resumes SET record=json_set(record,'$.dueAt',0) WHERE id=?",
                       (receipt['id'],))

    def recover(self):
        self.due()
        auth_wait.tick(self.runtime)
        f.f.f.eventually(lambda: not self.runtime.__dict__.get('_claude_auth_wait_jobs'))

    def test_fresh_same_account_queues_original_inputs_once(self):
        before = time.time()
        self.fail_auth()
        original = copy.deepcopy(self.runtime.agent(self.agent['id'])['startAttempt'])
        receipt = self.receipt()
        self.assertGreaterEqual(receipt['dueAt'], before + 179)
        self.assertEqual(receipt['attempt'], original)
        self.assertNotIn('usageResume', self.runtime.agent(self.agent['id']))
        self.recover()
        self.assertEqual(self.receipt()['status'], 'completed')
        self.assertEqual(self.runtime.agent(self.agent['id'])['status'], 'queued')
        self.runtime.dispatch()
        f.f.f.eventually(lambda: all(self.runtime.delivery_receipt(key)['status'] == 'delivered' for key in self.ids))
        self.runtime.dispatch()
        self.assertEqual(len(self.starts()), 1)
        self.assertEqual(self.starts()[0]['clientUserMessageId'], self.ids[0])
        for key in self.ids:
            self.assertIn('Exact input ' + key, json.dumps(self.starts()[0]['input']))
        self.assertFalse(any(method in {'turn/interrupt', 'command/exec', 'account/rateLimitResetCredit/consume'}
                             for server in self.runtime.servers.values() for method, _ in server.calls))

    def test_local_wait_keeps_the_owned_task_and_does_not_notify_terminal_failure(self):
        parent, task = self.owned_task()
        self.fail_auth()
        self.assertEqual(self.receipt()['taskClaims'], [task['id']])
        with self.runtime.lock, self.runtime.db() as db:
            self.assertEqual(self.runtime.release_failed_work(db, self.runtime.records(db, 'agents'), force=True), [])
            self.assertEqual(self.runtime.work_view(self.runtime.work_by_id(db, task['id'], parent['id']), {}), task)
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE agent=? "
                                       "AND kind IN ('child_result','work_released')", (parent['id'],)).fetchone()[0], 0)
        self.runtime.dispatch()
        self.assertEqual(self.starts(), [])
        self.recover()
        self.assertEqual(self.runtime.agent(self.agent['id'])['status'], 'queued')
        with self.runtime.read_db() as db:
            self.assertEqual(self.runtime.work_view(self.runtime.work_by_id(db, task['id'], parent['id']), {}), task)

    def test_changed_task_ownership_cancels_without_releasing_or_sending_the_old_input(self):
        parent, task = self.owned_task()
        self.fail_auth()
        changed = self.runtime.work_action(parent['id'], {'action': 'update', 'task_id': task['id'], 'owner': None},
                                           actor=parent['id'])
        before = self.runtime.agent(self.agent['id'])
        self.due()
        with patch('codex_claude.auth_metadata', side_effect=AssertionError('The old task is no longer assigned')):
            auth_wait.tick(self.runtime)
        self.assertEqual(self.receipt()['status'], 'cancelled')
        self.assertEqual(self.runtime.agent(self.agent['id']), before)
        with self.runtime.read_db() as db:
            self.assertEqual(self.runtime.work_view(self.runtime.work_by_id(db, task['id'], parent['id']), {}), changed)
        self.assertEqual([self.runtime.delivery_receipt(key)['status'] for key in self.ids], ['pending', 'pending'])
        self.assertEqual(self.starts(), [])

    def test_probe_is_async_sparse_and_holds_neither_shared_lock(self):
        self.fail_auth()
        self.due()
        entered, release = threading.Event(), threading.Event()
        self.addCleanup(release.set)
        calls = []
        def probe(_profile=None, force=False):
            calls.append(force)
            entered.set()
            if not release.wait(5):
                raise AssertionError('The fixture did not release the probe')
            return f.AUTH.copy()
        with patch('codex_claude.auth_metadata', side_effect=probe):
            began = time.monotonic()
            auth_wait.tick(self.runtime)
            self.assertLess(time.monotonic() - began, .5)
            self.assertTrue(entered.wait(2))
            with self.runtime.lock, self.runtime.db() as db:
                db.execute('CREATE TABLE fixture_unrelated_writer (id INTEGER)')
                db.execute('INSERT INTO fixture_unrelated_writer VALUES (1)')
            self.assertTrue(self.runtime.accounts.lock.acquire(timeout=.5))
            self.runtime.accounts.lock.release()
            for _ in range(20):
                auth_wait.tick(self.runtime)
            self.assertEqual(calls, [True])
            self.assertGreater(self.receipt()['dueAt'], time.time() + 178)
            release.set()
            f.f.f.eventually(lambda: self.receipt()['status'] == 'completed')

    def test_stop_during_probe_preserves_cancelled_input_and_epoch(self):
        self.fail_auth()
        self.due()
        entered, release = threading.Event(), threading.Event()
        self.addCleanup(release.set)
        def probe(_profile=None, force=False):
            entered.set()
            self.assertTrue(release.wait(5))
            return f.AUTH.copy()
        with patch('codex_claude.auth_metadata', side_effect=probe):
            auth_wait.tick(self.runtime)
            self.assertTrue(entered.wait(2))
            self.runtime.stop(self.agent['id'], descendants=False)
            before = self.runtime.agent(self.agent['id'])
            release.set()
            f.f.f.eventually(lambda: not self.runtime.__dict__.get('_claude_auth_wait_jobs'))
        self.assertEqual(self.runtime.agent(self.agent['id']), before)
        self.assertEqual(self.receipt()['status'], 'cancelled')
        self.assertEqual([self.runtime.delivery_receipt(key)['status'] for key in self.ids], ['cancelled', 'cancelled'])
        self.assertEqual(self.starts(), [])

    def test_restart_preserves_deadline_and_original_attempt(self):
        self.fail_auth()
        before = self.receipt()
        old = self.runtime
        old.close()
        self.runtime = f.f.ControlledRuntime(self.root / 'state', f.MonitorServer)
        self.addCleanup(self.runtime.close)
        self.assertEqual(self.receipt(), before)
        with patch('codex_claude.auth_metadata', side_effect=AssertionError('The receipt is not due')):
            auth_wait.tick(self.runtime)
        self.assertEqual(self.receipt(), before)
        self.recover()
        self.runtime.dispatch()
        f.f.f.eventually(lambda: all(self.runtime.delivery_receipt(key)['status'] == 'delivered' for key in self.ids))
        self.assertEqual(len(self.starts()), 1)

    def test_source_input_attempt_and_unknown_guards_retire_without_mutation(self):
        self.fail_auth()
        baseline = self.runtime.agent(self.agent['id'])
        saved = self.receipt()
        with self.runtime.read_db() as db:
            original_events = [dict(row) for row in db.execute('SELECT * FROM runtime_events WHERE agent=?',
                                                              (self.agent['id'],))]
        changes = [
            {'epoch': baseline['epoch'] + 1}, {'threadId': 'another-thread'}, {'accountKey': 'default'},
            {'model': 'another-model'}, {'rootId': 'another-root'}, {'nativeFailureHold': True},
            {'claudeOptions': {'settingSources': ['project']}},
            {'pendingSettings': {'model': 'a-new-model'}}, {'pendingSettingsAccountKey': 'another-account'},
            {'inFlight': True, 'turnId': 'unknown-turn'}, {'contextRepairWait': {'error': 'Keep this wait'}},
            {'startAttempt': {**baseline['startAttempt'], 'id': 'another-attempt'}},
            {'startAttempt': {**baseline['startAttempt'], 'submitted': True}},
        ]
        for changed in changes:
            with self.subTest(changed=changed):
                with self.runtime.lock, self.runtime.db() as db:
                    self.runtime.put(db, 'agents', {**baseline, **changed})
                    db.execute('UPDATE runtime_usage_resumes SET record=? WHERE id=?',
                               (json.dumps({**saved, 'dueAt': 0}), saved['id']))
                before = self.runtime.agent(self.agent['id'])
                with patch('codex_claude.auth_metadata', side_effect=AssertionError('Do not probe changed input')):
                    auth_wait.tick(self.runtime)
                self.assertEqual(self.receipt()['status'], 'cancelled')
                self.assertEqual(self.runtime.agent(self.agent['id']), before)
                with self.runtime.read_db() as db:
                    self.assertEqual([dict(row) for row in db.execute('SELECT * FROM runtime_events WHERE agent=?',
                                                                     (self.agent['id'],))], original_events)
        self.assertEqual(self.starts(), [])

    def test_event_body_metadata_assets_and_new_input_changes_cancel(self):
        self.fail_auth()
        saved = self.receipt()
        with self.runtime.read_db() as db:
            original = dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (self.ids[0],)).fetchone())
            metadata = db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (self.ids[0],)).fetchone()[0]
        for kind in ('body', 'metadata', 'asset', 'new_input'):
            with self.subTest(kind=kind):
                with self.runtime.lock, self.runtime.db() as db:
                    db.execute('UPDATE runtime_events SET text=? WHERE id=?', (original['text'], self.ids[0]))
                    db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?', (metadata, self.ids[0]))
                    db.execute('DELETE FROM runtime_events WHERE id=?', ('newer-input',))
                    db.execute('UPDATE runtime_usage_resumes SET record=? WHERE id=?',
                               (json.dumps({**saved, 'dueAt': 0}), saved['id']))
                    if kind == 'body':
                        db.execute('UPDATE runtime_events SET text=? WHERE id=?', ('Changed input', self.ids[0]))
                    elif kind in {'metadata', 'asset'}:
                        meta = json.loads(metadata)
                        meta.update({'delivery': 'after_turn'} if kind == 'metadata' else {'assets': ['missing-asset']})
                        db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?', (json.dumps(meta), self.ids[0]))
                    else:
                        db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                                   ('newer-input', self.agent['id'], 'user', 'A new input', 'pending',
                                    time.time(), self.agent['epoch'], None, None))
                with patch('codex_claude.auth_metadata', side_effect=AssertionError('Do not probe changed input')):
                    auth_wait.tick(self.runtime)
                self.assertEqual(self.receipt()['status'], 'cancelled')
        self.assertEqual(self.starts(), [])

    def test_changed_profile_during_probe_cannot_queue_input(self):
        self.fail_auth()
        self.due()
        entered, release = threading.Event(), threading.Event()
        self.addCleanup(release.set)
        def probe(_profile=None, force=False):
            entered.set()
            self.assertTrue(release.wait(5))
            return f.AUTH.copy()
        with patch('codex_claude.auth_metadata', side_effect=probe):
            auth_wait.tick(self.runtime)
            self.assertTrue(entered.wait(2))
            with self.runtime.accounts.lock:
                self.runtime.accounts._row('claude-local')['claudeOptions'] = {'configDir': '/other-profile'}
            release.set()
            f.f.f.eventually(lambda: not self.runtime.__dict__.get('_claude_auth_wait_jobs'))
        self.assertEqual(self.receipt()['status'], 'cancelled')
        self.assertEqual(self.runtime.agent(self.agent['id'])['status'], 'waiting')
        self.assertEqual(self.starts(), [])

    def test_generic_and_post_submission_errors_never_create_local_timeout_receipt(self):
        for metadata in ({**TIMEOUT, '_authErrorKind': 'parser'}, {**TIMEOUT, '_authErrorKind': None}):
            with self.subTest(metadata=metadata):
                with self.runtime.lock, self.runtime.db() as db:
                    current = self.runtime.agent(self.agent['id'], db)
                    current.update(status='queued', error=None)
                    current.pop('startAttempt', None)
                    self.runtime.put(db, 'agents', current)
                self.fail_auth(metadata)
                with self.runtime.read_db() as db:
                    self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_usage_resumes WHERE agent=? "
                                                "AND json_extract(record,'$.cause')='claude_auth_probe'",
                                                (self.agent['id'],)).fetchone()[0], 0)
        self.runtime.send(self.agent['id'], 'Explicit retry', 'new-authorized-input')
        self.runtime.dispatch()
        f.f.f.eventually(lambda: self.runtime.agent(self.agent['id'])['startAttempt'].get('submitted'))
        current = self.runtime.agent(self.agent['id'])
        with self.runtime.lock, self.runtime.db() as db:
            forged = auth_wait.AuthProbeTimeout({'source': auth_wait._source(self.runtime, current),
                'attempt': copy.deepcopy(current['startAttempt']), 'inputs': []})
            self.assertFalse(auth_wait.record_wait(self.runtime, db, current, forged, unknown=True))
            self.assertFalse(auth_wait.record_wait(self.runtime, db, current, forged))

    def test_malformed_retired_and_changed_waits_use_normal_startup_cleanup(self):
        self.fail_auth()
        saved = self.receipt()
        actor = self.runtime.agent(self.agent['id'])
        for change in ({'dueAt': None}, {'status': 'cancelled'},
                       {'source': {**saved['source'], 'threadId': 'another-thread'}}):
            with self.subTest(change=change):
                with self.runtime.lock, self.runtime.db() as db:
                    self.runtime.put(db, 'agents', actor)
                    db.execute('UPDATE runtime_usage_resumes SET record=? WHERE id=?',
                               (json.dumps({**saved, **change}), saved['id']))
                self.runtime.close()
                self.runtime = f.f.ControlledRuntime(self.root / 'state', f.MonitorServer)
                self.addCleanup(self.runtime.close)
                self.assertNotIn('startAttempt', self.runtime.agent(self.agent['id']))
                self.assertEqual(self.runtime.agent(self.agent['id'])['status'], 'waiting')

    def test_no_positive_same_identity_proof_keeps_inputs_pending(self):
        self.fail_auth()
        before = self.runtime.agent(self.agent['id'])
        for proof in ({**f.AUTH, 'accountId': 'claude:another-account'},
                      {**f.AUTH, '_credentialIdentity': None}, TIMEOUT,
                      {'status': 'signedOut', 'accountId': None}):
            with self.subTest(proof=proof):
                self.due()
                with patch('codex_claude.auth_metadata', return_value=proof):
                    auth_wait.tick(self.runtime)
                    f.f.f.eventually(lambda: not self.runtime.__dict__.get('_claude_auth_wait_jobs'))
                self.assertEqual(self.receipt()['status'], 'auth_probe_wait')
                self.assertGreater(self.receipt()['dueAt'], time.time() + 178)
                self.assertEqual(self.runtime.agent(self.agent['id']), before)
                self.assertEqual([self.runtime.delivery_receipt(key)['status'] for key in self.ids], ['pending', 'pending'])
        self.assertEqual(self.starts(), [])

    def test_parent_stop_cancels_only_the_wait_receipt(self):
        parent = self.runtime.new_lead({'cwd': str(self.root), 'account_key': 'claude-local'})
        with self.runtime.lock, self.runtime.db() as db:
            parent.update(autoWake=True, status='waiting')
            self.runtime.put(db, 'agents', parent)
            current = self.runtime.agent(self.agent['id'], db)
            current.update(parentId=parent['id'], rootId=parent['id'], isLead=False)
            self.runtime.put(db, 'agents', current)
        self.fail_auth()
        self.runtime.stop(parent['id'], descendants=False)
        before = self.runtime.agent(self.agent['id'])
        self.recover()
        self.assertEqual(self.receipt()['status'], 'cancelled')
        self.assertEqual(self.runtime.agent(self.agent['id']), before)
        self.assertEqual(self.starts(), [])

    def test_one_profile_probe_serves_two_exact_waits(self):
        other = self.runtime.new_lead({'cwd': str(self.root), 'account_key': 'claude-local'})
        other = self.runtime.prepare(other)
        self.fail_auth()
        first = self.agent
        self.agent, self.ids = other, ['other-auth-a', 'other-auth-b']
        self.fail_auth()
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("UPDATE runtime_usage_resumes SET record=json_set(record,'$.dueAt',0) "
                       "WHERE json_extract(record,'$.status')='auth_probe_wait'")
        with patch('codex_claude.auth_metadata', return_value=f.AUTH.copy()) as probe:
            auth_wait.tick(self.runtime)
            f.f.f.eventually(lambda: not self.runtime.__dict__.get('_claude_auth_wait_jobs'))
            self.assertEqual(probe.call_count, 1)
            self.assertTrue(probe.call_args.kwargs['force'])
        self.assertEqual(self.runtime.agent(first['id'])['status'], 'queued')
        self.assertEqual(self.runtime.agent(other['id'])['status'], 'queued')
        self.assertEqual(self.starts(), [])

    def test_changed_attachment_record_cancels_the_original_wait(self):
        path = self.root / 'attachment.txt'
        path.write_text('Private fixture attachment')
        asset = {'id': 'fixture-asset', 'agent': self.agent['id'], 'name': path.name,
                 'path': str(path), 'size': path.stat().st_size, 'image': False, 'mime': 'text/plain'}
        with self.runtime.lock, self.runtime.db() as db:
            db.execute('INSERT INTO runtime_assets VALUES (?,?)', (asset['id'], json.dumps(asset)))
        self.fail_auth(assets=[asset['id']])
        with self.runtime.lock, self.runtime.db() as db:
            db.execute('UPDATE runtime_assets SET record=? WHERE id=?',
                       (json.dumps({**asset, 'name': 'Changed attachment.txt'}), asset['id']))
        self.recover()
        self.assertEqual(self.receipt()['status'], 'cancelled')
        self.assertEqual(self.starts(), [])

    def test_profile_change_during_initial_timeout_is_not_a_typed_wait(self):
        def changed_profile(_profile=None, force=False):
            with self.runtime.accounts.lock:
                self.runtime.accounts._row('claude-local')['claudeOptions'] = {'configDir': '/changed-profile'}
            return TIMEOUT.copy()
        require_auth = auth_wait.require_auth
        def exact_preflight(runtime, agent):
            with patch('codex_claude.auth_metadata', side_effect=changed_profile):
                return require_auth(runtime, agent)
        with patch.object(auth_wait, 'require_auth', side_effect=exact_preflight):
            for key in self.ids:
                self.runtime.send(self.agent['id'], 'Exact input ' + key, key, manual=False)
            self.runtime.dispatch()
            f.f.f.eventually(lambda: self.runtime.agent(self.agent['id'])['status'] == 'failed')
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_usage_resumes WHERE agent=? "
                                        "AND json_extract(record,'$.cause')='claude_auth_probe'",
                                        (self.agent['id'],)).fetchone()[0], 0)
        self.assertEqual(self.starts(), [])


if __name__ == '__main__':
    suite = unittest.TestSuite(AuthWait(name) for name in AuthWait.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())

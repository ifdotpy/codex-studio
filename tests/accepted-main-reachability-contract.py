#!/usr/bin/env python3
"""Accepted task archive uses a fresh, scoped main commit without changing the checkout."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import subprocess
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('main_archive_fixture',
    Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class MainReachability(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update
    git = f.WorkspaceContract.git
    work = f.WorkspaceContract.work
    action = f.WorkspaceContract.action

    def project_with_result(self, remote=True, publish=True):
        self.git(self.project, 'init', '-q', '-b', 'main')
        self.git(self.project, 'config', 'user.name', 'Fixture')
        self.git(self.project, 'config', 'user.email', 'fixture@example.test')
        (self.project / 'tracked.txt').write_text('base\n')
        self.git(self.project, 'add', 'tracked.txt')
        self.git(self.project, 'commit', '-qm', 'base')
        (self.project / '.git' / 'info' / 'exclude').write_text('.worktrees/\n')
        self.base = self.git(self.project, 'rev-parse', 'HEAD')
        self.remote = self.root / 'upstream.git'
        if remote:
            self.git(self.root, 'init', '--bare', '-q', '-b', 'main', str(self.remote))
            self.git(self.project, 'remote', 'add', 'origin', str(self.remote))
            self.git(self.project, 'push', '-q', 'origin', 'main')
        self.actor = self.lead()
        self.owner = self.worker(self.actor)
        self.path = self.project / '.worktrees' / 'codex-agents' / self.owner['id']
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.git(self.project, 'worktree', 'add', '-q', '-b', 'worker-result', str(self.path), 'main')
        self.owner = self.agent_update(self.owner, role='implementer', cwd=str(self.path),
            worktree=True, worktreeReady=True, status='completed')
        (self.path / 'tracked.txt').write_text('accepted result\n')
        self.git(self.path, 'add', 'tracked.txt')
        self.git(self.path, 'commit', '-qm', 'worker result')
        self.revision = self.git(self.path, 'rev-parse', 'HEAD')
        if remote and publish:
            self.git(self.path, 'push', '-q', 'origin', 'HEAD:refs/heads/main')
            # Neither local main nor an old tracking ref proves the remote result.
            self.git(self.project, 'update-ref', 'refs/remotes/origin/main', self.base)
        self.task = self.work(self.actor, owner=self.owner['id'])
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=? AND kind='work_ready'",
                       (self.owner['id'],))
            current = self.runtime.agent(self.owner['id'], db)
            current.update(status='completed', inFlight=False)
            self.runtime.put(db, 'agents', current)
        self.task = self.action(self.owner, self.task, 'submit', result='Done',
            checks='Private Git checks passed', revision=self.revision)

    def accept(self):
        return self.runtime.work_action(self.actor['id'], {'action': 'accept',
            'task_id': self.task['id'], 'result': 'Reviewed'}, 'exact-accept', actor=self.actor['id'])

    def test_fresh_remote_main_archives_when_local_and_tracking_main_are_stale(self):
        self.project_with_result()
        self.git(self.project, 'config', '--add', 'remote.origin.fetch',
                 '+refs/heads/main:refs/heads/unrelated')
        (self.project / 'local.txt').write_text('keep uncommitted user file\n')
        before = self.git(self.project, 'status', '--porcelain')
        fetch_head = self.project / '.git' / 'FETCH_HEAD'
        fetch_head.write_text('keep previous fetch metadata\n')
        result = self.accept()
        self.assertEqual(result['archive']['status'], 'archived', result['archive'])
        self.assertEqual(result['archive']['mainEvidence']['source'], 'origin/main')
        self.assertEqual(result['archive']['mainEvidence']['commit'], self.revision)
        self.assertEqual(self.git(self.project, 'rev-parse', 'main'), self.base)
        self.assertEqual(self.git(self.project, 'rev-parse', 'origin/main'), self.base)
        self.assertEqual(self.git(self.project, 'status', '--porcelain'), before)
        self.assertEqual(fetch_head.read_text(), 'keep previous fetch metadata\n')
        self.assertEqual(self.git(self.project, 'for-each-ref', '--format=%(refname)',
                                 'refs/studio/archive-check'), '')
        self.assertEqual(self.git(self.project, 'for-each-ref', '--format=%(refname)',
                                 'refs/heads/unrelated'), '')
        self.assertFalse(self.path.exists())
        with patch('codex_work.manage_agent', side_effect=AssertionError('archive replay')):
            self.assertEqual(self.accept()['archive'], result['archive'])

    def test_local_main_remains_valid_for_unpublished_result_and_no_remote(self):
        for remote in (False, True):
            with self.subTest(remote=remote):
                # Each case gets a separate real Runtime and repository.
                if remote:
                    self.tearDown(); self.setUp()
                self.project_with_result(remote=remote, publish=False)
                self.git(self.project, 'update-ref', 'refs/heads/main', self.revision)
                result = self.accept()
                self.assertEqual(result['archive']['status'], 'archived', result['archive'])
                self.assertEqual(result['archive']['mainEvidence'],
                                 {'source': 'local main', 'commit': self.revision})

    def test_failed_remote_check_does_not_use_local_main_as_false_success(self):
        self.project_with_result(publish=False)
        self.git(self.project, 'update-ref', 'refs/heads/main', self.revision)
        self.git(self.project, 'remote', 'set-url', 'origin', str(self.root / 'missing.git'))
        result = self.accept()
        self.assertEqual(result['archive']['status'], 'kept')
        self.assertIn('origin/main could not be refreshed', result['archive']['reason'])
        self.assertNotIn('mainEvidence', result['archive'])
        self.assertTrue(self.path.exists())

    def test_foreign_only_remote_is_not_a_local_only_project(self):
        self.project_with_result(publish=False)
        self.git(self.project, 'update-ref', 'refs/heads/main', self.revision)
        self.git(self.project, 'remote', 'rename', 'origin', 'foreign')
        result = self.accept()
        self.assertEqual(result['archive']['status'], 'kept')
        self.assertIn('origin', result['archive']['reason'])
        self.assertTrue(self.path.exists())

    def test_origin_url_rewrite_runs_once_and_cannot_prove_foreign_main(self):
        self.project_with_result(publish=False)
        foreign = self.root / 'foreign.git'
        self.git(self.root, 'init', '--bare', '-q', '-b', 'main', str(foreign))
        self.git(self.path, 'push', '-q', str(foreign), 'HEAD:refs/heads/main')
        alias = 'private-proof-origin:repo'
        self.git(self.project, 'remote', 'set-url', 'origin', alias)
        self.git(self.project, 'config', 'url.' + str(self.remote) + '.insteadOf', alias)
        self.git(self.project, 'config', 'url.' + str(foreign) + '.insteadOf', str(self.remote))
        self.assertEqual(self.git(self.project, 'remote', 'get-url', 'origin'), str(self.remote))
        self.assertEqual(self.git(self.remote, 'rev-parse', 'main'), self.base)
        result = self.accept()
        self.assertEqual(result['archive']['status'], 'kept', result['archive'])
        self.assertIn('not reachable', result['archive']['reason'])
        self.assertNotIn('mainEvidence', result['archive'])
        self.assertTrue(self.path.exists())
        self.assertEqual(self.git(self.project, 'config', '--get', 'remote.origin.url'), alias)
        self.assertEqual(self.git(self.project, 'remote'), 'origin')

    def test_fetch_error_after_ref_commit_cleans_only_receipt_proved_ref(self):
        self.project_with_result()
        from codex_worktree_creation import _run_checkout
        def after_ref_failure(command, **kwargs):
            result = _run_checkout(command, **kwargs)
            self.assertLessEqual(kwargs['timeout'], 5)
            raise subprocess.CalledProcessError(1, command, output=result.stdout,
                                                stderr='private failure after ref commit')
        with patch('codex_worktree_creation._run_checkout', side_effect=after_ref_failure), \
                patch.object(self.runtime, '_queue_accepted_archive'):
            result = self.accept()
        self.assertEqual(result['archive']['status'], 'kept')
        self.assertIn('unconfirmed', result['archive']['reason'])
        self.assertTrue(result['archive']['retryable'])
        self.assertTrue(self.path.exists())
        self.assertEqual(self.git(self.project, 'for-each-ref', '--format=%(refname)',
                                 'refs/studio/archive-check'), '')

    def test_exact_old_accept_receipt_rechecks_only_archive(self):
        self.project_with_result()
        with patch.object(self.runtime, '_archive_accepted_owner', return_value={
                'status': 'kept', 'reason': 'The result commit is not reachable from main'}):
            first = self.accept()
        self.assertEqual(first['archive']['status'], 'kept')
        with self.runtime.lock, self.runtime.db() as db:
            before = json.loads(db.execute('SELECT record FROM runtime_work WHERE id=?',
                                            (self.task['id'],)).fetchone()[0])
            # The legacy keep sent a decision. This worker has read that exact event
            # and finished its turn before the lead retries the saved accept receipt.
            db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=? AND kind='work_decision'",
                       (self.owner['id'],))
            current = self.runtime.agent(self.owner['id'], db)
            current.update(status='completed', inFlight=False)
            self.runtime.put(db, 'agents', current)
        recovered = self.accept()
        self.assertEqual(recovered['archive']['status'], 'archived', recovered['archive'])
        with self.runtime.db() as db:
            after = json.loads(db.execute('SELECT record FROM runtime_work WHERE id=?',
                                          (self.task['id'],)).fetchone()[0])
            receipt = json.loads(db.execute('SELECT result FROM runtime_operation_receipts WHERE id=?',
                                            ('exact-accept',)).fetchone()[0])
            decisions = db.execute("SELECT count(*) FROM runtime_events WHERE agent=? AND kind='work_decision'",
                                   (self.owner['id'],)).fetchone()[0]
        self.assertEqual(after['results'], before['results'])
        self.assertEqual(after['decisions'], before['decisions'])
        self.assertEqual(after['version'], before['version'])
        self.assertEqual(decisions, 1)
        self.assertEqual(receipt['archive'], recovered['archive'])
        with patch('codex_work.manage_agent', side_effect=AssertionError('second archive')):
            self.assertEqual(self.accept()['archive'], recovered['archive'])

    def test_new_proved_unmerged_result_does_not_repeat_fetch_on_receipt_replay(self):
        self.project_with_result(publish=False)
        from codex_worktree_creation import _run_checkout
        with patch('codex_worktree_creation._run_checkout', wraps=_run_checkout) as fetch:
            first = self.accept()
            self.assertEqual(first['archive']['status'], 'kept')
            self.assertIn('not reachable from fresh origin/main or local main', first['archive']['reason'])
            self.assertEqual(self.accept()['archive'], first['archive'])
        self.assertEqual(fetch.call_count, 1)
        self.assertTrue(self.path.exists())

    def test_fetch_wait_has_no_runtime_lock_or_database_transaction(self):
        self.project_with_result()
        from codex_worktree_creation import _run_checkout
        entered, release = threading.Event(), threading.Event()
        errors, results = [], []
        def held(command, **kwargs):
            if 'fetch' in command:
                entered.set()
                if not release.wait(2):
                    raise AssertionError('private fetch barrier was not released')
            return _run_checkout(command, **kwargs)
        def accept():
            try: results.append(self.accept())
            except BaseException as error: errors.append(error)
        with patch('codex_worktree_creation._run_checkout', side_effect=held):
            thread = threading.Thread(target=accept)
            thread.start()
            try:
                self.assertTrue(entered.wait(2), 'archive did not fetch origin/main')
                self.assertTrue(self.runtime.lock.acquire(timeout=.2), 'fetch held Runtime.lock')
                self.runtime.lock.release()
                with self.runtime.db() as db:
                    db.execute('BEGIN IMMEDIATE')
                    db.execute('UPDATE runtime_work SET record=record WHERE id=?', (self.task['id'],))
            finally:
                release.set(); thread.join(3)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(results[0]['archive']['status'], 'archived')

    def test_changed_origin_or_fetched_ref_blocks_archive(self):
        for change in ('origin', 'ref'):
            with self.subTest(change=change):
                if change == 'ref':
                    self.tearDown(); self.setUp()
                self.project_with_result()
                from codex_worktree_creation import _run_checkout
                changed = False
                changed_ref = None
                def raced(command, **kwargs):
                    nonlocal changed, changed_ref
                    output = _run_checkout(command, **kwargs)
                    if 'fetch' in command and not changed:
                        changed = True
                        if change == 'origin':
                            self.git(self.project, 'remote', 'set-url', 'origin', str(self.root / 'other.git'))
                        else:
                            ref = next(arg.split(':', 1)[1] for arg in command
                                       if arg.startswith('+refs/heads/main:'))
                            self.git(self.project, 'update-ref', ref, self.base)
                            changed_ref = ref
                    return output
                with patch('codex_worktree_creation._run_checkout', side_effect=raced):
                    result = self.accept()
                self.assertEqual(result['archive']['status'], 'kept', result['archive'])
                self.assertTrue(self.path.exists())
                self.assertNotIn('mainEvidence', result['archive'])
                if changed_ref:
                    self.assertEqual(self.git(self.project, 'rev-parse', changed_ref), self.base)


if __name__ == '__main__':
    unittest.main()

#!/usr/bin/env python3
"""Public worker lifecycle callers keep exact identities across backend restarts."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
from pathlib import Path
import sys
import subprocess
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_catalog import CatalogUnavailable

spec = importlib.util.spec_from_file_location(
    'worker_defaults_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class WorkerLifecycleRestartContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='worker-lifecycle-restart-')
        self.root = Path(self.temp.name).resolve()
        self.state = self.root / 'state'
        self.catalog_calls = []
        self.runtime = self.open_runtime()
        self.lead = self.runtime.create({
            'name': 'Lifecycle lead', 'prompt': '', 'cwd': str(self.root),
            'account_key': 'parent-account'}, draft=True)

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def open_runtime(self):
        rt = fixture.ControlledRuntime(self.state, fixture.f.FakeServer)
        base_get = rt.accounts.get
        for key in ('parent-account', 'default', 'worker-explicit'):
            rt.accounts.data['accounts'][key] = {
                'id': key, 'provider': 'codex', 'status': 'ready', 'home': str(self.root)}
        fake_accounts = {'default', 'parent-account', 'worker-explicit'}
        rt.accounts.get = lambda key: (
            copy.deepcopy(rt.accounts.data['accounts'][key])
            if key in fake_accounts else base_get(key))
        rt.accounts.list = lambda: [
            copy.deepcopy(row) for row in rt.accounts.data['accounts'].values()
            if row['id'] in fake_accounts or not row.get('deleted')
        ]

        def catalog(account='default'):
            self.catalog_calls.append(account)
            return {'data': [{'model': 'gpt-6-luna'}]}
        rt.catalog = catalog
        return rt

    def batch(self):
        return {'agents': [
            {'name': 'Parent account', 'prompt': 'One', 'model': 'gpt-6-luna', 'role': 'reviewer'},
            {'name': 'Default account', 'prompt': 'Two', 'model': 'gpt-6-luna',
             'role': 'reviewer', 'account_key': 'default'},
            {'name': 'Explicit account', 'prompt': 'Three', 'model': 'gpt-6-luna',
             'role': 'reviewer', 'account_key': 'worker-explicit'},
        ]}

    def spawn(self, request_id='spawn-restart-identity'):
        actor = self.runtime.agent(self.lead['id'])
        return self.runtime.spawn_agents(actor, self.batch(), request_id)

    def agents(self):
        with self.runtime.db() as db:
            return [a for a in self.runtime.records(db, 'agents')
                    if a.get('parentId') == self.lead['id']]

    def restart(self):
        self.runtime.close()
        self.runtime = self.open_runtime()
        self.lead = self.runtime.agent(self.lead['id'])

    def test_spawn_batch_restarts_before_catalog_after_validation_and_after_commit(self):
        entered = threading.Event()
        release = threading.Event()
        old_catalog = self.runtime.catalog

        def pending(account='default'):
            if account == 'parent-account':
                entered.set()
                release.wait(10)
                raise CatalogUnavailable('old backend stopped during catalog read')
            return old_catalog(account)
        self.runtime.catalog = pending
        failures = []

        def old_spawn():
            try:
                self.spawn()
            except Exception as error:
                failures.append(error)
        thread = threading.Thread(target=old_spawn)
        thread.start()
        self.assertTrue(entered.wait(5), 'spawn did not reach the pending catalog request')
        self.restart()
        result = self.spawn()
        release.set()
        thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(len(failures), 1)
        self.assertEqual([agent['accountKey'] for agent in result['agents']],
                         ['parent-account', 'default', 'worker-explicit'])
        self.assertEqual(len(self.agents()), 3)

        self.restart()
        original_records = self.runtime.records
        raised = False

        def crash_after_validation(db, table=None, **kwargs):
            nonlocal raised
            if table == 'work' and not raised:
                raised = True
                raise RuntimeError('simulated backend restart after batch validation')
            return original_records(db, table, **kwargs)
        with patch.object(self.runtime, 'records', side_effect=crash_after_validation):
            with self.assertRaisesRegex(RuntimeError, 'after batch validation'):
                self.spawn('validated-batch')
        self.assertEqual(len(self.agents()), 3)
        self.restart()
        validated = self.spawn('validated-batch')
        self.assertEqual(len(validated['agents']), 3)
        self.assertEqual(len(self.agents()), 6)

        committed = self.spawn('committed-batch')
        self.assertEqual(len(committed['agents']), 3)
        expected = [(row['id'], row['accountKey']) for row in committed['agents']]
        self.restart()
        replayed = self.spawn('committed-batch')
        self.assertEqual([(row['id'], row['accountKey']) for row in replayed['agents']], expected)
        self.assertEqual(len(self.agents()), 9)

    def make_repo(self):
        repo = self.root / 'project'
        repo.mkdir(exist_ok=True)
        if not (repo / '.git').exists():
            subprocess.run(['git', 'init', '-q', '-b', 'main', str(repo)], check=True)
            subprocess.run(['git', '-C', str(repo), 'config', 'user.name', 'Fixture'], check=True)
            subprocess.run(['git', '-C', str(repo), 'config', 'user.email', 'fixture@example.test'], check=True)
            (repo / 'tracked.txt').write_text('fixture\n')
            subprocess.run(['git', '-C', str(repo), 'add', 'tracked.txt'], check=True)
            subprocess.run(['git', '-C', str(repo), 'commit', '-qm', 'fixture'], check=True)
        return repo

    def make_worktree_worker(self, repo, *, active=False):
        worker = self.runtime.create({
            'name': 'Archive worker', 'prompt': 'Complete the assigned task',
            'role': 'implementer', 'model': 'gpt-6-luna', 'cwd': str(repo),
            '_worktree': False,
        }, parent=self.lead['id'], defer=True,
            _catalog=('parent-account', {'data': [{'model': 'gpt-6-luna'}]}))
        worker_id = worker['id']
        worktree = repo / '.worktrees' / 'codex-agents' / worker_id
        worktree.parent.mkdir(parents=True, exist_ok=True)
        branch = 'worker-' + worker_id
        subprocess.run(['git', '-C', str(repo), 'worktree', 'add', '-qb', branch, str(worktree), 'main'],
                       check=True)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(worker_id, db)
            current.update(cwd=str(worktree), branch=branch, worktreeReady=True,
                           worktree=True, worktreeWarning=None,
                           status='running' if active else 'completed',
                           autoWake=active, inFlight=active, threadId=None)
            self.runtime.put(db, 'agents', current)
        return worker_id, worktree

    def restart_runtime(self):
        self.runtime.close()
        self.runtime = self.open_runtime()
        self.lead = self.runtime.agent(self.lead['id'])

    def owned_worker(self, label):
        worker = self.runtime.create({
            'name': label, 'prompt': 'Work on the assigned task', 'role': 'reviewer',
            'model': 'gpt-6-luna', 'cwd': str(self.root),
        }, parent=self.lead['id'], defer=True,
            _catalog=('parent-account', {'data': [{'model': 'gpt-6-luna'}]}))
        task = self.runtime.work_action(self.lead['id'], {
            'action': 'create', 'title': label, 'description': 'Lifecycle fixture'},
            key=label + '-task-create')
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(worker['id'], db)
            current.update(status='running', autoWake=True, inFlight=True)
            self.runtime.put(db, 'agents', current)
        self.runtime.work_action(worker['id'], {
            'action': 'claim', 'task_id': task['id']},
            key=label + '-task-claim', actor=worker['id'])
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(worker['id'], db)
            current.update(threadId='thread-' + label, turnId='turn-' + label,
                           turnEpoch=current['epoch'], status='running', inFlight=True,
                           autoWake=True)
            self.runtime.put(db, 'agents', current)
        return worker['id'], task['id']

    def fail_owned_worker(self, worker_id, label, error_code):
        server = self.runtime.connect('parent-account')
        server.notify({'method': 'turn/completed', 'params': {
            'threadId': 'thread-' + label,
            'turn': {'id': 'turn-' + label, 'status': 'failed',
                     'error': {'message': 'Fixture failure', 'codexErrorInfo': error_code}},
        }})

    def task_owner(self, task_id):
        with self.runtime.db() as db:
            return self.runtime.work_by_id(db, task_id, self.lead['id'])['owner']

    def test_retry_ownership_survives_scheduled_and_unknown_restart_boundaries(self):
        worker_id, task_id = self.owned_worker('capacity-scheduled')
        self.fail_owned_worker(worker_id, 'capacity-scheduled', 'serverOverloaded')
        self.restart_runtime()
        retry = self.runtime.agent(worker_id)['capacityRetry']
        self.assertEqual(retry['status'], 'scheduled')
        self.assertEqual(retry['taskClaims'], [task_id])
        with self.runtime.lock, self.runtime.db() as db:
            self.assertEqual(self.runtime.release_failed_work(
                db, self.runtime.records(db, 'agents'), force=True), [])
        self.assertEqual(self.task_owner(task_id), worker_id)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(worker_id, db)
            current['capacityRetry']['dueAt'] = time.time() - 1
            self.runtime.capacity_save(db, current, current['capacityRetry'])
            self.runtime.put(db, 'agents', current)
        self.runtime.capacity_tick()
        fixture.f.eventually(lambda: bool(self.runtime.agent(worker_id).get('turnId')))
        starts = [call for call in self.runtime.connect('parent-account').calls
                  if call[0] == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.restart_runtime()
        self.assertEqual(self.runtime.agent(worker_id)['capacityRetry']['status'], 'unknown')
        self.runtime.capacity_tick()
        self.assertEqual(len([call for call in self.runtime.connect('parent-account').calls
                              if call[0] == 'turn/start']), 0)
        self.assertEqual(self.task_owner(task_id), worker_id)

        worker_usage, task_usage = self.owned_worker('usage-scheduled')
        self.fail_owned_worker(worker_usage, 'usage-scheduled', 'usageLimitExceeded')
        usage = self.runtime.agent(worker_usage)['usageResume']
        self.assertEqual(usage['status'], 'scheduled')
        self.assertEqual(usage['taskClaims'], [task_usage])
        self.restart_runtime()
        self.assertEqual(self.runtime.agent(worker_usage)['usageResume']['status'], 'scheduled')
        self.assertEqual(self.task_owner(task_usage), worker_usage)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(worker_usage, db)
            resume = current['usageResume']
            resume['dueAt'] = time.time() - 1
            self.runtime.usage_resume_save(db, current, resume)
            self.runtime.put(db, 'agents', current)
        self.runtime.usage_resume_tick()
        started = self.runtime.agent(worker_usage)['usageResume']
        self.assertEqual(started['status'], 'started')
        self.assertEqual(self.task_owner(task_usage), worker_usage)
        self.restart_runtime()
        self.runtime.usage_resume_tick()
        with self.runtime.db() as db:
            self.assertEqual(db.execute(
                "SELECT COUNT(*) FROM runtime_events WHERE id=?",
                ('usage-resume:' + started['id'],)).fetchone()[0], 1)
        self.assertEqual(self.task_owner(task_usage), worker_usage)

        worker_unknown, task_unknown = self.owned_worker('capacity-unknown')
        self.fail_owned_worker(worker_unknown, 'capacity-unknown', 'serverOverloaded')
        unknown = self.runtime.agent(worker_unknown)['capacityRetry']
        self.runtime.connect('parent-account').fail_start = True
        self.runtime.capacity_retry(worker_unknown, unknown['id'], 'retry')
        fixture.f.eventually(lambda: self.runtime.agent(worker_unknown)
                             ['capacityRetry']['status'] == 'unknown')
        self.assertEqual(self.task_owner(task_unknown), worker_unknown)
        self.restart_runtime()
        self.assertEqual(self.runtime.agent(worker_unknown)['capacityRetry']['status'], 'unknown')
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(worker_unknown, db)
            current.update(status='failed', autoWake=True)
            self.runtime.put(db, 'agents', current)
            released = self.runtime.release_failed_work(
                db, self.runtime.records(db, 'agents'), force=True)
        self.assertEqual(released, [])
        self.assertEqual(self.task_owner(task_unknown), worker_unknown)
        self.assertEqual(self.runtime.capacity_retry(
            worker_unknown, unknown['id'], 'retry')['status'], 'unknown')

    def test_automatic_continuations_stop_when_the_assigned_task_changes(self):
        worker_id, task_id = self.owned_worker('capacity-task-changed')
        self.fail_owned_worker(worker_id, 'capacity-task-changed', 'serverOverloaded')
        retry = self.runtime.agent(worker_id)['capacityRetry']
        self.runtime.work_action(self.lead['id'], {'action': 'cancel', 'task_id': task_id,
            'reason': 'Reassigned by lead'}, key='cancel-capacity-task')
        self.assertEqual(self.runtime.capacity_retry(
            worker_id, retry['id'], 'retry', _automatic=True)['status'], 'cancelled')

        worker_usage, task_usage = self.owned_worker('usage-task-changed')
        self.fail_owned_worker(worker_usage, 'usage-task-changed', 'usageLimitExceeded')
        resume = self.runtime.agent(worker_usage)['usageResume']
        self.runtime.work_action(self.lead['id'], {'action': 'cancel', 'task_id': task_usage,
            'reason': 'Reassigned by lead'}, key='cancel-usage-task')
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(worker_usage, db)
            current['usageResume']['dueAt'] = time.time() - 1
            self.runtime.usage_resume_save(db, current, current['usageResume'])
            self.runtime.put(db, 'agents', current)
        self.runtime.usage_resume_tick()
        self.assertEqual(self.runtime.agent(worker_usage)['usageResume']['status'], 'cancelled')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_events WHERE id=?',
                ('child-stop:' + worker_usage + ':' + str(self.runtime.agent(worker_usage)['epoch'])
                 + ':hold:' + resume['id'] + ':task-changed',)).fetchone()[0], 1)

    def test_accept_intent_retries_after_restart_and_archives_after_turn(self):
        repo = self.make_repo()
        worker_id, _ = self.make_worktree_worker(repo, active=True)
        task = self.runtime.work_action(self.lead['id'], {
            'action': 'create', 'title': 'Restart safe archive', 'owner': worker_id},
            key='archive-task-create')
        self.runtime.work_action(worker_id, {'action': 'claim', 'task_id': task['id']},
                                 key='archive-task-claim', actor=worker_id)
        # Model receipt of this assignment before the fixture worker submits its result.
        with self.runtime.lock, self.runtime.db() as db:
            assignment_id = 'work-ready:' + task['id'] + ':assignment:1'
            self.assertEqual(db.execute(
                "SELECT kind FROM runtime_events WHERE id=? AND agent=?",
                (assignment_id, worker_id)).fetchone()[0], 'work_ready')
            db.execute("UPDATE runtime_events SET status='delivered' WHERE id=? AND agent=?",
                       (assignment_id, worker_id))
        revision = subprocess.check_output(['git', '-C', str(repo), 'rev-parse', 'HEAD'], text=True).strip()
        self.runtime.work_action(worker_id, {'action': 'submit', 'task_id': task['id'],
            'result': 'Done', 'checks': 'fixture passed', 'revision': revision},
            key='archive-task-submit', actor=worker_id)
        accepted = self.runtime.work_action(self.lead['id'], {'action': 'accept',
            'task_id': task['id'], 'result': 'Verified'}, key='archive-task-accept')
        self.assertTrue(accepted['archivePending'])
        with self.runtime.db() as db:
            work = self.runtime.work_by_id(db, task['id'], self.lead['id'])
            self.assertEqual(work['archiveIntent']['status'], 'pending')
        self.restart_runtime()
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            # ControlledRuntime omits dispatch; advance its archive scheduler explicitly.
            self.runtime.accepted_archive_tick()
            with self.runtime.db() as db:
                work = self.runtime.work_by_id(db, task['id'], self.lead['id'])
                agent = self.runtime.agent(worker_id, db)
            if work.get('archiveIntent', {}).get('status') == 'complete':
                break
            time.sleep(.05)
        self.assertEqual(work['archiveIntent']['status'], 'complete')
        self.assertEqual(work['archive']['status'], 'archived')
        self.assertTrue(agent.get('deletedAt'))

    def test_review_archive_marker_recovers_after_restart(self):
        repo = self.make_repo()
        worker_id, _worktree = self.make_worktree_worker(repo)
        event_id = 'review-delivered-before-restart'
        with self.runtime.lock, self.runtime.db() as db:
            lead = self.runtime.agent(self.lead['id'], db)
            self.runtime.enqueue(db, lead, 'work_review', 'Review passed', event_id)
            db.execute("UPDATE runtime_events SET status='delivered' WHERE id=?", (event_id,))
            child = self.runtime.agent(worker_id, db)
            child['reviewArchiveScheduled'] = event_id
            self.runtime.put(db, 'agents', child)
        self.restart_runtime()
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            agent = self.runtime.agent(worker_id)
            if agent.get('deletedAt'):
                break
            time.sleep(.05)
        self.assertTrue(agent.get('deletedAt'))
        self.assertEqual(agent['agentArchive']['reason'], 'Reviewer result delivered to parent')

    def test_archive_cleanup_recovers_after_ref_removal_and_metadata_boundaries(self):
        import codex_agent_management as management
        repo = self.make_repo()
        original_git = management._git
        for boundary in ('archive-ref', 'git-remove', 'metadata'):
            worker_id, worktree = self.make_worktree_worker(repo)
            crashed = False

            def stop_after_boundary(path, *args):
                nonlocal crashed
                result = original_git(path, *args)
                matches = ((boundary == 'archive-ref' and args[:1] == ('update-ref',))
                           or (boundary == 'git-remove' and args[:2] == ('worktree', 'remove'))
                           or (boundary == 'metadata' and args[:1] == ('worktree', 'remove')))
                if matches and not crashed and boundary != 'metadata':
                    crashed = True
                    raise SystemExit('simulated process exit')
                return result

            if boundary == 'metadata':
                original_put = self.runtime.put
                def put_then_exit(db, table, record):
                    nonlocal crashed
                    original_put(db, table, record)
                    if (table == 'agents' and record.get('id') == worker_id
                            and record.get('cleanedWorktree') and not record.get('worktreeCleanup')
                            and record.get('agentArchive', {}).get('cleanupPending') and not crashed):
                        crashed = True
                        raise SystemExit('simulated process exit after metadata update')
                put_patch = patch.object(self.runtime, 'put', side_effect=put_then_exit)
            else:
                put_patch = None
            git_patch = patch.object(management, '_git', side_effect=stop_after_boundary)
            git_patch.start()
            if put_patch:
                put_patch.start()
            try:
                with self.assertRaises(SystemExit):
                    management.manage_agent(self.runtime, self.lead['id'], {
                        'action': 'archive', 'agent_id': worker_id, 'reason': 'accepted fixture'},
                        self.lead['epoch'])
            finally:
                git_patch.stop()
                if put_patch:
                    put_patch.stop()
            self.assertTrue(crashed)
            self.restart_runtime()
            archived = management.manage_agent(self.runtime, self.lead['id'], {
                'action': 'archive', 'agent_id': worker_id, 'reason': 'accepted fixture'},
                self.lead['epoch'])
            self.assertEqual(archived['status'], 'archived')
            current = self.runtime.agent(worker_id)
            self.assertFalse(current.get('worktreeReady'))
            self.assertFalse(current['agentArchive'].get('cleanupPending'))
            self.assertFalse(worktree.exists())


if __name__ == '__main__':
    unittest.main()

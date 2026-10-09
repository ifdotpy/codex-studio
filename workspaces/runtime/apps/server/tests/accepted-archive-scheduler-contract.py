#!/usr/bin/env python3
"""Archive retries honor durable deadlines and retire existing legacy frames."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
from contextlib import contextmanager
import copy
import importlib.util
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_work


def legacy_archive_loop(self, task_id, intent_id):
    # Preserve the loop from b57c3376 WorkMixin._run_accepted_archive.
    # Keep its id guards, outcome commit, shared wait and finally for frame tests.
    delay = 1
    key = (task_id, intent_id)
    try:
        while not self.closed:
            with self.lock, self.db() as db:
                row = db.execute('SELECT record FROM runtime_work WHERE id=?', (task_id,)).fetchone()
                if not row:
                    return
                work = json.loads(row[0])
                intent = work.get('archiveIntent') or {}
                if intent.get('id') != intent_id or intent.get('status') != 'pending':
                    return
                owner = self.agent(work['owner'], db) if work.get('owner') else None
                root = self.agent(work['rootId'], db)
                if work.get('status') != 'accepted' or not owner or owner.get('deletedAt'):
                    intent.update(status='terminal', updated=time.time(),
                                  outcome={'status': 'kept', 'reason': 'The accepted task owner changed'})
                    work['archiveIntent'] = intent
                    self.put(db, 'work', work)
                    return
                snapshot = work
                actor_epoch = root['epoch']
            try:
                outcome = self._archive_accepted_owner(work['rootId'], snapshot, actor_epoch)
            except Exception as error:
                outcome = {'status': 'kept', 'reason': 'The archive check failed: ' + str(error)[:180]}
            with self.lock, self.db() as db:
                row = db.execute('SELECT record FROM runtime_work WHERE id=?', (task_id,)).fetchone()
                if not row:
                    return
                current = json.loads(row[0])
                intent = current.get('archiveIntent') or {}
                if intent.get('id') != intent_id or intent.get('status') != 'pending':
                    return
                current['archive'] = outcome
                if outcome.get('retryable'):
                    attempts = intent.get('attempts', 0) + 1
                    delay = min(60, 2 ** min(attempts, 6))
                    intent.update(attempts=attempts, nextAttemptAt=time.time() + delay,
                                  lastOutcome=outcome, updated=time.time())
                    current['archiveIntent'] = intent
                else:
                    intent.update(status='complete' if outcome.get('status') == 'archived' else 'terminal',
                                  updated=time.time(), outcome=outcome)
                    current['archiveIntent'] = intent
                self.put(db, 'work', current)
            if not outcome.get('retryable'):
                return
            self.changed.wait(delay)
            self.changed.clear()
    finally:
        with self.lock:
            self.__dict__.setdefault('_accepted_archive_running', set()).discard(key)


class SharedEvent:
    def __init__(self):
        self.event = threading.Event()
        self.entered = threading.Event()
        self.waits = []
        self.clears = 0

    def set(self):
        self.event.set()

    def is_set(self):
        return self.event.is_set()

    def wait(self, delay):
        self.waits.append(delay)
        self.entered.set()
        return self.event.wait(delay)

    def clear(self):
        self.clears += 1
        self.event.clear()


class ArchiveFixture(codex_work.WorkMixin):
    def __init__(self, root):
        self.path = Path(root) / 'archive.sqlite3'
        self.lock = threading.RLock()
        self.closed = False
        self.changed = SharedEvent()
        self.release = threading.Event()
        self.release.set()
        self.calls = []
        self.calls_changed = threading.Event()
        self.effects = 0
        self.callback = lambda *args: {'status': 'kept', 'retryable': True, 'reason': 'owner busy'}
        self.executor = concurrent.futures.ThreadPoolExecutor(max_workers=1)
        self.reads = 0
        with self.db() as db:
            db.execute('PRAGMA journal_mode=WAL')
            db.execute('CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL)')
            db.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL)')
            for identity in ('lead', 'worker'):
                self.put(db, 'agents', {'id': identity, 'rootId': 'lead', 'epoch': 1})
            self.put(db, 'work', {
                'id': 'task', 'status': 'accepted', 'rootId': 'lead', 'owner': 'worker',
                'decisions': [{'resultId': 'result', 'reason': 'accepted'}],
                'archiveIntent': {'id': 'task:result', 'status': 'pending', 'owner': 'worker',
                                  'created': time.time(), 'attempts': 0}})

    @contextmanager
    def db(self, busy_timeout=None):
        db = sqlite3.connect(self.path, timeout=.2 if busy_timeout is None else busy_timeout / 1000)
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    @contextmanager
    def read_db(self):
        self.reads += 1
        with self.db() as db:
            db.execute('PRAGMA query_only=ON')
            yield db

    def put(self, db, table, record):
        db.execute('INSERT OR REPLACE INTO runtime_' + table + ' VALUES (?,?)',
                   (record['id'], json.dumps(record)))

    def agent(self, identity, db):
        return json.loads(db.execute('SELECT record FROM runtime_agents WHERE id=?',
                                     (identity,)).fetchone()[0])

    def work(self):
        with self.db() as db:
            return json.loads(db.execute('SELECT record FROM runtime_work WHERE id=?', ('task',)).fetchone()[0])

    def edit(self, operation):
        with self.lock, self.db() as db:
            row = db.execute('SELECT record FROM runtime_work WHERE id=?', ('task',)).fetchone()
            work = json.loads(row[0])
            operation(work)
            self.put(db, 'work', work)

    def delivery_executor(self):
        return self.executor

    def _archive_accepted_owner(self, actor, work, epoch):
        self.calls.append((actor, copy.deepcopy(work), epoch, time.time()))
        self.calls_changed.set()
        return self.callback(actor, work, epoch)

    def tick(self):
        self.__dict__.pop('_accepted_archive_tick_at', None)
        self.accepted_archive_tick()

    def idle(self):
        self.executor.submit(lambda: None).result(timeout=1)

    def close(self):
        self.closed = True
        self.release.set()
        self.changed.set()
        self.executor.shutdown(wait=True, cancel_futures=True)


def frame_contains(worker, code):
    frame = sys._current_frames().get(worker.ident)
    while frame is not None:
        if frame.f_code is code:
            return True
        frame = frame.f_back
    return False


class AcceptedArchiveScheduler(unittest.TestCase):
    legacy = staticmethod(legacy_archive_loop)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='accepted-archive-scheduler-')
        self.runtime = ArchiveFixture(self.temp.name)

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def test_future_deadline_and_shared_wakes_do_not_submit_or_clear(self):
        rt = self.runtime
        rt.edit(lambda work: work['archiveIntent'].update(nextAttemptAt=time.time() + 60))
        original = rt.work()
        for _ in range(20):
            rt.changed.set()
            rt._queue_accepted_archive('task', 'task:result')
            rt.tick()
        rt.idle()
        self.assertEqual(rt.calls, [])
        self.assertEqual(rt.changed.waits, [])
        self.assertEqual(rt.changed.clears, 0)
        self.assertTrue(rt.changed.is_set())
        self.assertEqual(rt.work()['archiveIntent']['id'], original['archiveIntent']['id'])
        self.assertEqual(rt.work()['decisions'], original['decisions'])
        rt._run_accepted_archive('task', 'task:result')
        self.assertEqual(rt.calls, [])

    def test_one_attempt_releases_pool_and_due_tick_retries_once(self):
        rt = self.runtime
        rt.changed.set()
        rt.tick()
        rt.idle()
        self.assertEqual(len(rt.calls), 1)
        saved = rt.work()['archiveIntent']
        self.assertEqual(saved['attempts'], 1)
        self.assertGreater(saved['nextAttemptAt'], time.time())
        self.assertEqual(rt._accepted_archive_running, set())
        for _ in range(20):
            rt.tick()
        rt.idle()
        self.assertEqual(len(rt.calls), 1)
        rt.edit(lambda work: work['archiveIntent'].update(nextAttemptAt=0))
        rt.tick()
        rt.idle()
        self.assertEqual(len(rt.calls), 2)
        self.assertEqual(rt.work()['archiveIntent']['attempts'], 2)
        self.assertEqual(rt.work()['archiveIntent']['id'], 'task:result')
        self.assertEqual(rt.changed.waits, [])
        self.assertEqual(rt.changed.clears, 0)

    def test_throttle_and_partial_due_index_skip_rich_history(self):
        rt = self.runtime
        rt.edit(lambda work: work['archiveIntent'].update(nextAttemptAt=time.time() + 60))
        with rt.db() as db:
            for index in range(40):
                rt.put(db, 'work', {'id': str(index), 'status': 'accepted', 'description': 'x' * 30000})
        rt.accepted_archive_tick()
        reads = rt.reads
        for _ in range(100):
            rt.accepted_archive_tick()
        self.assertEqual(rt.reads, reads)
        with rt.db() as db:
            plan = [row[3] for row in db.execute(
                "EXPLAIN QUERY PLAN SELECT id,record FROM runtime_work "
                "WHERE json_extract(record,'$.archiveIntent.status')='pending' "
                "AND COALESCE(json_extract(record,'$.archiveIntent.nextAttemptAt'),0)<=? "
                "ORDER BY COALESCE(json_extract(record,'$.archiveIntent.nextAttemptAt'),0),id LIMIT 16",
                (time.time(),))]
        self.assertTrue(any('SEARCH' in line and 'runtime_work_archive_due' in line for line in plan), plan)
        self.assertFalse(any('TEMP B-TREE' in line for line in plan), plan)
        print('archive due query:', plan)

    def test_index_busy_does_not_hold_runtime_lock_and_retries_later(self):
        rt = self.runtime
        writer = sqlite3.connect(rt.path)
        writer.execute('BEGIN IMMEDIATE')
        try:
            before = time.monotonic()
            worker = threading.Thread(target=rt.accepted_archive_tick)
            worker.start()
            with rt.lock:
                pass
            worker.join(.5)
            self.assertFalse(worker.is_alive())
            self.assertLess(time.monotonic() - before, .5)
            self.assertFalse(rt.__dict__.get('_accepted_archive_index_ready'))
            self.assertEqual(rt.calls, [])
        finally:
            writer.rollback()
            writer.close()
        rt.tick()
        rt.idle()
        self.assertTrue(rt._accepted_archive_index_ready)
        self.assertEqual(len(rt.calls), 1)

    def test_same_task_keys_closed_and_failed_submit_preserve_claims(self):
        rt = self.runtime
        rt._accepted_archive_running = {('task', 'different-generation')}
        rt.tick()
        rt._run_accepted_archive('task', 'task:result')
        self.assertEqual(rt.calls, [])
        self.assertEqual(rt._accepted_archive_running, {('task', 'different-generation')})
        self.assertEqual(rt.work()['archiveIntent']['id'], 'task:result')
        rt._accepted_archive_running.clear()
        rt.closed = True
        rt._queue_accepted_archive('task', 'task:result')
        self.assertEqual(rt.calls, [])
        rt.closed = False
        rt.executor.shutdown(wait=True)
        with self.assertRaises(RuntimeError):
            rt._queue_accepted_archive('task', 'task:result')
        self.assertEqual(rt._accepted_archive_running, set())

    def test_reservation_commit_failure_releases_only_the_owned_claim(self):
        rt = self.runtime
        original = rt.db

        @contextmanager
        def rollback_commit(*args, **kwargs):
            with original(*args, **kwargs) as db:
                yield db
                raise sqlite3.OperationalError('injected commit failure')

        rt._accepted_archive_running = {('foreign-task', 'foreign-intent')}
        rt.db = rollback_commit
        with self.assertRaisesRegex(sqlite3.OperationalError, 'commit failure'):
            rt._queue_accepted_archive('task', 'task:result')
        rt.db = original
        self.assertEqual(rt._accepted_archive_running, {('foreign-task', 'foreign-intent')})
        self.assertNotIn('schedulerVersion', rt.work()['archiveIntent'])
        self.assertEqual(rt.calls, [])
        rt._queue_accepted_archive('task', 'task:result')
        rt.idle()
        self.assertEqual(len(rt.calls), 1)
        rt.edit(lambda work: work['archiveIntent'].update(status='pending', nextAttemptAt=0))
        rt.edit(lambda work: work['archiveIntent'].pop('schedulerVersion', None))
        rt._accepted_archive_running = {('task', 'task:result')}
        rt.db = rollback_commit
        with self.assertRaisesRegex(sqlite3.OperationalError, 'commit failure'):
            rt._queue_accepted_archive('task', 'task:result')
        rt.db = original
        self.assertEqual(rt._accepted_archive_running, {('task', 'task:result')})
        self.assertEqual(rt.work()['archiveIntent']['id'], 'task:result')

    def start_legacy(self):
        rt = self.runtime
        live = codex_work.WorkMixin._run_accepted_archive
        code = live.__code__
        live.__code__ = self.legacy.__code__
        rt._accepted_archive_running = {('task', 'task:result')}
        worker = threading.Thread(target=rt._run_accepted_archive, args=('task', 'task:result'))
        worker.start()
        return live, code, worker

    def test_active_old_wait_frame_is_invalidated_before_replacement(self):
        rt = self.runtime
        live, code, worker = self.start_legacy()
        try:
            self.assertTrue(rt.changed.entered.wait(1))
            self.assertTrue(frame_contains(worker, self.legacy.__code__))
            live.__code__ = code
            due = rt.work()['archiveIntent']['nextAttemptAt']
            rt.tick()
            migrated = rt.work()['archiveIntent']
            self.assertEqual(migrated['id'], 'task:result:scheduler-v2')
            self.assertEqual(migrated['previousId'], 'task:result')
            self.assertEqual(migrated['nextAttemptAt'], due)
            for _ in range(3):
                rt.tick()
            self.assertEqual(len(rt.calls), 1)
            self.assertEqual(rt._accepted_archive_running, {('task', 'task:result')})
            rt.changed.set()
            worker.join(1)
            self.assertFalse(worker.is_alive())
            rt.tick()
            rt.idle()
            self.assertEqual(len(rt.calls), 1)
            rt.edit(lambda work: work['archiveIntent'].update(nextAttemptAt=0))
            rt.tick()
            rt.idle()
            self.assertEqual(len(rt.calls), 2)
            self.assertEqual(rt.changed.waits, [2])
            self.assertEqual(rt.changed.clears, 1)
        finally:
            live.__code__ = code
            rt.closed = True
            rt.changed.set()
            worker.join(1)

    def test_old_committed_archive_is_reconciled_without_second_operation(self):
        rt = self.runtime
        committed = threading.Event()
        rt.release.clear()

        def archive(*args):
            with rt.lock, rt.db() as db:
                owner = rt.agent('worker', db)
                at = time.time()
                owner.update(deletedAt=at, epoch=2, worktreeReady=False,
                             cleanedWorktree={'bytes': 42},
                             agentArchive={'at': at, 'by': 'lead', 'epoch': 2,
                                           'reason': 'Accepted task result is on main',
                                           'cleanupPending': False})
                rt.put(db, 'agents', owner)
                rt.effects += 1
            committed.set()
            self.assertTrue(rt.release.wait(2))
            return {'status': 'archived', 'worktree': {'state': 'removed', 'bytes': 42}}

        rt.callback = archive
        live, code, worker = self.start_legacy()
        try:
            self.assertTrue(committed.wait(1))
            self.assertTrue(frame_contains(worker, self.legacy.__code__))
            live.__code__ = code
            rt.tick()
            rt.tick()
            self.assertEqual(rt.effects, 1)
            self.assertEqual(len(rt.calls), 1)
            self.assertEqual(rt._accepted_archive_running, {('task', 'task:result')})
            rt.release.set()
            worker.join(1)
            self.assertFalse(worker.is_alive())
            rt.tick()
            rt.idle()
            saved = rt.work()
            self.assertEqual(saved['archiveIntent']['status'], 'complete')
            self.assertEqual(saved['archive'], {'status': 'archived', 'worktree': {'state': 'removed', 'bytes': 42}})
            self.assertEqual(rt.effects, 1)
            self.assertEqual(len(rt.calls), 1)
        finally:
            live.__code__ = code
            rt.closed = True
            rt.release.set()
            worker.join(1)

    def test_unconfirmed_deleted_archive_never_replays_management(self):
        rt = self.runtime
        for field, value in [('by', 'other-lead'), ('reason', 'other archive'),
                             ('cleanupPending', True), ('epoch', 99), ('at', 0)]:
            with self.subTest(field=field):
                rt.edit(lambda work: work['archiveIntent'].update(status='pending', nextAttemptAt=0))
                with rt.db() as db:
                    owner = rt.agent('worker', db)
                    at = time.time()
                    receipt = {'at': at, 'by': 'lead', 'epoch': 2,
                               'reason': 'Accepted task result is on main', 'cleanupPending': False}
                    receipt[field] = value
                    owner.update(deletedAt=at, epoch=2, agentArchive=receipt)
                    rt.put(db, 'agents', owner)
                rt.tick()
                rt.idle()
                self.assertEqual(rt.work()['archiveIntent']['status'], 'terminal')
                self.assertEqual(rt.work()['archive']['status'], 'kept')
                self.assertEqual(rt.calls, [])

    def test_archive_concurrency_reserves_delivery_capacity(self):
        rt = self.runtime

        class Recorder:
            def __init__(self):
                self.jobs = []

            def submit(self, *args):
                self.jobs.append(args)

        recorder = Recorder()
        rt.delivery_executor = lambda: recorder
        with rt.db() as db:
            original = rt.work()
            for index in range(8):
                work = copy.deepcopy(original)
                work['id'] = 'other-' + str(index)
                work['archiveIntent']['id'] = work['id'] + ':result'
                rt.put(db, 'work', work)
        rt.tick()
        self.assertEqual(len(recorder.jobs), 4)
        self.assertEqual(len(rt._accepted_archive_running), 4)

    def test_bounded_legacy_scan_rotates_past_queued_keys(self):
        rt = self.runtime
        original = rt.work()
        original['archiveIntent']['nextAttemptAt'] = time.time() + 60
        with rt.db() as db:
            rt.put(db, 'work', original)
            for index in range(64):
                work = copy.deepcopy(original)
                work['id'] = 'other-' + str(index).zfill(3)
                work['archiveIntent']['id'] = work['id'] + ':result'
                rt.put(db, 'work', work)
        rt._accepted_archive_running = {('task', 'task:result')}
        rt._accepted_archive_running.update(
            ('other-' + str(index).zfill(3), 'other-' + str(index).zfill(3) + ':result')
            for index in range(64))
        rt.tick()
        self.assertEqual(rt.work()['archiveIntent']['id'], 'task:result')
        rt.tick()
        self.assertEqual(rt.work()['archiveIntent']['id'], 'task:result:scheduler-v2')
        self.assertEqual(rt.calls, [])
        self.assertEqual(len(rt._accepted_archive_running), 65)


class AcceptedArchiveRuntimeRestart(unittest.TestCase):
    def test_startup_commits_before_pending_archive_resumes_and_preserves_due(self):
        spec = importlib.util.spec_from_file_location(
            'accepted_archive_runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        calls = []
        archive_called = threading.Event()

        class ControlledRuntime(fixture.Runtime):
            def _archive_accepted_owner(self, actor, work, epoch):
                calls.append((work['id'], time.time()))
                archive_called.set()
                return {'status': 'kept', 'retryable': True, 'reason': 'fixture owner busy'}

        with tempfile.TemporaryDirectory(prefix='accepted-archive-real-restart-') as directory:
            from unittest.mock import patch
            with patch.object(subprocess, 'Popen', side_effect=AssertionError('No native process may start')):
                runtime = ControlledRuntime(Path(directory), fixture.FakeServer)
                try:
                    lead = runtime.create({'name': 'Archive restart lead', 'cwd': directory, 'prompt': ''}, draft=True)
                    due = time.time() + 60
                    with runtime.lock, runtime.db() as db:
                        runtime.put(db, 'work', {
                            'id': 'restart-task', 'rootId': lead['id'], 'owner': lead['id'],
                            'status': 'accepted', 'decisions': [{'resultId': 'result'}],
                            'archiveIntent': {'id': 'restart-task:result', 'status': 'pending',
                                              'attempts': 7, 'created': time.time(), 'nextAttemptAt': due}})
                finally:
                    runtime.close()
                before = time.monotonic()
                runtime = ControlledRuntime(Path(directory), fixture.FakeServer)
                try:
                    self.assertLess(time.monotonic() - before, 2)
                    runtime.accepted_archive_tick()
                    with runtime.read_db() as db:
                        work = json.loads(db.execute('SELECT record FROM runtime_work WHERE id=?',
                                                     ('restart-task',)).fetchone()[0])
                    self.assertEqual(calls, [])
                    self.assertEqual(work['archiveIntent']['id'], 'restart-task:result')
                    self.assertEqual(work['archiveIntent']['attempts'], 7)
                    self.assertEqual(work['archiveIntent']['nextAttemptAt'], due)
                    with runtime.lock, runtime.db() as db:
                        work['archiveIntent']['nextAttemptAt'] = 0
                        runtime.put(db, 'work', work)
                    runtime.changed.set()
                    self.assertTrue(archive_called.wait(30), 'archive callback did not run')
                    self.assertEqual(len(calls), 1)
                    self.assertEqual(calls[0][0], 'restart-task')
                    self.assertEqual(runtime.servers, {})
                finally:
                    runtime.close()


if __name__ == '__main__':
    unittest.main()

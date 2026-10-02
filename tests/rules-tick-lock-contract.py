#!/usr/bin/env python3
"""File watch observations do not hold the runtime lock or SQLite writer."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import importlib.util
import json
from pathlib import Path
import sqlite3
import threading
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'workspace_fixture', Path(__file__).with_name('workspace-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class RulesTickLockContract(unittest.TestCase):
    lead = fixture.WorkspaceContract.lead
    agent_update = fixture.WorkspaceContract.agent_update
    events = fixture.WorkspaceContract.events

    def setUp(self):
        fixture.WorkspaceContract.setUp(self)
        self.runtime.schedule_fast_dispatch = lambda *args, **kwargs: None
        self.threads = []
        self.releases = []

    def tearDown(self):
        for release in self.releases:
            release.set()
        for thread in self.threads:
            thread.join(3)
        fixture.WorkspaceContract.tearDown(self)

    def file_rule(self, lead, name, **options):
        path = (self.project / name).resolve()
        path.write_text('initial')
        rule = self.runtime.rules({'agent': lead['id'], 'name': name, 'kind': 'file',
                                   'path': name, 'stallTimeoutSeconds': 0, **options})
        self.update_rule(rule, nextAt=time.time() - 1)
        return rule, path

    def record(self, rule):
        with self.runtime.read_db() as db:
            row = db.execute('SELECT record FROM runtime_rules WHERE id=?', (rule['id'],)).fetchone()
            return json.loads(row[0]) if row else None

    def update_rule(self, rule, **changes):
        with self.runtime.lock, self.runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_rules WHERE id=?', (rule['id'],)).fetchone()
            current = json.loads(row[0])
            current.update(changes)
            self.runtime.put(db, 'rules', current)

    def worker(self, action, name='rule-tick'):
        done, errors = threading.Event(), []
        def run():
            try:
                action()
            except BaseException as error:
                errors.append(error)
            finally:
                done.set()
        thread = threading.Thread(target=run, name=name, daemon=True)
        self.threads.append(thread)
        thread.start()
        return done, errors

    def finished(self, worker, timeout=2):
        done, errors = worker
        self.assertTrue(done.wait(timeout), 'The fixture operation did not complete')
        self.assertEqual(errors, [])

    @contextmanager
    def gated_stat(self, path, count=1):
        entered, release = threading.Event(), threading.Event()
        self.releases.append(release)
        original, seen, owned = Path.stat, [], []
        guard = threading.Lock()
        def stat(current, *args, **kwargs):
            if current == path and threading.current_thread().name == 'rule-tick':
                with guard:
                    seen.append(current)
                    owned.append(self.runtime.lock._is_owned())
                    if len(seen) == count:
                        entered.set()
                if not release.wait(3):
                    raise RuntimeError('The fixture file stat timed out')
            return original(current, *args, **kwargs)
        with patch.object(Path, 'stat', stat):
            try:
                yield entered, release, owned
            finally:
                release.set()

    def test_slow_file_stat_does_not_block_another_send(self):
        owner = self.lead()
        self.file_rule(owner, 'first.txt')
        rule, path = self.file_rule(owner, 'second.txt')
        other = self.lead('Other chat')
        with self.gated_stat(path) as (entered, release, owned):
            tick = self.worker(self.runtime.rules_tick)
            self.assertTrue(entered.wait(2))
            sent = self.worker(lambda: self.runtime.send(other['id'], 'Continue', 'gated-stat-send'), 'send')
            try:
                self.finished(sent, .5)
                self.assertEqual(owned, [False])
                self.assertEqual(self.runtime.delivery_receipt('gated-stat-send')['status'], 'pending')
            finally:
                release.set()
            self.finished(tick)
        self.assertEqual(self.record(rule)['checks'], 0)
        self.assertIsNone(self.runtime.server)

    def test_second_file_stat_does_not_hold_the_main_writer(self):
        owner = self.lead()
        self.file_rule(owner, 'first.txt')
        _, path = self.file_rule(owner, 'second.txt')
        def writer():
            db = sqlite3.connect(self.runtime.db_path, timeout=.2)
            try:
                db.execute('BEGIN IMMEDIATE')
                db.rollback()
            finally:
                db.close()
        with self.gated_stat(path) as (entered, release, _):
            tick = self.worker(self.runtime.rules_tick)
            self.assertTrue(entered.wait(2))
            try:
                self.finished(self.worker(writer, 'writer'), .5)
            finally:
                release.set()
            self.finished(tick)

    def test_stop_during_stat_cannot_reserve_a_check(self):
        owner = self.lead()
        rule, path = self.file_rule(owner, 'watched.txt', command='fixture-rule-command')
        path.write_text('changed content')
        with self.gated_stat(path) as (entered, release, _):
            tick = self.worker(self.runtime.rules_tick)
            self.assertTrue(entered.wait(2))
            self.runtime.stop(owner['id'])
            release.set()
            self.finished(tick)
        self.assertEqual(self.record(rule)['checks'], 0)
        self.assertEqual(self.events(owner, 'rule'), [])
        self.assertIsNone(self.runtime.server)

    def test_pause_delete_and_edit_discard_the_old_observation(self):
        for action in ('pause', 'delete', 'save'):
            with self.subTest(action=action):
                owner = self.lead(action)
                rule, path = self.file_rule(owner, action + '.txt')
                path.write_text('changed content')
                with self.gated_stat(path) as (entered, release, _):
                    tick = self.worker(self.runtime.rules_tick)
                    self.assertTrue(entered.wait(2))
                    data = {'agent': owner['id'], 'id': rule['id'], 'action': action}
                    if action == 'save':
                        data.update(name='Edited watch', kind='file', path=path.name,
                                    stallTimeoutSeconds=0, intervalSeconds=90)
                    changed = self.runtime.rules(data)
                    release.set()
                    self.finished(tick)
                current = self.record(rule)
                if action == 'delete':
                    self.assertIsNone(current)
                else:
                    self.assertEqual(current, changed)
                    self.assertEqual(current['checks'], 0)
                self.assertEqual(self.events(owner, 'rule'), [])

    def test_owner_connection_or_permissions_change_discards_the_observation(self):
        for changes in ({'threadId': 'replacement-thread'}, {'accountKey': 'replacement-account'},
                        {'approvalPolicy': 'on-request'}, {'epoch': 99}, {'autoWake': False}):
            with self.subTest(changes=changes):
                owner = self.lead(str(changes))
                rule, path = self.file_rule(owner, str(len(self.threads)) + '.txt')
                path.write_text('changed content')
                with self.gated_stat(path) as (entered, release, _):
                    tick = self.worker(self.runtime.rules_tick)
                    self.assertTrue(entered.wait(2))
                    self.agent_update(owner, **changes)
                    release.set()
                    self.finished(tick)
                self.assertEqual(self.record(rule)['checks'], 0)
                self.assertEqual(self.events(owner, 'rule'), [])

    def test_concurrent_observations_reserve_one_check(self):
        owner = self.lead()
        rule, path = self.file_rule(owner, 'watched.txt')
        path.write_text('changed content')
        with self.gated_stat(path, count=2) as (entered, release, owned):
            first = self.worker(self.runtime.rules_tick)
            second = self.worker(self.runtime.rules_tick)
            self.assertTrue(entered.wait(2))
            release.set()
            self.finished(first)
            self.finished(second)
        fixture.eventually(lambda: self.record(rule)['wakes'] == 1)
        self.assertEqual(owned, [False, False])
        self.assertEqual(self.record(rule)['checks'], 1)
        self.assertEqual(self.record(rule)['fileGeneration'], 1)
        self.assertEqual(len(self.events(owner, 'rule')), 1)
        self.assertIsNone(self.runtime.server)

    def test_resume_during_stat_does_not_emit_the_old_stall(self):
        owner = self.lead()
        rule, path = self.file_rule(owner, 'quiet.txt', stallTimeoutSeconds=30)
        self.update_rule(rule, fileActivityAt=time.time() - 31)
        with self.gated_stat(path) as (entered, release, _):
            tick = self.worker(self.runtime.rules_tick)
            self.assertTrue(entered.wait(2))
            resumed = self.runtime.rules({'agent': owner['id'], 'id': rule['id'], 'action': 'resume'})
            release.set()
            self.finished(tick)
        self.assertEqual(self.record(rule), resumed)
        self.assertEqual(self.events(owner, 'rule_stall'), [])

    def test_exact_native_recovery_keeps_the_watch_without_stat(self):
        owner = self.lead()
        rule, _ = self.file_rule(owner, 'recovering.txt')
        recovery = {'stage': 'pending', 'autoWake': True, 'epoch': owner['epoch'],
                    'accountKey': owner.get('accountKey'), 'threadId': owner.get('threadId')}
        self.agent_update(owner, restartRecovery=recovery, autoWake=False)
        with patch.object(self.runtime, 'file_fingerprint', side_effect=AssertionError('Unexpected file stat')):
            self.runtime.rules_tick()
        self.assertEqual(self.record(rule)['status'], 'active')
        self.assertEqual(self.record(rule)['checks'], 0)
        self.assertEqual(self.events(owner, 'rule'), [])

    def test_a_caller_cannot_hold_the_runtime_lock_around_the_tick(self):
        owner = self.lead()
        self.file_rule(owner, 'watched.txt')
        with self.runtime.lock, patch.object(self.runtime, 'file_fingerprint') as fingerprint:
            with self.assertRaisesRegex(RuntimeError, 'outside the runtime lock'):
                self.runtime.rules_tick()
            fingerprint.assert_not_called()


if __name__ == '__main__':
    unittest.main()

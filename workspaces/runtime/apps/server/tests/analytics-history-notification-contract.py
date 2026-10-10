#!/usr/bin/env python3
"""Native file hints reduce idle imports without replacing checkpoint proof."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import os
import struct
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location('notification_history_fixture',
    Path(__file__).with_name('analytics-history-lock-order-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_analytics_history as history
from codex_history_notifications import HistoryFileNotifications


class Hints:
    enabled = True

    def __init__(self, limit=128):
        self.paths, self.pending = {}, set()
        self.limit = limit
        self.closed = False

    def watch(self, actor, path):
        if self.paths.get(actor) == path or len(self.paths) >= self.limit:
            return False
        self.paths[actor] = path
        return True

    def contains(self, actor):
        return actor in self.paths

    def retain(self, actors):
        self.paths = {actor: path for actor, path in self.paths.items() if actor in actors}

    def collect(self, timeout=0):
        pending, self.pending = self.pending, set()
        return pending

    def close(self):
        self.closed = True
        self.paths.clear()


class NotificationImports(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture.HistoryLockOrder()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.runtime = self.fixture.runtime
        self.actor = self.fixture.agent['id']
        with self.runtime.db() as db:
            agent = self.runtime.agent(self.actor, db)
            agent.update(status='running', inFlight=True)
            self.runtime.put(db, 'agents', agent)
        self.clock = [100.0]
        for clock_patch in (patch.object(history.time, 'monotonic', side_effect=lambda: self.clock[0]),
                            patch.object(history.time, 'thread_time', return_value=0)):
            clock_patch.start()
            self.addCleanup(clock_patch.stop)
        for _ in range(4):
            self.runtime.analytics_history_step()
        self.hints = Hints()
        self.runtime._analytics_history_notifications = self.hints
        self.runtime.analytics_history_thread = threading.current_thread()
        self.addCleanup(lambda: delattr(self.runtime, 'analytics_history_thread'))
        self.runtime._analytics_history_cursor = 0
        # Registration follows a prior read, then one exact read closes that race.
        self.runtime.analytics_history_step()
        self.runtime.analytics_history_step()
        self.assertTrue(self.hints.contains(self.actor))

    def state(self):
        return self.fixture.usage()[3]

    def append_usage(self, response='notification-response', amount=35):
        record = {'type': 'token_usage_record', 'timestamp': '2026-09-06T18:00:01Z', 'payload': {
            'thread_id': self.fixture.thread, 'turn_id': 'notification-turn', 'response_id': response,
            'usage': {'total_tokens': amount}, 'thread_token_usage': {'total_tokens': amount}}}
        with self.fixture.path.open('a') as stream:
            stream.write(json.dumps(record) + '\n')

    def test_quiet_active_file_has_no_import_open_stat_or_actor_lookup(self):
        original = self.state()
        self.clock[0] += history.ACTIVE_RECONCILE_SECONDS
        with patch.object(Path, 'open', side_effect=AssertionError('Quiet rollout opened')), \
                patch.object(Path, 'stat', side_effect=AssertionError('Quiet rollout checked')), \
                patch.object(self.runtime, 'agent', side_effect=AssertionError('Quiet actor loaded')):
            for _ in range(4):
                self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(self.state(), original)

    def test_coalesced_hints_import_exact_usage_receipt_once(self):
        self.append_usage()
        self.hints.pending.update([self.actor] * 30)
        self.assertTrue(self.runtime.analytics_history_step())
        for _ in range(4):
            self.hints.pending.add(self.actor)
            self.runtime.analytics_history_step()
        budget, receipts, usage, state = self.fixture.usage()
        self.assertEqual(budget['spent'], 135)
        self.assertEqual(receipts, [('response', 0), ('response', 35), ('response', 100)])
        self.assertEqual([row['responseId'] for row in usage].count('notification-response'), 1)
        self.assertEqual(state['offset'], self.fixture.path.stat().st_size)

    def test_lost_hint_gets_exact_periodic_reconciliation(self):
        self.append_usage()
        self.clock[0] += history.NOTIFIED_RECONCILE_SECONDS - 1
        self.assertFalse(self.runtime.analytics_history_step())
        self.clock[0] += 1
        self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(self.fixture.usage()[0]['spent'], 135)
        self.assertEqual(self.state()['offset'], self.fixture.path.stat().st_size)

    def test_partial_line_waits_for_hint_and_reads_the_completed_record(self):
        with self.fixture.path.open('ab') as stream:
            stream.write(b'{"type":"event_msg","payload":')
        self.hints.pending.add(self.actor)
        self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(self.state()['status'], 'partialLine')
        self.clock[0] += 5
        with patch.object(Path, 'open', side_effect=AssertionError('Partial line busy poll')):
            self.assertFalse(self.runtime.analytics_history_step())
        with self.fixture.path.open('ab') as stream:
            stream.write(b'{"type":"task_complete","turn_id":"notification-turn"}}\n')
        self.hints.pending.add(self.actor)
        self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(self.state()['status'], 'current')

    def test_replacement_and_truncation_keep_checkpoint_and_receipts(self):
        before = self.fixture.usage()
        previous = self.fixture.path.read_bytes()
        for mode in ('replace', 'truncate'):
            with self.subTest(mode=mode):
                if mode == 'replace':
                    temporary = self.fixture.path.with_suffix('.tmp')
                    temporary.write_bytes(previous)
                    temporary.replace(self.fixture.path)
                else:
                    self.fixture.path.write_bytes(b'')
                self.hints.pending.add(self.actor)
                self.assertFalse(self.runtime.analytics_history_step())
                after = self.fixture.usage()
                self.assertEqual(after[:3], before[:3])
                self.assertEqual(after[3]['offset'], before[3]['offset'])
                self.assertEqual(after[3]['status'], 'identityChanged')

    def test_account_and_thread_change_ignore_old_pending_hint(self):
        self.append_usage()
        self.hints.pending.add(self.actor)
        with self.runtime.db() as db:
            actor = self.runtime.agent(self.actor, db)
            actor.update(accountKey='fresh-profile', threadId=str(uuid.uuid4()))
            self.runtime.put(db, 'agents', actor)
        self.assertFalse(self.runtime.analytics_history_step())
        self.assertFalse(self.hints.contains(self.actor))
        self.assertNotIn(self.actor, self.runtime._analytics_history_dirty)
        self.assertEqual(self.fixture.usage()[0]['spent'], 100)
        with self.runtime.analytics_db() as db:
            states = [json.loads(row[0]) for row in db.execute('SELECT record FROM analytics_history')]
        self.assertTrue(any(state['threadId'] == actor['threadId'] and state['status'] == 'missing'
                            for state in states))

    def test_watch_failure_keeps_active_five_second_fallback(self):
        self.hints.paths.clear()
        self.hints.limit = 0
        self.clock[0] += history.NOTIFIED_RECONCILE_SECONDS
        self.runtime.analytics_history_step()
        self.append_usage()
        self.clock[0] += history.ACTIVE_RECONCILE_SECONDS - 1
        self.assertFalse(self.runtime.analytics_history_step())
        self.clock[0] += 1
        self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(self.fixture.usage()[0]['spent'], 135)

    def test_legacy_budget_backlog_keeps_importing_without_waiting_for_a_file_hint(self):
        with self.runtime.analytics_db() as db:
            previous_end = db.execute('SELECT MAX(seq) FROM analytics_usage').fetchone()[0]
            for index in range(130):
                record = {'threadId': self.fixture.thread, 'turnId': 'legacy-turn', 'at': 2,
                          'responseId': 'legacy-' + str(index),
                          'rawTokenUsageRecord': {'response_id': 'legacy-' + str(index)},
                          'requestUsage': {'totalTokens': 1}, 'last': {'totalTokens': 1}}
                db.execute('INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
                    ('legacy-' + str(index), self.actor, self.actor, self.fixture.thread,
                     'legacy-turn', 2, json.dumps(record)))
            end = db.execute('SELECT MAX(seq) FROM analytics_usage').fetchone()[0]
            db.execute('UPDATE analytics_meta SET value=? WHERE key=?',
                       (json.dumps({'cursor': previous_end, 'end': end}), 'budgetUsageMigrationV1:' + self.actor))
        self.hints.pending.add(self.actor)
        for _ in range(4):
            self.runtime.analytics_history_step()
        self.assertEqual(self.fixture.usage()[0]['spent'], 230)
        with self.runtime.analytics_db() as db:
            state = json.loads(db.execute('SELECT value FROM analytics_meta WHERE key=?',
                ('budgetUsageMigrationV1:' + self.actor,)).fetchone()[0])
        self.assertEqual(state['cursor'], state['end'])

    def test_unsupported_native_source_keeps_five_second_import_fallback(self):
        with patch.object(sys, 'platform', 'unsupported-fixture'):
            notifications = HistoryFileNotifications()
        self.addCleanup(notifications.close)
        self.runtime._analytics_history_notifications = notifications
        self.runtime._analytics_history_polls[self.actor]['due'] = 0
        self.runtime.analytics_history_step()
        self.append_usage()
        self.clock[0] += history.ACTIVE_RECONCILE_SECONDS - 1
        self.assertFalse(self.runtime.analytics_history_step())
        self.clock[0] += 1
        self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(self.fixture.usage()[0]['spent'], 135)

    def test_missing_file_can_appear_after_discovery_cache_reconciliation(self):
        with self.runtime.db() as db:
            actor = self.runtime.agent(self.actor, db)
            actor.update(threadId=str(uuid.uuid4()))
            self.runtime.put(db, 'agents', actor)
        self.runtime.analytics_history_step()
        self.assertFalse(self.hints.contains(self.actor))
        path = self.fixture.path.with_name('rollout-' + actor['threadId'] + '.jsonl')
        path.write_text(json.dumps({'type': 'session_meta', 'payload': {'id': actor['threadId']}}) + '\n')
        self.clock[0] += 31
        self.assertTrue(self.runtime.analytics_history_step())
        self.runtime.analytics_history_step()
        self.assertEqual(self.hints.paths[self.actor], str(path))
        with self.runtime.analytics_db() as db:
            row = db.execute('SELECT record FROM analytics_history WHERE id=?',
                (actor['id'] + ':' + actor.get('accountKey', 'default') + ':' + actor['threadId'],)).fetchone()
        state = json.loads(row[0])
        self.assertEqual(state['status'], 'current')
        self.assertEqual(state['offset'], path.stat().st_size)

    def test_inactive_roster_never_allocates_archived_file_watches(self):
        with self.runtime.db() as db:
            actor = self.runtime.agent(self.actor, db)
            actor.update(status='completed', inFlight=False, deletedAt=1)
            self.runtime.put(db, 'agents', actor)
        self.runtime.analytics_history_step()
        self.assertEqual(self.hints.paths, {})
        self.assertEqual(self.runtime._analytics_history_polls[self.actor]['due'],
                         self.clock[0] + history.NOTIFIED_RECONCILE_SECONDS)


class WorkerNotificationLifetime(unittest.TestCase):
    def test_real_worker_wakes_after_append_and_closes_its_owned_watches(self):
        if sys.platform != 'darwin' and not sys.platform.startswith('linux'):
            self.skipTest('Native exact-file notifications are unavailable on this platform')
        case = fixture.HistoryLockOrder()
        case.setUp()
        self.addCleanup(case.doCleanups)
        runtime = case.runtime
        with runtime.db() as db:
            actor = runtime.agent(case.agent['id'], db)
            actor.update(status='running', inFlight=True)
            runtime.put(db, 'agents', actor)
        instances = []
        def create():
            observer = HistoryFileNotifications()
            instances.append(observer)
            return observer
        def eventually(predicate):
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                if predicate():
                    return
                time.sleep(.01)
            self.fail('The native history worker did not reach the expected state')
        with patch('codex_history_notifications.HistoryFileNotifications', side_effect=create):
            self.assertTrue(runtime.analytics_history_start())
            eventually(lambda: instances and instances[0].contains(actor['id'])
                       and runtime._analytics_history_polls.get(actor['id'], {}).get('due', 0) > time.monotonic())
            record = {'type': 'token_usage_record', 'payload': {
                'thread_id': case.thread, 'turn_id': 'worker-turn', 'response_id': 'worker-notification',
                'usage': {'total_tokens': 35}, 'thread_token_usage': {'total_tokens': 135}}}
            with case.path.open('a') as stream:
                stream.write(json.dumps(record) + '\n')
            eventually(lambda: case.usage()[0]['spent'] == 135)
        worker = runtime.analytics_history_thread
        runtime.close()
        self.assertFalse(worker.is_alive())
        self.assertTrue(instances[0]._closed)
        self.assertEqual(instances[0]._watches, {})
        self.assertFalse(hasattr(runtime, '_analytics_history_notifications'))


class NativeFileHints(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='studio-history-hints-')
        self.addCleanup(self.temporary.cleanup)
        self.path = Path(self.temporary.name) / 'rollout.jsonl'
        self.path.write_text('initial\n')
        self.notifications = HistoryFileNotifications(limit=2)
        self.addCleanup(self.notifications.close)

    def test_real_native_append_replace_and_resource_limit(self):
        if not self.notifications.enabled:
            self.skipTest('Native exact-file notifications are unavailable on this platform')
        self.assertTrue(self.notifications.watch('one', str(self.path)))
        second = self.path.with_name('second.jsonl')
        second.write_text('second\n')
        self.assertTrue(self.notifications.watch('two', str(second)))
        self.assertFalse(self.notifications.watch('three', str(second)))
        with self.path.open('a') as stream:
            stream.write('append\n')
        self.assertIn('one', self.notifications.collect(1))
        replacement = self.path.with_suffix('.tmp')
        replacement.write_text('replacement\n')
        replacement.replace(self.path)
        self.assertIn('one', self.notifications.collect(1))
        self.assertFalse(self.notifications.contains('one'))
        self.assertTrue(self.notifications.watch('one', str(self.path)))
        self.notifications.retain({'one'})
        self.assertFalse(self.notifications.contains('two'))
        self.notifications.close()
        self.notifications.close()
        self.assertFalse(self.notifications.enabled)
        self.assertEqual(self.notifications._watches, {})
        self.assertFalse(self.notifications.watch('closed', str(self.path)))

    def test_linux_overflow_and_ignored_events_make_exact_owners_due(self):
        from types import SimpleNamespace
        with patch.object(sys, 'platform', 'unsupported-fixture'):
            notifications = HistoryFileNotifications()
        read_fd, write_fd = os.pipe()
        notifications._fd = read_fd
        removed = []
        notifications._libc = SimpleNamespace(inotify_rm_watch=lambda fd, watch: removed.append(watch))
        notifications._watches = {'one': ('one.jsonl', 7), 'two': ('two.jsonl', 8)}
        notifications._owners = {7: {'one'}, 8: {'two'}}
        try:
            os.write(write_fd, struct.pack('iIII', -1, 0x4000, 0, 0)
                     + struct.pack('iIII', 7, 0x8000, 0, 0))
            self.assertEqual(notifications.collect(), {'one', 'two'})
            self.assertFalse(notifications.contains('one'))
            self.assertTrue(notifications.contains('two'))
            self.assertEqual(removed, [7])
        finally:
            notifications.close()
            os.close(write_fd)
        self.assertEqual(removed, [7, 8])
        with self.assertRaises(OSError):
            os.fstat(read_fd)

    def test_unsupported_platform_uses_no_native_resources(self):
        with patch.object(sys, 'platform', 'unsupported-fixture'):
            notifications = HistoryFileNotifications()
        try:
            self.assertFalse(notifications.enabled)
            self.assertFalse(notifications.watch('actor', str(self.path)))
            self.assertEqual(notifications.collect(), set())
        finally:
            notifications.close()


if __name__ == '__main__':
    unittest.main()

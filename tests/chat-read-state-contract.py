#!/usr/bin/env python3
"""Read state uses SQLite, exact results, and revision conflict checks."""
from concurrent.futures import ThreadPoolExecutor
import importlib.util
from pathlib import Path
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ChatReadStateContract(fixture.RuntimeContract):
    def completed(self):
        agent = self.runtime.new_lead({})
        return self.change(agent, threadId='thread-1', lastCompletedTurn='turn-1',
                           lastCompletedTurnStatus='completed', status='completed')

    def change(self, agent, **fields):
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(agent['id'], db)
            current.update(fields)
            self.runtime.put(db, 'agents', current)
            return current

    def request(self, agent, read=True, revision=0, **fields):
        return {'id': agent['id'], 'read_state': {
            'thread_id': agent['threadId'], 'turn_id': agent['lastCompletedTurn'],
            'read': read, 'expected_revision': revision, **fields}}

    def apply(self, agent, **values):
        return self.runtime.chat_organization(agent['id'], self.request(agent, **values))

    def test_first_read_and_explicit_unread_change_only_read_state(self):
        agent = self.completed()
        first = self.apply(agent)
        self.assertEqual(first, {**agent, 'readState': {
            'threadId': 'thread-1', 'turnId': 'turn-1', 'read': True, 'revision': 1}})
        unread = self.apply(agent, read=False, revision=1)
        self.assertEqual(unread['readState'], {**first['readState'], 'read': False, 'revision': 2})
        self.assertFalse(self.runtime.servers, 'Read metadata must not start a native server')

    def test_exact_replays_do_not_write_or_increment_revision(self):
        agent = self.completed()
        first = self.apply(agent)
        with patch.object(self.runtime, 'put', wraps=self.runtime.put) as put:
            self.assertEqual(self.apply(agent, revision=0), first)
            self.assertEqual(self.apply(agent, revision=1), first)
            put.assert_not_called()
        unread = self.apply(agent, read=False, revision=1)
        with patch.object(self.runtime, 'put', wraps=self.runtime.put) as put:
            self.assertEqual(self.apply(agent, read=False, revision=1), unread)
            put.assert_not_called()

    def test_new_result_rejects_old_acknowledgement_before_replay(self):
        agent = self.completed()
        self.apply(agent)
        new = self.change(agent, lastCompletedTurn='turn-2')
        before = self.runtime.agent(agent['id'])
        with self.assertRaisesRegex(ValueError, 'completed result changed'):
            self.apply(agent)
        self.assertEqual(self.runtime.agent(agent['id']), before)
        acknowledged = self.apply(new, revision=1)
        self.assertEqual(acknowledged['readState']['turnId'], 'turn-2')
        self.assertEqual(acknowledged['readState']['revision'], 2)

    def test_old_thread_or_unsuccessful_result_cannot_be_read(self):
        for field, value in [('threadId', 'new-thread'),
                             ('lastCompletedTurnStatus', 'failed'),
                             ('lastCompletedTurnStatus', 'interrupted'),
                             ('lastCompletedTurnStatus', None)]:
            with self.subTest(field=field, value=value):
                agent = self.completed()
                before = self.change(agent, **{field: value})
                with self.assertRaisesRegex(ValueError, 'completed result changed'):
                    self.apply(agent)
                self.assertEqual(self.runtime.agent(agent['id']), before)

    def test_explicit_unread_wins_over_delayed_read_and_old_replays(self):
        agent = self.completed()
        self.apply(agent)
        unread = self.apply(agent, read=False, revision=1)
        for revision in (0, 1, 3):
            with self.subTest(revision=revision), self.assertRaisesRegex(ValueError, 'read state changed'):
                self.apply(agent, revision=revision)
        self.assertEqual(self.runtime.agent(agent['id']), unread)
        self.apply(agent, read=True, revision=2)
        with self.assertRaisesRegex(ValueError, 'read state changed'):
            self.apply(agent, read=True, revision=0)

    def test_two_clients_cannot_overwrite_the_same_revision(self):
        agent = self.completed()
        gate = threading.Barrier(2)
        def submit(read):
            gate.wait(5)
            try:
                return self.apply(agent, read=read)
            except ValueError as error:
                return error
        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(submit, [True, False]))
        accepted = [result for result in results if isinstance(result, dict)]
        rejected = [result for result in results if isinstance(result, ValueError)]
        self.assertEqual(len(accepted), 1)
        self.assertEqual(len(rejected), 1)
        self.assertEqual(self.runtime.agent(agent['id']), accepted[0])
        self.assertEqual(accepted[0]['readState']['revision'], 1)

    def test_restart_preserves_explicit_unread_and_revision(self):
        agent = self.completed()
        self.apply(agent)
        expected = self.apply(agent, read=False, revision=1)['readState']
        self.runtime.close()
        self.runtime = fixture.Runtime(self.root, fixture.FakeServer)
        self.assertEqual(self.runtime.agent(agent['id'])['readState'], expected)
        self.assertEqual(self.apply(agent, read=False, revision=1)['readState'], expected)
        with self.assertRaisesRegex(ValueError, 'read state changed'):
            self.apply(agent)

    def test_read_is_legal_during_work_and_next_completion_remains_distinct(self):
        agent = self.lead()
        self.complete(agent)
        fixture.eventually(lambda: self.runtime.agent(agent['id']).get('lastCompletedTurn') == agent['turnId'])
        completed = self.runtime.agent(agent['id'])
        self.runtime.send(agent['id'], 'Continue the work')
        fixture.eventually(lambda: self.runtime.agent(agent['id'])['status'] == 'running'
                           and self.runtime.agent(agent['id']).get('turnId'))
        active = self.runtime.agent(agent['id'])
        native_calls = list(self.runtime.server.calls)
        result = self.apply(completed)
        for field in ('turnId', 'status', 'epoch', 'inFlight', 'autoWake'):
            self.assertEqual(result[field], active[field])
        self.assertEqual(self.runtime.server.calls, native_calls)
        self.complete(active)
        fixture.eventually(lambda: self.runtime.agent(agent['id']).get('lastCompletedTurn') == active['turnId'])
        new = self.runtime.agent(agent['id'])
        self.assertNotEqual(new['lastCompletedTurn'], new['readState']['turnId'])
        with self.assertRaisesRegex(ValueError, 'completed result changed'):
            self.apply(completed, revision=1)

    def test_malformed_values_and_mixed_mutations_do_not_write(self):
        agent = self.completed()
        invalid = [None, [], {}, {**self.request(agent)['read_state'], 'extra': 1}]
        for field, values in [('thread_id', ['', ' ', None, 1, []]),
                              ('turn_id', ['', '\n', None, 1, []]),
                              ('read', [None, 0, 1, 'true']),
                              ('expected_revision', [None, -1, True, 0.0, '0'])]:
            invalid.extend({**self.request(agent)['read_state'], field: value} for value in values)
        requests = [{'id': agent['id'], 'read_state': value} for value in invalid]
        requests.extend({**self.request(agent), field: value} for field, value in
                        [('pinned', True), ('archived', True), ('project', 'Other'),
                         ('project_folder', None), ('id', 'another-agent')])
        with patch.object(self.runtime, 'put', wraps=self.runtime.put) as put:
            for request in requests:
                with self.subTest(request=request), self.assertRaises(ValueError):
                    self.runtime.chat_organization(agent['id'], request)
            put.assert_not_called()
        self.assertEqual(self.runtime.agent(agent['id']), agent)

    def test_deleted_chat_rejected_and_pin_archive_behavior_preserved(self):
        agent = self.completed()
        deleted = self.change(agent, deletedAt=1)
        with self.assertRaisesRegex(ValueError, 'deleted'):
            self.apply(agent)
        self.assertEqual(self.runtime.agent(agent['id']), deleted)
        agent = self.completed()
        read = self.apply(agent)['readState']
        organized = self.runtime.chat_organization(agent['id'], {'pinned': True, 'archived': True})
        self.assertTrue(organized['pinned'])
        self.assertTrue(organized['archived'])
        self.assertEqual(organized['readState'], read)
        self.change(agent, archived=False, inFlight=True)
        with self.assertRaisesRegex(ValueError, 'Stop active work'):
            self.runtime.chat_organization(agent['id'], {'archived': True})


if __name__ == '__main__':
    suite = unittest.TestSuite(ChatReadStateContract(name)
        for name in ChatReadStateContract.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())

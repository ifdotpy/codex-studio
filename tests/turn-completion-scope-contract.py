#!/usr/bin/env python3
"""Turn completion uses the exact item scope and preserves legacy labels."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import importlib.util
import json
from pathlib import Path
import tempfile
import time
import unittest

spec = importlib.util.spec_from_file_location('turn_scope_fixture',
    Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class TurnCompletionScope(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.FakeServer)
        self.runtime.closed = True
        self.runtime.changed.set()
        self.runtime.scheduler.join(timeout=5)
        self.assertFalse(self.runtime.scheduler.is_alive())
        self.runtime.closed = False
        self.actor = self.runtime.create({'name': 'Turn scope', 'cwd': self.temp.name,
                                          'prompt': 'Fixture'}, defer=True)
        self.update(threadId='scope-thread', turnId='scope-turn', status='running',
                    autoWake=False, inFlight=True)
        self.created = time.time()

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def update(self, **fields):
        with self.runtime.lock, self.runtime.db() as db:
            self.actor = self.runtime.agent(self.actor['id'], db)
            self.actor.update(fields)
            self.runtime.put(db, 'agents', self.actor)
        return self.actor

    def seed(self):
        records = {
            'current': {'turnId': 'scope-turn'},
            'old-copy': {'turnId': 'scope-turn'},
            'other-turn': {'turnId': 'other-turn', 'turnStatus': 'failed'},
            'legacy-null': {'turnId': None},
            'legacy-missing': {},
            'legacy-number': {'turnId': 7},
        }
        with self.runtime.lock, self.runtime.db() as db:
            for key, fields in records.items():
                record = {'id': self.actor['id'] + ':' + key, 'role': 'assistant',
                          'title': 'Assistant', 'text': key, **fields}
                db.execute('INSERT INTO runtime_items VALUES (?,?,?,?)',
                    (record['id'], self.actor['id'], json.dumps(record),
                     self.created - 3600 if key == 'old-copy' else self.created))
            foreign = {'id': 'foreign:current', 'role': 'assistant',
                       'turnId': 'scope-turn', 'text': 'Foreign'}
            db.execute('INSERT INTO runtime_items VALUES (?,?,?,?)',
                       (foreign['id'], 'foreign', json.dumps(foreign), self.created))
        return self.items()

    def items(self):
        with self.runtime.db() as db:
            return {row[0]: row[1] for row in db.execute('SELECT id,record FROM runtime_items')}

    def complete(self, turn='scope-turn', status='completed'):
        queries = []
        original = self.runtime.db

        @contextmanager
        def traced(**options):
            with original(**options) as db:
                db.set_trace_callback(queries.append)
                yield db
        self.runtime.db = traced
        try:
            self.runtime.notification({'method': 'turn/completed', 'params': {
                'threadId': 'scope-thread', 'turn': {'id': turn, 'status': status}}})
        finally:
            self.runtime.db = original
        updates = set(query for query in queries if query.startswith('UPDATE runtime_items ')
                      and "json_set(record,'$.turnStatus'" in query)
        return updates

    def assert_labels(self, before, marked, status='completed'):
        after = self.items()
        for key, raw in before.items():
            if key in marked:
                self.assertEqual(json.loads(after[key]), {**json.loads(raw), 'turnStatus': status})
            else:
                self.assertEqual(after[key], raw, key)

    def assert_turn_index(self, updates, bounded):
        self.assertEqual(len(updates), 1)
        with self.runtime.db() as db:
            plan = ' '.join(row[3] for row in db.execute('EXPLAIN QUERY PLAN ' + next(iter(updates))))
        self.assertIn('runtime_item_turn_scope', plan)
        self.assertIn('<expr>=?', plan)
        if bounded:
            self.assertIn('created>?', plan)

    def test_unbound_attempt_marks_the_exact_turn_without_history_scan(self):
        self.update(startAttempt={'id': 'unbound', 'created': self.created,
                    'submitted': True, 'epoch': self.actor['epoch'], 'events': []})
        before = self.seed()
        updates = self.complete()
        self.assert_labels(before, {self.actor['id'] + ':current', self.actor['id'] + ':old-copy'})
        self.assert_turn_index(updates, bounded=True)

    def test_bound_attempt_preserves_the_existing_created_window(self):
        self.update(startAttempt={'id': 'bound', 'created': self.created,
                    'submitted': True, 'turnId': 'scope-turn',
                    'epoch': self.actor['epoch'], 'events': []})
        before = self.seed()
        updates = self.complete()
        self.assert_labels(before, {self.actor['id'] + ':current'})
        self.assert_turn_index(updates, bounded=True)

    def test_observed_unbound_attempt_keeps_the_full_exact_turn_fallback(self):
        self.update(startAttempt={'id': 'observed', 'created': self.created,
                    'submitted': True, 'observedTurnId': 'scope-turn',
                    'epoch': self.actor['epoch'], 'events': []})
        before = self.seed()
        updates = self.complete()
        self.assert_labels(before, {self.actor['id'] + ':current', self.actor['id'] + ':old-copy'})
        self.assert_turn_index(updates, bounded=True)

    def test_old_completion_preserves_the_current_turn_and_marks_old_copies(self):
        self.update(turnId='newer-turn')
        before_actor = self.runtime.agent(self.actor['id'])
        before = self.seed()
        updates = self.complete()
        self.assert_labels(before, {self.actor['id'] + ':current', self.actor['id'] + ':old-copy'})
        self.assertEqual(self.runtime.agent(self.actor['id']), before_actor)
        self.assert_turn_index(updates, bounded=False)

    def test_missing_turn_id_does_not_mark_legacy_null_items(self):
        before = self.seed()
        updates = self.complete(turn=None)
        self.assert_labels(before, set())
        self.assert_turn_index(updates, bounded=False)

    def test_duplicate_completion_preserves_terminal_labels_and_the_stop(self):
        self.update(epoch=2, turnEpoch=1, error='Explicit stop')
        before = self.seed()
        self.complete(status='interrupted')
        self.assert_labels(before, {self.actor['id'] + ':current', self.actor['id'] + ':old-copy'},
                           status='interrupted')
        actor = self.runtime.agent(self.actor['id'])
        self.assertEqual((actor['status'], actor['autoWake'], actor['epoch'], actor['error']),
                         ('paused', False, 2, 'Explicit stop'))
        saved = self.items()
        self.assertEqual(self.complete(status='interrupted'), set())
        self.assertEqual(self.items(), saved)

    def test_existing_state_gets_the_index_without_item_changes(self):
        before = self.seed()
        with self.runtime.lock, self.runtime.db() as db:
            db.execute('DROP INDEX runtime_item_turn_scope')
        self.runtime.close()
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.FakeServer)
        self.runtime.closed = True
        self.runtime.changed.set()
        self.runtime.scheduler.join(timeout=5)
        self.assertFalse(self.runtime.scheduler.is_alive())
        self.runtime.closed = False
        self.assertEqual(self.items(), before)
        with self.runtime.db() as db:
            plan = ' '.join(row[3] for row in db.execute(
                "EXPLAIN QUERY PLAN SELECT id FROM runtime_items WHERE agent=? "
                "AND json_extract(record,'$.turnId')=? AND created>=?",
                (self.actor['id'], 'scope-turn', 0)))
        self.assertIn('runtime_item_turn_scope', plan)
        self.assertIn('<expr>=?', plan)


if __name__ == '__main__':
    unittest.main()

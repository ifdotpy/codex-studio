#!/usr/bin/env python3
"""Exact historical tool completions settle tasks without changing the current turn."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('old_task_fixture',
    Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class ControlledRuntime(f.Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.05)
            self.changed.clear()


class OldTurnTaskCompletion(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.runtime = ControlledRuntime(self.root, f.FakeServer)
        self.agent = self.runtime.create({'name': 'Exact tool history',
            'cwd': str(self.root), 'prompt': 'Fixture'}, defer=True)
        self.runtime.connect()
        self.update(threadId='fixture-thread', turnId='old-turn', status='running',
                    autoWake=True, inFlight=True)

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def update(self, **changes):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.agent['id'], db)
            a.update(changes)
            self.runtime.put(db, 'agents', a)
        self.agent = a
        return a

    def notify(self, method, item, *, turn='old-turn', account='default', connection=None):
        self.runtime.notification({'method': method, 'params': {
            'threadId': 'fixture-thread', 'turnId': turn, 'item': copy.deepcopy(item)}},
            account, connection)

    def start(self, kind, key='old-item'):
        self.notify('item/started', {'id': key, 'type': kind, 'tool': 'fixture-tool'})
        return self.task(key)

    def task(self, key='old-item'):
        with self.runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_tasks WHERE id=?',
                             (self.agent['id'] + ':' + key,)).fetchone()
        return json.loads(row[0]) if row else None

    def transcript(self, key='old-item'):
        with self.runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_items WHERE id=?',
                             (self.agent['id'] + ':' + key,)).fetchone()
        return json.loads(row[0]) if row else None

    def advance(self):
        return self.update(turnId='new-turn', tokensUsed=123,
            activeTools=[{'id': 'new-item', 'type': 'commandExecution', 'name': 'commandExecution'}],
            activity={'phase': 'tool', 'at': 42,
                      'tools': [{'id': 'new-item', 'type': 'commandExecution', 'name': 'commandExecution'}]})

    def budget_rows(self):
        with self.runtime.db() as db:
            return [tuple(row) for row in db.execute(
                'SELECT * FROM runtime_budget_usage WHERE agent=?', (self.agent['id'],))]

    def test_exact_old_dynamic_completion_preserves_current_actor_and_replay(self):
        self.exact_completion('dynamicToolCall')

    def test_exact_old_mcp_completion_preserves_current_actor_and_replay(self):
        self.exact_completion('mcpToolCall')

    def exact_completion(self, kind):
        original = self.start(kind)
        self.update(tokenBudget=1000)
        self.runtime.notification({'method': 'thread/tokenUsage/updated', 'params': {
            'threadId': 'fixture-thread', 'turnId': 'old-turn',
            'tokenUsage': {'total': {'totalTokens': 123}}}})
        before = copy.deepcopy(self.advance())
        budget = self.budget_rows()
        self.assertTrue(budget, 'The fixture must contain an actual token charge')
        calls = list(self.runtime.server.calls)
        complete = {'id': 'old-item', 'type': kind, 'tool': 'fixture-tool',
                    'status': 'completed', 'success': True,
                    'contentItems': [{'type': 'inputText', 'text': 'Exact saved result'}]}
        self.notify('item/completed', complete)
        task = self.task()
        self.assertEqual(task['status'], 'completed')
        self.assertEqual(task['created'], original['created'])
        self.assertEqual((task['agent'], task['itemId'], task['turnId'], task['type']),
                         (original['agent'], 'old-item', 'old-turn', kind))
        self.assertEqual(self.runtime.agent(self.agent['id']), before)
        item = self.transcript()
        self.assertEqual((item['title'], item['turnId'], item['toolStatus']),
                         (kind, 'old-turn', 'completed'))
        self.assertEqual(json.loads(item['text'])['type'], kind)
        self.assertEqual(json.loads(item['text'])['contentItems'], complete['contentItems'])
        self.notify('item/completed', complete)
        self.notify('item/started', complete)
        self.assertEqual(self.task(), task)
        self.assertEqual(self.transcript(), item)
        self.assertEqual(self.runtime.agent(self.agent['id']), before)
        self.assertEqual(self.budget_rows(), budget)
        self.assertEqual(self.runtime.server.calls, calls)

    def test_missing_or_mismatched_historical_tool_is_not_created_or_settled(self):
        original = self.start('dynamicToolCall')
        before = copy.deepcopy(self.advance())
        for item, turn in (({'id': 'missing', 'type': 'dynamicToolCall'}, 'old-turn'),
                           ({'id': 'old-item', 'type': 'mcpToolCall'}, 'old-turn'),
                           ({'id': 'old-item', 'type': 'dynamicToolCall'}, 'other-turn')):
            self.notify('item/completed', {**item, 'status': 'completed'}, turn=turn)
        self.assertIsNone(self.task('missing'))
        self.assertIsNone(self.transcript('missing'))
        self.assertEqual(self.task(), original)
        self.assertEqual(self.runtime.agent(self.agent['id']), before)
        with self.runtime.lock, self.runtime.db() as db:
            original['agent'] = 'other-owner'
            self.runtime.put(db, 'tasks', original)
        self.notify('item/completed', {'id': 'old-item', 'type': 'dynamicToolCall',
                                     'status': 'completed'})
        self.assertEqual(self.task(), original)

    def test_params_item_id_cannot_replace_the_native_completion_identity(self):
        original = self.start('dynamicToolCall')
        transcript = self.transcript()
        before = copy.deepcopy(self.advance())
        for native_id in (None, '', 7):
            item = {'type': 'dynamicToolCall', 'status': 'completed', 'success': True}
            if native_id is not None:
                item['id'] = native_id
            self.runtime.notification({'method': 'item/completed', 'params': {
                'threadId': 'fixture-thread', 'turnId': 'old-turn',
                'itemId': 'old-item', 'item': item}})
            self.assertEqual(self.task(), original)
            self.assertEqual(self.transcript(), transcript)
            self.assertEqual(self.runtime.agent(self.agent['id']), before)

    def test_failure_receipt_settles_history_without_resuming_stopped_owner(self):
        self.start('mcpToolCall')
        before = copy.deepcopy(self.update(turnId='new-turn', epoch=self.agent['epoch'] + 1,
            status='paused', autoWake=False, inFlight=False, error='Explicit Stop', activeTools=[]))
        self.notify('item/completed', {'id': 'old-item', 'type': 'mcpToolCall',
            'status': 'failed', 'success': False, 'error': 'Exact native failure'})
        task = self.task()
        self.assertEqual((task['status'], task.get('error')), ('failed', 'Exact native failure'))
        self.assertEqual(self.runtime.agent(self.agent['id']), before)
        self.notify('item/completed', {'id': 'old-item', 'type': 'mcpToolCall',
                                     'status': 'completed', 'success': True})
        self.assertEqual(self.task(), task)

    def test_changed_account_or_connection_does_not_settle_history(self):
        original = self.start('dynamicToolCall')
        self.advance()
        item = {'id': 'old-item', 'type': 'dynamicToolCall', 'status': 'completed'}
        self.notify('item/completed', item, account='other-account')
        self.notify('item/completed', item, connection='retired-connection')
        self.assertEqual(self.task(), original)

    def test_current_tool_and_late_command_keep_existing_behavior(self):
        self.start('dynamicToolCall')
        self.notify('item/completed', {'id': 'old-item', 'type': 'dynamicToolCall',
                                     'status': 'completed'})
        self.assertEqual(self.task()['status'], 'completed')
        self.assertEqual(self.runtime.agent(self.agent['id'])['activeTools'], [])
        self.start('commandExecution', 'command')
        before = copy.deepcopy(self.advance())
        self.notify('item/completed', {'id': 'command', 'type': 'commandExecution',
            'status': 'completed', 'processId': '123', 'exitCode': 0,
            'aggregatedOutput': 'Exact command output'})
        self.assertEqual(self.task('command')['status'], 'completed')
        self.assertEqual(self.transcript('command')['title'], 'commandExecution')
        self.assertEqual(json.loads(self.transcript('command')['text'])['exitCode'], 0)
        self.assertEqual(self.runtime.agent(self.agent['id']), before)

    def test_exact_completion_removes_only_the_historical_task_blocker(self):
        from codex_agent_management import _blockers
        original = self.start('dynamicToolCall')
        self.advance()
        with self.runtime.lock, self.runtime.db() as db:
            before = _blockers(self.runtime, db, self.runtime.agent(self.agent['id'], db))
        self.assertEqual(next(b['ids'] for b in before if b['kind'] == 'background_tasks'),
                         [original['id']])
        self.notify('item/completed', {'id': 'old-item', 'type': 'dynamicToolCall',
                                     'status': 'completed', 'success': True})
        with self.runtime.lock, self.runtime.db() as db:
            after = _blockers(self.runtime, db, self.runtime.agent(self.agent['id'], db))
        self.assertFalse(any(b['kind'] == 'background_tasks' for b in after))
        self.assertTrue(any(b['kind'] == 'active_turn' for b in after))

    def test_mismatched_transcript_is_preserved_while_exact_task_settles(self):
        variants = [
            {'turnId': 'new-turn'},
            {'title': 'commandExecution'},
            {'text': json.dumps({'id': 'old-item', 'type': 'mcpToolCall'})},
            {'text': json.dumps({'id': 'other-item', 'type': 'dynamicToolCall'})},
            {'text': 'legacy invalid JSON'},
        ]
        for index, changes in enumerate(variants):
            with self.subTest(changes=changes):
                key = 'historical-' + str(index)
                self.update(turnId='old-turn')
                original = self.start('dynamicToolCall', key)
                before = copy.deepcopy(self.advance())
                saved = self.transcript(key)
                changes = copy.deepcopy(changes)
                if changes.get('text', '').startswith('{'):
                    native = json.loads(changes['text'])
                    if native['id'] == 'old-item':
                        native['id'] = key
                    changes['text'] = json.dumps(native)
                saved.update(changes)
                with self.runtime.lock, self.runtime.db() as db:
                    db.execute('UPDATE runtime_items SET record=? WHERE id=?',
                               (json.dumps(saved), saved['id']))
                self.notify('item/completed', {'id': key, 'type': 'dynamicToolCall',
                    'status': 'completed', 'success': True, 'contentItems': []})
                settled = self.task(key)
                self.assertEqual(settled['status'], 'completed')
                self.assertEqual(settled['turnId'], original['turnId'])
                self.assertEqual(self.transcript(key), saved)
                self.assertEqual(self.runtime.agent(self.agent['id']), before)


if __name__ == '__main__':
    unittest.main()

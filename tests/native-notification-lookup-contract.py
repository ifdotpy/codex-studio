#!/usr/bin/env python3
"""Native callbacks retain exact ownership while avoiding full agent decoding."""
import ast
import copy
from contextlib import contextmanager
import importlib.util
import inspect
import json
from pathlib import Path
import statistics
import sys
import tempfile
import textwrap
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import Runtime


class ControlledRuntime(Runtime):
    def schedule(self):
        pass


class NativeLookup(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.runtime = ControlledRuntime(Path(self.tmp.name), fixture.FakeServer)
        self.addCleanup(self.runtime.close)
        self.connection = patch.object(self.runtime, 'connection_current', return_value=True)
        self.connection.start()
        self.addCleanup(self.connection.stop)
        self.base = self.runtime.create({'name': 'Fixture', 'cwd': self.tmp.name, 'prompt': 'Fixture'}, defer=True)
        with self.runtime.db() as db:
            db.execute('DELETE FROM runtime_agents')

    def actor(self, key='target', **fields):
        agent = {**copy.deepcopy(self.base), 'id': key, 'rootId': key,
                 'threadId': 'shared-thread', 'turnId': 'current-turn',
                 'inFlight': True, 'status': 'running', 'autoWake': True, **fields}
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', agent)
        return agent

    def emit(self, *, account='default', method='item/agentMessage/delta', **params):
        self.runtime.notification({'method': method, 'params': {
            'threadId': 'shared-thread', 'turnId': 'current-turn', 'itemId': 'message',
            'delta': 'Exact output ✓\n', **params}}, account, 'connection')

    def item(self, key='target:message'):
        with self.runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_items WHERE id=?', (key,)).fetchone()
            return json.loads(row[0]) if row else None

    def test_account_and_missing_default_remain_distinct_from_null(self):
        self.actor('null', accountKey=None)
        self.actor('other', accountKey='other')
        default = self.actor('target')
        default.pop('accountKey')
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', default)
        self.emit()
        self.assertEqual(self.item()['text'], 'Exact output ✓\n')
        self.assertIsNone(self.item('other:message'))
        self.assertIsNone(self.item('null:message'))
        self.emit(account='other')
        self.assertEqual(self.item('other:message')['text'], 'Exact output ✓\n')

    def test_deleted_first_owner_does_not_route_to_a_duplicate_thread(self):
        self.actor('deleted', deletedAt=1)
        self.actor('target')
        self.emit()
        self.assertIsNone(self.item())
        self.assertIsNone(self.item('deleted:message'))

    def test_empty_unknown_or_stale_connection_does_not_write(self):
        self.actor()
        for tid in (None, '', 'unknown-thread'):
            self.emit(threadId=tid)
        with patch.object(self.runtime, 'connection_current', return_value=False):
            self.emit()
        self.assertIsNone(self.item())

    def test_transfer_changes_index_scope_and_old_turn_stays_stale(self):
        agent = self.actor()
        self.emit(turnId='old-turn')
        self.assertIsNone(self.item())
        agent.update(accountKey='other', threadId='new-thread')
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', agent)
        self.emit()
        self.emit(account='other', threadId='new-thread')
        self.assertEqual(self.item()['text'], 'Exact output ✓\n')

    def test_startup_adds_index_to_existing_agents_without_changing_their_scope(self):
        before = self.actor(inFlight=False, autoWake=False, status='paused')
        with self.runtime.db() as db:
            db.execute('DROP INDEX runtime_agent_native_scope')
        self.runtime.close()
        self.runtime = ControlledRuntime(Path(self.tmp.name), fixture.FakeServer)
        self.addCleanup(self.runtime.close)
        after = self.runtime.agent(before['id'])
        for field in ('id', 'threadId', 'accountKey', 'epoch', 'status', 'autoWake'):
            self.assertEqual(after[field], before[field])
        with self.runtime.db() as db:
            self.assertIsNotNone(db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_agent_native_scope'").fetchone())

    def test_thread_object_close_keeps_turn_and_discards_loaded_identity(self):
        agent = self.actor(inFlight=False)
        self.runtime.loaded.add(agent['id'])
        self.emit(method='thread/closed', threadId=None, thread={'id': agent['threadId']})
        self.assertNotIn(agent['id'], self.runtime.loaded)
        self.assertEqual(self.runtime.agent(agent['id'])['turnId'], 'current-turn')

    def test_late_command_completion_retains_original_task_without_changing_current_turn(self):
        self.actor()
        with self.runtime.db() as db:
            self.runtime.put(db, 'tasks', {'id': 'target:old-command', 'agent': 'target',
                'itemId': 'old-command', 'turnId': 'old-turn', 'kind': 'command', 'status': 'running'})
        self.emit(method='item/completed', turnId='old-turn', item={
            'id': 'old-command', 'type': 'commandExecution', 'status': 'completed',
            'exitCode': 0, 'aggregatedOutput': 'Original command finished'})
        with self.runtime.db() as db:
            task = json.loads(db.execute("SELECT record FROM runtime_tasks WHERE id='target:old-command'").fetchone()[0])
        self.assertEqual(task['status'], 'completed')
        self.assertEqual(task['tail'], 'Original command finished')
        self.assertEqual(self.runtime.agent('target')['turnId'], 'current-turn')

    def test_notification_uses_index_and_decodes_only_its_owner(self):
        for i in range(124):
            self.actor('peer-' + str(i), threadId='peer-thread-' + str(i), prompt='fixture ' * 400)
        self.actor()
        plans = []
        original_db = self.runtime.db
        @contextmanager
        def observed_db():
            with original_db() as db:
                db.set_trace_callback(lambda sql: plans.append(sql) if sql.startswith('SELECT record FROM runtime_agents WHERE json_extract') else None)
                yield db
        with patch.object(self.runtime, 'db', side_effect=observed_db):
            with patch.object(self.runtime, 'records', side_effect=AssertionError('Full registry decoding during callback')):
                self.emit()
        self.assertEqual(self.item()['text'], 'Exact output ✓\n')
        self.assertEqual(len(plans), 1)
        with self.runtime.db() as db:
            plan = db.execute('EXPLAIN QUERY PLAN ' + plans[0]).fetchall()
        self.assertTrue(any('USING INDEX runtime_agent_native_scope' in row[3] for row in plan), plan)


def legacy_notification():
    """Negative control: restore only the old actor scan in the current callback."""
    tree = ast.parse(textwrap.dedent(inspect.getsource(Runtime.notification)))
    function = tree.body[0]
    changed = False
    for block in ast.walk(function):
        if not isinstance(block, ast.With):
            continue
        body = block.body
        for i, node in enumerate(body):
            if (isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name)
                    and node.targets[0].id == 'row' and i + 2 < len(body)
                    and isinstance(body[i + 2], ast.Assign)
                    and isinstance(body[i + 2].targets[0], ast.Name)
                    and body[i + 2].targets[0].id == 'a'):
                body[i:i + 3] = ast.parse('''
a = next((a for a in self.records(db, "agents") if a.get("threadId") == tid and tid and a.get("accountKey", "default") == account_key), None)
if not a:
    return
''').body
                changed = True
                break
    if not changed:
        raise AssertionError('The legacy scan benchmark needs review after callback changes')
    scope = dict(Runtime.notification.__globals__)
    exec(compile(ast.fix_missing_locations(tree), '<legacy-notification-scan>', 'exec'), scope)
    return scope['notification']


def benchmark():
    baseline = legacy_notification()
    results = []
    # Live read-only sample: 124 agents, average record 3,763 characters.
    for count in (124, 512):
        timings = {'legacy': [], 'indexed': []}
        states = {}
        for label, callback in (('legacy', baseline), ('indexed', Runtime.notification)):
            case = NativeLookup()
            case.setUp()
            try:
                for i in range(count - 1):
                    case.actor('peer-' + str(i), threadId='peer-thread-' + str(i), prompt='x' * 2900)
                target = case.actor(prompt='x' * 2900)
                with case.runtime.db() as db:
                    average_size = db.execute('SELECT avg(length(record)) FROM runtime_agents').fetchone()[0]
                messages = [{'method': 'item/started', 'params': {'threadId': target['threadId'], 'turnId': target['turnId'],
                    'item': {'id': 'command', 'type': 'commandExecution', 'status': 'inProgress', 'command': 'fixture'}}}]
                messages += [{'method': 'item/commandExecution/outputDelta', 'params': {
                    'threadId': target['threadId'], 'turnId': target['turnId'], 'itemId': 'command', 'delta': 'Exact output ✓\n'}} for _ in range(200)]
                messages += [{'method': 'item/completed', 'params': {'threadId': target['threadId'], 'turnId': target['turnId'],
                    'item': {'id': 'command', 'type': 'commandExecution', 'status': 'completed', 'exitCode': 0,
                             'aggregatedOutput': 'Exact output ✓\n' * 200}}}]
                for message in messages:
                    start = time.perf_counter()
                    callback(case.runtime, message, 'default', 'connection')
                    timings[label].append((time.perf_counter() - start) * 1000)
                with case.runtime.db() as db:
                    task = json.loads(db.execute("SELECT record FROM runtime_tasks WHERE id='target:command'").fetchone()[0])
                    item = json.loads(case.item('target:command')['text'])
                states[label] = {'status': task['status'], 'tail': task['tail'], 'exitCode': task['exitCode'],
                                 'item': item, 'events': case.runtime.agent('target')['events']}
            finally:
                case.doCleanups()
        assert states['legacy'] == states['indexed'], 'Callback content or terminal receipt changed'
        assert states['indexed']['tail'] == 'Exact output ✓\n' * 200
        result = {'agents': count, 'meanAgentRecordChars': round(average_size), 'callbacks': len(messages),
                  'sameContentAndTerminalReceipt': True}
        for label, values in timings.items():
            result[label] = {'medianMs': round(statistics.median(values), 3),
                             'p95Ms': round(sorted(values)[int((len(values) - 1) * .95)], 3),
                             'totalMs': round(sum(values), 3)}
        results.append(result)
    print(json.dumps(results, indent=2))


if __name__ == '__main__':
    if '--benchmark' in sys.argv:
        benchmark()
    else:
        unittest.main(verbosity=2)

#!/usr/bin/env python3
"""Active-task guards exclude historical payloads and retain account ownership."""
from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime
from codex_workspace import WorkspaceMixin, active_task_records
from codex_agent_management import _blockers


class Fixture:
    disconnected = Runtime.disconnected
    _assert_workspace_idle = WorkspaceMixin._assert_workspace_idle
    records = staticmethod(Runtime.records)

    def __init__(self):
        self.connection = sqlite3.connect(':memory:')
        self.connection.row_factory = sqlite3.Row
        self.lock = threading.RLock()
        self.connection_ids = {'one': 'connection-one'}
        self.offline_accounts = set()
        self.loaded = {'first', 'second'}
        self.preparations = {}
        for table in ('agents', 'tasks', 'monitors', 'requests', 'work'):
            self.connection.execute(f'CREATE TABLE runtime_{table}(id TEXT PRIMARY KEY, record TEXT NOT NULL)')
        self.connection.execute("CREATE INDEX runtime_task_status ON runtime_tasks(json_extract(record,'$.status'),json_extract(record,'$.created'))")
        self.connection.execute('CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT, epoch INTEGER, status TEXT, error TEXT)')
        for key, account in [('first', 'one'), ('second', 'two')]:
            self.put(self.connection, 'agents', {'id': key, 'accountKey': account, 'status': 'completed',
                'inFlight': False, 'cwd': '/tmp/' + key, 'epoch': 1})
        self.connection.executemany('INSERT INTO runtime_tasks VALUES (?,?)', (
            (f'old-{i}', json.dumps({'id': f'old-{i}', 'agent': 'first', 'status': 'completed',
                                   'created': i, 'tail': 'historical-marker' * 50})) for i in range(2000)))
        for key, owner, status in [('running-first', 'first', 'running'), ('running-second', 'second', 'running'),
                                    ('starting-first', 'first', 'starting'), ('pending-first', 'first', 'pending'),
                                    ('unknown-first', 'first', 'unknown')]:
            self.put(self.connection, 'tasks', {'id': key, 'agent': owner, 'status': status, 'created': 3000})

    @contextmanager
    def db(self):
        with self.connection:
            yield self.connection

    def put(self, db, table, record):
        db.execute(f'INSERT OR REPLACE INTO runtime_{table} VALUES (?,?)', (record['id'], json.dumps(record)))

    def agent(self, key, db):
        return json.loads(db.execute('SELECT record FROM runtime_agents WHERE id=?', (key,)).fetchone()[0])

    def capacity_restart(self, db, agent):
        pass

    def _workspace_operation_busy(self, db, cwd, reservation):
        return False

    def resource_action(self):
        return {'state': {'claims': {}, 'queue': {}}}


class ActiveTasks(unittest.TestCase):
    def setUp(self):
        self.runtime = Fixture()
        original = json.loads
        def decode(value, *args, **kwargs):
            if isinstance(value, str) and 'historical-marker' in value:
                raise AssertionError('A completed task payload was decoded')
            return original(value, *args, **kwargs)
        self.decoder = patch('json.loads', side_effect=decode)
        self.decoder.start()

    def tearDown(self):
        self.decoder.stop()
        self.runtime.connection.close()

    def test_query_uses_existing_status_index_and_excludes_history(self):
        db = self.runtime.connection
        plan = db.execute("EXPLAIN QUERY PLAN SELECT record FROM runtime_tasks WHERE json_extract(record,'$.status') IN (?)", ('running',)).fetchall()
        self.assertTrue(any('runtime_task_status' in row[3] for row in plan))
        steps = [0]
        def progress():
            steps[0] += 1
            return steps[0] > 20
        db.set_progress_handler(progress, 100)
        try:
            self.assertEqual({r['id'] for r in active_task_records(db)}, {'running-first', 'running-second'})
        finally:
            db.set_progress_handler(None, 0)
        self.assertLess(steps[0], 20, 'The query must not visit 2000 historical rows')

    def test_disconnect_marks_only_running_tasks_from_own_account_lost(self):
        self.runtime.disconnected('one', 'connection-one')
        rows = {r['id']: r for r in active_task_records(self.runtime.connection, ('running', 'lost', 'starting', 'pending', 'unknown'))}
        self.assertEqual(rows['running-first']['status'], 'lost')
        self.assertEqual(rows['running-first']['error'], 'Codex disconnected. Tool outcome unknown.')
        self.assertEqual(rows['running-second']['status'], 'running')
        for status in ('starting', 'pending', 'unknown'):
            self.assertEqual(rows[status + '-first']['status'], status)
        self.assertEqual(self.runtime.loaded, {'second'})

    def test_idle_guard_still_blocks_matching_live_work(self):
        db = self.runtime.connection
        agent = self.runtime.agent('first', db)
        with self.assertRaisesRegex(ValueError, 'command or tool'):
            self.runtime._assert_workspace_idle(db, agent)
        db.execute("DELETE FROM runtime_tasks WHERE id='running-first'")
        self.runtime._assert_workspace_idle(db, agent)

    def test_team_skips_unused_work_and_preserves_response(self):
        first = {'id': 'lead', 'rootId': 'lead', 'name': 'Lead', 'status': 'running'}
        other = {'id': 'other', 'rootId': 'other'}
        snapshots = []
        class Team:
            def snapshot(self, *, include_work=True):
                snapshots.append(include_work)
                if include_work:
                    raise AssertionError('Team lookup loaded unused work history')
                return {'agents': [first, other], 'monitors': [{'id': 'yes', 'agent': 'lead'}, {'id': 'no', 'agent': 'other'}]}
            def agent(self, key):
                return first
            def worker_defaults(self, agent):
                return {'model': 'configured'}
        result = Runtime.team(Team(), 'lead')
        self.assertEqual(snapshots, [False])
        self.assertEqual(result['workerDefaults'], {'model': 'configured'})
        self.assertEqual(result['monitors'], [{'id': 'yes', 'agent': 'lead'}])
        self.assertEqual(result['agents'], [{key: first.get(key) for key in
            ('id', 'parentId', 'name', 'status', 'cwd', 'model', 'effort', 'fastMode', 'workerDefaults', 'tokensUsed', 'error')}])

    def test_archive_preserves_all_original_uncertain_status_guards(self):
        db = self.runtime.connection
        blockers = _blockers(self.runtime, db, self.runtime.agent('first', db))
        tasks = next(b for b in blockers if b['kind'] == 'background_tasks')
        self.assertEqual(set(tasks['ids']), {'running-first', 'starting-first', 'pending-first', 'unknown-first'})
        self.assertEqual(tasks['count'], 4)


if __name__ == '__main__':
    unittest.main()

#!/usr/bin/env python3
"""Work owners and transfer roots recheck only the agents they can select."""
import importlib.util
import json
from pathlib import Path
import unittest
import time

spec = importlib.util.spec_from_file_location('roster_contract',
    Path(__file__).with_name('scheduler-roster-cache-cpu-contract.py'))
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)


class DependencyRoster(unittest.TestCase):
    connect = base.RosterCache.connect
    add = base.RosterCache.add
    roster = base.RosterCache.roster
    ids = base.RosterCache.ids
    tearDown = base.RosterCache.tearDown

    def setUp(self):
        base.RosterCache.setUp(self)
        self.runtime.db_path = self.path
        self.runtime.install_scheduler_change_tracking(self.db)
        self.db.execute("CREATE INDEX runtime_agent_root ON runtime_agents(json_extract(record,'$.rootId'))")
        self.db.commit()

    def full_reads(self):
        return sum(' AND id IN (' not in sql for sql in self.scans)

    def work(self, key, owner, status, db=None):
        target = db or self.db
        target.execute('INSERT INTO runtime_work VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                       (key, json.dumps({'id': key, 'owner': owner, 'status': status})))
        target.commit()

    def transfer(self, key, root, status, db=None):
        target = db or self.db
        target.execute('INSERT INTO runtime_account_transfers VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                       (key, json.dumps({'id': key, 'leadId': root, 'status': status})))
        target.commit()

    def test_work_owner_states_and_duplicate_owners_change_only_membership(self):
        self.add('failed', status='failed')
        self.add('deleted', deletedAt=1)
        self.add('idle')
        self.add('active', status='running')
        self.assertEqual(self.ids(), {'active'})
        writer = self.connect()
        for status in ('ready', 'running', 'blocked'):
            self.work('work', 'failed', status, writer)
            self.assertEqual(self.ids(), {'failed', 'active'})
        self.work('second', 'failed', 'ready', writer)
        self.work('work', 'deleted', 'blocked', writer)
        self.assertEqual(self.ids(), {'failed', 'deleted', 'active'})
        self.work('second', 'missing', 'accepted', writer)
        self.assertEqual(self.ids(), {'deleted', 'active'})
        self.work('work', 'idle', 'running', writer)
        self.assertEqual(self.ids(), {'active'}, 'A healthy idle owner does not need release work')
        self.work('work', None, 'ready', writer)
        self.assertEqual(self.ids(), {'active'})
        writer.execute('DELETE FROM runtime_work')
        writer.commit()
        self.assertEqual(self.ids(), {'active'})
        self.assertEqual(self.full_reads(), 1, 'Owner dependencies must not scan unrelated agent JSON')

    def test_transfer_roots_use_index_and_preserve_deleted_agent_rules(self):
        self.add('lead', rootId='lead', parentId=None, isLead=True)
        self.add('child', rootId='lead', parentId='lead')
        self.add('deleted', rootId='lead', deletedAt=1)
        self.add('other', rootId='other')
        self.add('unrelated', rootId='unrelated', status='running')
        self.assertEqual(self.ids(), {'unrelated'})
        self.transfer('first', 'lead', 'pending')
        self.assertEqual(self.ids(), {'lead', 'child', 'unrelated'})
        self.transfer('duplicate', 'lead', 'pending')
        self.transfer('first', 'other', 'pending')
        self.assertEqual(self.ids(), {'lead', 'child', 'other', 'unrelated'})
        self.transfer('duplicate', 'lead', 'completed')
        self.assertEqual(self.ids(), {'other', 'unrelated'})
        self.transfer('first', 'other', 'failed')
        self.assertEqual(self.ids(), {'unrelated'})
        self.assertEqual(self.full_reads(), 1)
        plan = self.db.execute("EXPLAIN QUERY PLAN SELECT id FROM runtime_agents WHERE "
                               "json_extract(record,'$.rootId') IN (?)", ('lead',)).fetchall()
        self.assertTrue(any('runtime_agent_root' in row[3] for row in plan))

    def test_combined_agent_owner_and_transfer_change_keep_current_order(self):
        self.add('first', status='failed', rootId='first-root')
        self.add('second', status='running', rootId='second-root')
        self.add('third', rootId='third-root')
        self.work('work', 'first', 'ready')
        self.assertEqual([row['id'] for row in self.roster()], ['first', 'second'])
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','paused') WHERE id='second'")
        self.db.execute("UPDATE runtime_work SET record=json_set(record,'$.owner','second')")
        self.db.execute("INSERT INTO runtime_account_transfers VALUES('transfer',?)",
                        (json.dumps({'leadId': 'third-root', 'status': 'pending'}),))
        self.db.commit()
        self.assertEqual([row['id'] for row in self.roster()], ['third'])
        self.assertEqual(self.full_reads(), 1)

    def test_large_transfer_delta_batches_root_and_agent_parameters(self):
        agents = [(str(index), json.dumps({'id': str(index), 'rootId': 'root-' + str(index),
                   'status': 'completed'})) for index in range(260)]
        self.db.executemany('INSERT INTO runtime_agents VALUES(?,?)', agents)
        self.db.commit()
        self.assertEqual(self.ids(), set())
        self.db.executemany('INSERT INTO runtime_account_transfers VALUES(?,?)',
            [(key, json.dumps({'leadId': 'root-' + key, 'status': 'pending'})) for key, _ in agents])
        self.db.commit()
        self.assertEqual(self.ids(), {key for key, _ in agents})
        self.db.execute("UPDATE runtime_account_transfers SET record=json_set(record,'$.status','completed')")
        self.db.commit()
        self.assertEqual(self.ids(), set())
        self.assertEqual(self.full_reads(), 1)

    def test_dependency_rollback_and_other_writer_do_not_publish_uncommitted_rows(self):
        self.add('first', status='failed')
        self.add('second', status='failed')
        self.assertEqual(self.ids(), set())
        published = self.runtime._scheduler_agent_roster
        self.db.execute("INSERT INTO runtime_work VALUES('work',?)", (json.dumps({'owner': 'first', 'status': 'ready'}),))
        self.assertEqual(self.ids(), {'first'})
        self.assertIs(self.runtime.__dict__.get("_scheduler_agent_roster"), published)
        self.assertEqual(self.ids(self.connect()), set())
        self.db.rollback()
        writer = self.connect()
        self.work('work', 'second', 'ready', writer)
        self.assertEqual(self.ids(), {'second'})
        self.assertEqual(self.full_reads(), 1)

    def test_old_wal_dependency_snapshot_does_not_replace_current_roster(self):
        self.add('first', status='failed')
        self.add('second', status='failed')
        self.work('work', 'first', 'ready')
        self.assertEqual(self.ids(), {'first'})
        old = self.connect()
        old.execute('BEGIN')
        self.assertEqual(self.ids(old), {'first'})
        self.work('work', 'second', 'ready')
        self.assertEqual(self.ids(), {'second'})
        published = self.runtime._scheduler_agent_roster
        self.assertEqual(self.ids(old), {'first'})
        self.assertIs(self.runtime.__dict__.get("_scheduler_agent_roster"), published)
        old.rollback()
        self.assertEqual(self.ids(), {'second'})
        self.assertEqual(self.full_reads(), 1)

    def test_concurrent_commit_after_transfer_id_read_reloads_full_current_predicate(self):
        self.add('first', rootId='first-root')
        self.add('second', rootId='second-root')
        self.assertEqual(self.ids(), set())
        self.transfer('transfer', 'first-root', 'pending')
        writer = self.connect()
        inner = self.db
        fired = False

        class Cursor:
            def __init__(self, cursor):
                self.cursor = cursor
            def fetchall(self):
                nonlocal fired
                rows = self.cursor.fetchall()
                if not fired:
                    fired = True
                    writer.execute("UPDATE runtime_account_transfers SET record=json_set(record,'$.leadId','second-root')")
                    writer.commit()
                return rows

        class Connection:
            def __getattr__(self, name):
                return getattr(inner, name)
            def execute(self, sql, *args):
                cursor = inner.execute(sql, *args)
                return Cursor(cursor) if sql.startswith('SELECT id FROM runtime_agents WHERE ') else cursor

        self.assertEqual(self.ids(Connection()), {'second'})
        self.assertTrue(fired)
        self.assertEqual(self.full_reads(), 2, 'A changed snapshot requires the existing complete fallback')
        self.assertFalse(self.db.in_transaction)


class RuntimeWorkOwner(unittest.TestCase):
    def setUp(self):
        scope_spec = importlib.util.spec_from_file_location('scope_contract',
            Path(__file__).with_name('scheduler-agent-scope-contract.py'))
        self.scope = importlib.util.module_from_spec(scope_spec)
        scope_spec.loader.exec_module(self.scope)
        self.scope.SchedulerAgentScope.setUp(self)

    def tearDown(self):
        self.scope.SchedulerAgentScope.tearDown(self)

    def test_real_put_keeps_committed_roster_for_dependency_delta(self):
        root = self.runtime.create({'name': 'Root', 'cwd': self.tmp.name, 'prompt': 'Coordinate'}, defer=True)
        owner = dict(root, id='archived-work-owner', rootId=root['id'], parentId=root['id'],
                     isLead=False, name='Work owner', status='completed', autoWake=False,
                     deletedAt=time.time())
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', owner)
        with self.runtime.db() as db:
            self.assertNotIn(owner['id'], {row['id'] for row in self.runtime.scheduler_agents(db)})
        published = self.runtime._scheduler_agent_roster
        scans = []
        with self.runtime.lock, self.runtime.db() as db:
            db.set_trace_callback(scans.append)
            work = {'id': 'owner-work', 'rootId': root['id'], 'owner': owner['id'],
                    'status': 'ready', 'dependencies': []}
            self.runtime.put(db, 'work', work)
            self.assertIs(self.runtime.__dict__.get("_scheduler_agent_roster"), published)
            self.assertIn(owner['id'], {row['id'] for row in self.runtime.scheduler_agents(db)})
            self.assertIs(self.runtime.__dict__.get("_scheduler_agent_roster"), published)
            work['status'] = 'accepted'
            self.runtime.put(db, 'work', work)
            self.assertNotIn(owner['id'], {row['id'] for row in self.runtime.scheduler_agents(db)})
        reads = [sql for sql in scans if sql.startswith('SELECT id,record,rowid FROM runtime_agents')]
        self.assertEqual(sum(' AND id IN (' not in sql for sql in reads), 0)


if __name__ == '__main__':
    unittest.main()

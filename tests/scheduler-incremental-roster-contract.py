#!/usr/bin/env python3
"""Changed agent IDs reduce scheduler work without losing SQLite snapshot identity."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import time
from types import MethodType
import unittest

spec = importlib.util.spec_from_file_location('roster_contract',
    Path(__file__).with_name('scheduler-roster-cache-cpu-contract.py'))
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)


class IncrementalRoster(base.RosterCache):
    def setUp(self):
        super().setUp()
        self.runtime.db_path = self.path
        self.runtime.install_scheduler_change_tracking(self.db)

    def full_reads(self):
        return sum(' AND id IN (' not in sql for sql in self.scans)

    def update(self, key, **changes):
        raw = self.db.execute('SELECT record FROM runtime_agents WHERE id=?', (key,)).fetchone()[0]
        agent = json.loads(raw)
        agent.update(changes)
        self.db.execute('UPDATE runtime_agents SET record=? WHERE id=?', (json.dumps(agent), key))
        self.db.commit()

    def test_single_actor_updates_recheck_only_its_exact_membership(self):
        self.add('first', status='running')
        self.add('second', status='running')
        self.add('idle')
        self.assertEqual(self.ids(), {'first', 'second'})
        full = self.full_reads()
        self.update('first', status='paused')
        self.assertEqual(self.ids(), {'second'})
        self.update('idle', status='queued', autoWake=True)
        self.assertEqual([row['id'] for row in self.roster()], ['second', 'idle'])
        self.update('second', metadata={'nested': ['fresh']})
        rows = self.roster()
        self.assertEqual(rows[0]['metadata'], {'nested': ['fresh']})
        rows[0]['metadata']['nested'].append('caller-change')
        self.assertEqual(self.roster()[0]['metadata'], {'nested': ['fresh']})
        self.assertEqual(self.full_reads(), full)

    def test_id_rowid_replace_and_delete_keep_exact_row_order(self):
        self.add('first', status='running')
        self.add('second', status='running')
        self.assertEqual([row['id'] for row in self.roster()], ['first', 'second'])
        self.db.execute("UPDATE runtime_agents SET id='new',record=json_set(record,'$.id','new') WHERE id='first'")
        self.db.commit()
        self.assertEqual([row['id'] for row in self.roster()], ['new', 'second'])
        self.db.execute("UPDATE runtime_agents SET rowid=-1 WHERE id='second'")
        self.db.commit()
        self.assertEqual([row['id'] for row in self.roster()], ['second', 'new'])
        raw = self.db.execute("SELECT record FROM runtime_agents WHERE id='second'").fetchone()[0]
        self.db.execute("INSERT OR REPLACE INTO runtime_agents VALUES('second',?)", (raw,))
        self.db.commit()
        self.assertEqual([row['id'] for row in self.roster()], ['new', 'second'])
        self.db.execute("DELETE FROM runtime_agents WHERE id='new'")
        self.db.commit()
        self.assertEqual(self.ids(), {'second'})
        self.assertEqual(self.full_reads(), 1)

    def test_primary_key_only_update_does_not_leave_old_sql_identity(self):
        self.add(status='running')
        self.roster()
        self.db.execute("UPDATE runtime_agents SET id='new'")
        self.db.commit()
        self.assertEqual(self.roster()[0]['id'], 'agent')
        self.assertEqual(set(self.runtime._scheduler_agent_roster['records']), {'new'})
        self.assertEqual(self.full_reads(), 1)

    def test_deleted_cleanup_and_unknown_receipts_keep_original_predicate(self):
        self.add()
        self.assertEqual(self.ids(), set())
        self.update('agent', deletedAt=1, workspaceOperation='exact-reservation')
        self.assertEqual(self.ids(), {'agent'})
        self.update('agent', workspaceOperation=None, status='completed',
                    startAttempt={'submitted': True, 'executionOutcome': 'unknown', 'turnId': 'turn'},
                    lastCompletedTurn='turn', lastCompletedTurnStatus='completed')
        self.assertEqual(self.ids(), set(), 'Deleted owner needs an actual cleanup dependency')
        self.update('agent', deletedAt=None)
        self.assertEqual(self.ids(), {'agent'}, 'Unknown native submission stays in recovery')
        self.update('agent', startAttempt={'submitted': True, 'turnId': 'turn'})
        self.assertEqual(self.ids(), set())
        self.assertEqual(self.full_reads(), 1)

    def test_missing_changed_row_and_corrupt_state_use_full_predicate(self):
        for failure in ('row', 'total', 'floor', 'generation'):
            with self.subTest(failure=failure):
                self.add('agent-' + failure, status='running')
                self.roster()
                self.update('agent-' + failure, name='fresh')
                state = tuple(self.db.execute('SELECT floor,generation,entries,total '
                                              'FROM runtime_scheduler_agent_change_state').fetchone())
                changed_generation = self.db.execute('SELECT generation FROM runtime_scheduler_agent_changes '
                    'WHERE id=?', ('agent-' + failure,)).fetchone()[0]
                if failure == 'row':
                    self.db.execute('DELETE FROM runtime_scheduler_agent_changes WHERE id=?', ('agent-' + failure,))
                else:
                    self.db.execute('UPDATE runtime_scheduler_agent_change_state SET ' + failure + '=' + failure + '+999')
                self.db.commit()
                full = self.full_reads()
                self.assertEqual(next(row for row in self.roster() if row['id'] == 'agent-' + failure)['name'], 'fresh')
                self.assertEqual(self.full_reads(), full + 1)
                self.db.execute('INSERT INTO runtime_scheduler_agent_changes VALUES(?,?) '
                    'ON CONFLICT(id) DO UPDATE SET generation=excluded.generation', ('agent-' + failure, changed_generation))
                self.db.execute('UPDATE runtime_scheduler_agent_change_state SET floor=?,generation=?,entries=?,total=?', state)
                self.db.commit()

    def test_missing_trigger_changes_schema_key_even_without_generation_bump(self):
        self.add(status='running')
        self.roster()
        self.db.execute('DROP TRIGGER runtime_agent_record_generation_update')
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','approval')")
        self.db.commit()
        self.assertEqual(self.roster()[0]['status'], 'approval')
        self.assertEqual(self.full_reads(), 2)
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','paused')")
        self.db.commit()
        self.assertIsNone(self.runtime.scheduler_roster_key(self.db))
        self.assertEqual(self.ids(), set())
        self.assertEqual(self.full_reads(), 3)

    def test_uncommitted_ddl_proof_is_not_reused_after_schema_version_rollback(self):
        self.add(status='running')
        self.roster()
        self.db.execute('BEGIN')
        self.db.execute('DROP TRIGGER runtime_agent_record_generation_update')
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','approval')")
        schema = self.db.execute('PRAGMA schema_version').fetchone()[0]
        self.assertIsNone(self.runtime.scheduler_roster_key(self.db))
        self.assertEqual(self.roster()[0]['status'], 'approval')
        self.db.rollback()
        writer = self.connect()
        writer.execute('DROP TRIGGER runtime_agent_record_generation_update')
        writer.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','paused')")
        writer.commit()
        self.assertEqual(writer.execute('PRAGMA schema_version').fetchone()[0], schema)
        self.assertIsNone(self.runtime.scheduler_roster_key(self.db))
        self.assertEqual(self.ids(), set())

    def test_journal_floor_before_cached_generation_uses_full_read(self):
        self.add(status='running')
        self.roster()
        self.db.execute('DROP TABLE runtime_scheduler_agent_changes')
        self.db.execute('DROP TABLE runtime_scheduler_agent_change_state')
        for action in ('insert', 'update', 'delete'):
            self.db.execute('DROP TRIGGER runtime_agent_record_generation_' + action)
        self.db.commit()
        self.runtime.install_scheduler_change_tracking(self.db)
        self.update('agent', status='approval')
        self.assertEqual(self.roster()[0]['status'], 'approval')
        self.assertEqual(self.full_reads(), 2)

    def test_reinstallation_requires_full_seed_after_untracked_same_generation_write(self):
        self.add(status='running')
        self.roster()
        generation = self.db.execute('SELECT value FROM runtime_agent_record_generation').fetchone()[0]
        self.db.execute('DROP TRIGGER runtime_agent_record_generation_update')
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','paused')")
        self.db.commit()
        self.runtime.install_scheduler_change_tracking(self.db)
        self.assertEqual(self.db.execute('SELECT value FROM runtime_agent_record_generation').fetchone()[0], generation)
        self.assertEqual(self.db.execute('SELECT floor FROM runtime_scheduler_agent_change_state').fetchone()[0], generation)
        self.assertEqual(self.ids(), set())
        self.assertEqual(self.full_reads(), 2)

    def test_changed_ids_and_selected_rows_share_snapshot_and_race_falls_back(self):
        self.add('first', status='running')
        self.add('second', status='running')
        self.roster()
        self.update('first', name='first-new')
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
                    writer.execute("UPDATE runtime_agents SET record=json_set(record,'$.name','second-new') WHERE id='second'")
                    writer.commit()
                return rows
        class Connection:
            def __getattr__(self, name):
                return getattr(inner, name)
            def execute(self, sql, *args):
                cursor = inner.execute(sql, *args)
                return Cursor(cursor) if sql.startswith('SELECT id,record') and ' AND id IN (' in sql else cursor
        current = {row['id']: row['name'] for row in self.roster(Connection())}
        self.assertEqual(current, {'first': 'first-new', 'second': 'second-new'})
        self.assertEqual(self.full_reads(), 2)
        self.assertFalse(self.db.in_transaction)

    def test_old_wal_view_keeps_newer_published_roster(self):
        self.add(status='running')
        old = self.connect()
        old.execute('BEGIN')
        self.roster(old)
        self.roster()
        self.update('agent', status='approval')
        self.roster()
        published = self.runtime._scheduler_agent_roster
        self.assertEqual(self.roster(old)[0]['status'], 'running')
        self.assertIs(self.runtime._scheduler_agent_roster, published)
        old.rollback()

    def test_rollback_journal_is_not_reused_when_generation_repeats(self):
        self.add('first', status='running')
        self.add('second', status='running')
        self.roster()
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.name','rolled-back') WHERE id='first'")
        generation = self.db.execute('SELECT value FROM runtime_agent_record_generation').fetchone()[0]
        self.assertEqual(self.roster()[0]['name'], 'rolled-back')
        self.db.rollback()
        self.update('second', name='committed')
        self.assertEqual(self.db.execute('SELECT value FROM runtime_agent_record_generation').fetchone()[0], generation)
        self.assertEqual({row['id']: row['name'] for row in self.roster()}, {'first': 'first', 'second': 'committed'})

    def test_installer_is_idempotent_and_preserves_unknown_schema(self):
        self.add(status='running')
        before = tuple(self.db.execute('SELECT * FROM runtime_scheduler_agent_change_state').fetchone())
        self.runtime.install_scheduler_change_tracking(self.db)
        self.assertEqual(tuple(self.db.execute('SELECT * FROM runtime_scheduler_agent_change_state').fetchone()), before)
        self.db.execute('DROP INDEX runtime_scheduler_agent_change_generation')
        self.db.execute('CREATE INDEX runtime_scheduler_agent_change_generation ON runtime_scheduler_agent_changes(id)')
        self.db.commit()
        schema = list(self.db.execute('SELECT name,sql FROM sqlite_master ORDER BY name'))
        with self.assertRaisesRegex(RuntimeError, 'Foreign scheduler'):
            self.runtime.install_scheduler_change_tracking(self.db)
        self.assertEqual(list(self.db.execute('SELECT name,sql FROM sqlite_master ORDER BY name')), schema)

    def test_installer_failure_rolls_back_all_trigger_replacement(self):
        original = self.db
        before = list(original.execute('SELECT name,sql FROM sqlite_master ORDER BY name'))
        # Move back to known old triggers, leaving the fresh journal present.
        for action in ('insert', 'update', 'delete'):
            original.execute('DROP TRIGGER runtime_agent_record_generation_' + action)
        original.commit()
        before = list(original.execute('SELECT name,sql FROM sqlite_master ORDER BY name'))
        class Connection:
            def __getattr__(self, name):
                return getattr(original, name)
            def execute(self, sql, *args):
                if sql.startswith('CREATE TRIGGER runtime_agent_record_generation_update'):
                    raise OSError('Private install failure')
                return original.execute(sql, *args)
        with self.assertRaisesRegex(OSError, 'Private install failure'):
            self.runtime.install_scheduler_change_tracking(Connection())
        self.assertEqual(list(original.execute('SELECT name,sql FROM sqlite_master ORDER BY name')), before)
        self.assertFalse(original.in_transaction)

    def test_installer_lock_scope_and_busy_timeout_remain_bounded(self):
        with self.runtime.lock, self.assertRaisesRegex(RuntimeError, 'outside Runtime.lock'):
            self.runtime.install_scheduler_change_tracking(self.db)
        writer = self.connect()
        writer.execute('BEGIN IMMEDIATE')
        self.db.execute('PRAGMA busy_timeout=2000')
        started = time.monotonic()
        with self.assertRaises(sqlite3.OperationalError):
            self.runtime.install_scheduler_change_tracking(self.db)
        self.assertLess(time.monotonic() - started, .8)
        self.assertEqual(self.db.execute('PRAGMA busy_timeout').fetchone()[0], 2000)
        self.assertFalse(self.db.in_transaction)
        writer.rollback()

    def test_selected_roster_can_exceed_bounded_decode_cache(self):
        records = [(str(index), json.dumps({'id': str(index), 'status': 'running'})) for index in range(4097)]
        self.db.executemany('INSERT INTO runtime_agents VALUES(?,?)', records)
        self.db.commit()
        self.assertEqual(len(self.roster()), 4097)
        self.assertLessEqual(len(self.runtime._scheduler_agent_cache), 4096)
        self.update('0', status='approval')
        rows = self.roster()
        self.assertEqual(len(rows), 4097)
        self.assertEqual(rows[0]['status'], 'approval')

    def test_paired_frequent_actor_updates_include_writer_cost(self):
        baseline = base.RosterCache()
        baseline.setUp()
        self.addCleanup(baseline.tearDown)
        def full_rows(runtime, db, _cached, query):
            # The same maintained predicate provides a complete read baseline.
            return base.Runtime.scheduler_agent_rows(runtime, db, None, query)
        baseline.runtime.scheduler_agent_rows = MethodType(full_rows, baseline.runtime)
        payloads = []
        for index in range(2192):
            agent = {'id': str(index), 'rootId': 'root', 'parentId': 'root', 'epoch': 1,
                     'status': 'running' if index < 9 else 'completed', 'inFlight': index < 9,
                     'autoWake': False, 'prompt': 'x' * 7500, 'lastCompletedTurn': 'turn',
                     'lastCompletedTurnStatus': 'completed', 'worktree': index < 634}
            if index >= 9:
                agent.update(restartRecovery={'stage': 'finished'}, contextRepair={'phase': 'completed'},
                             startAttempt={'submitted': True, 'turnId': 'turn'})
            payloads.append((str(index), json.dumps(agent)))
        results = []
        for case in (baseline, self):
            case.db.executemany('INSERT INTO runtime_agents VALUES(?,?)', payloads)
            case.db.executescript("""
                CREATE INDEX fixture_root ON runtime_agents(json_extract(record,'$.rootId'));
                CREATE INDEX fixture_account ON runtime_agents(json_extract(record,'$.accountKey'));
                CREATE INDEX fixture_inflight ON runtime_agents(json_extract(record,'$.inFlight'));
            """)
            case.db.commit()
            self.assertEqual({row['id'] for row in case.roster()}, {str(index) for index in range(9)})
            writes = []
            write_wall = []
            started = time.thread_time()
            started_wall = time.monotonic()
            scans = len(case.scans)
            for iteration in range(24):
                write = time.thread_time()
                write_started = time.monotonic()
                case.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.lastAnswer',?) WHERE id=?",
                                ('new-' + str(iteration), str(iteration % 9)))
                case.db.commit()
                writes.append(time.thread_time() - write)
                write_wall.append(time.monotonic() - write_started)
                rows = case.roster()
                self.assertEqual({row['id'] for row in rows}, {str(index) for index in range(9)})
                self.assertEqual(next(row for row in rows if row['id'] == str(iteration % 9))['lastAnswer'], 'new-' + str(iteration))
            results.append({'totalCpuMs': (time.thread_time() - started) * 1000,
                            'totalWallMs': (time.monotonic() - started_wall) * 1000,
                            'writerP95CpuMs': sorted(writes)[22] * 1000,
                            'writerP95WallMs': sorted(write_wall)[22] * 1000,
                            'writerCpuMs': sum(writes) * 1000,
                            'fullReads': sum(' AND id IN (' not in sql for sql in case.scans[scans:])})
        print(json.dumps({'rows': len(payloads), 'rawBytes': sum(len(raw.encode()) for _, raw in payloads),
                          'baseline': results[0], 'incremental': results[1]}), flush=True)
        self.assertEqual(results[1]['fullReads'], 0)
        self.assertEqual(results[0]['fullReads'], 24)
        self.assertLess(results[1]['totalCpuMs'], results[0]['totalCpuMs'] * .25)
        self.assertLess(results[1]['writerP95CpuMs'], max(1, results[0]['writerP95CpuMs'] * 2))
        self.assertLess(results[1]['writerP95WallMs'], max(5, results[0]['writerP95WallMs'] * 2))


if __name__ == '__main__':
    unittest.main()

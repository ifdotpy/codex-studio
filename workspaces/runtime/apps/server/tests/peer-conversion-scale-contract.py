#!/usr/bin/env python3
"""Bounded conversion reads and measured global-lock time on a large isolated DB."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sys
import threading
import time
import unittest

sys.dont_write_bytecode = True
root = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
spec = importlib.util.spec_from_file_location('conversion_fixture', SERVER_TESTS_ROOT / 'peer-conversion-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_peer_conversion import INDEXES, busy_query, move_query, rooms_query, setup_indexes
from codex_peer_teams import manage


class MeasuredLock:
    def __init__(self, lock):
        self.lock = lock
        self.local = threading.local()
        self.holds = []

    def __enter__(self):
        self.lock.acquire()
        if not getattr(self.local, 'depth', 0):
            self.local.started = time.perf_counter()
        self.local.depth = getattr(self.local, 'depth', 0) + 1
        return self

    def __exit__(self, *args):
        self.local.depth -= 1
        if not self.local.depth:
            self.holds.append((time.perf_counter() - self.local.started) * 1000)
        self.lock.release()

    def __getattr__(self, key):
        return getattr(self.lock, key)


class Scale(fixture.Conversion):
    # Run only the scale contract here, rather than inheriting the smaller suite.
    def test_large_fixture_uses_indexes_and_holds_lock_under_200ms(self):
        with self.rt.db() as db:
            for table, count in (('tasks', 613_000), ('monitors', 34_000), ('event_meta', 52_097)):
                db.executemany('INSERT INTO runtime_' + table + ' VALUES (?,?)', (
                    (f'{table}:{i}', json.dumps({'id': f'{table}:{i}', 'agent': f'unrelated:{i % 1000}',
                        'status': 'completed', 'created': i, 'rootId': f'unrelated:{i % 1000}'}))
                    for i in range(count)))
            for table, count in (('requests', 196), ('tool_requests', 102_411), ('rules', 74),
                                 ('work', 1493), ('plans', 0), ('annotations', 0), ('complaints', 173)):
                db.executemany('INSERT INTO runtime_' + table + ' VALUES (?,?)', (
                    (f'{table}:{i}', json.dumps({'id': f'{table}:{i}', 'agent': f'unrelated:{i}',
                        'rootId': f'unrelated:{i}', 'leadId': f'unrelated:{i}', 'status': 'completed',
                        'outcome': 'applied', 'inFlight': False, 'text': 'Saved task evidence. ' * 400 if table == 'work' else ''})) for i in range(count)))
            db.executemany('INSERT INTO runtime_rooms VALUES (?,?)', (
                (f'room:{i}', json.dumps({'id': f'room:{i}', 'rootId': f'unrelated:{i}',
                    'members': [f'unrelated:{i}', f'unrelated:{i + 1}'], 'kind': 'private', 'updated': i}))
                for i in range(1000)))
            db.execute('CREATE TABLE IF NOT EXISTS runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL)')
            db.executemany('INSERT INTO runtime_account_transfers VALUES (?,?)', (
                (f'transfer:{i}', json.dumps({'id': f'transfer:{i}', 'leadId': f'unrelated:{i}',
                    'members': {}, 'status': 'completed'})) for i in range(2000)))
            # Build only the added indexes again, with populated tables. Existing
            # task, monitor, work and tool-request indexes are reused unchanged.
            for indexes in INDEXES.values():
                for name, _, _ in indexes:
                    db.execute('DROP INDEX IF EXISTS ' + name)
            db.commit()
            index_build_ms = {}
            for table, indexes in INDEXES.items():
                for name, expression, predicate in indexes:
                    started = time.perf_counter()
                    db.execute('CREATE INDEX ' + name + ' ON runtime_' + table + '(' + expression + ')'
                               + (' WHERE ' + predicate if predicate else ''))
                    db.commit()
                    elapsed = (time.perf_counter() - started) * 1000
                    index_build_ms[name] = round(elapsed, 3)
                    self.assertLess(elapsed, 3000, (name, elapsed))
            setup_indexes(db)
            counts = {table: db.execute('SELECT COUNT(*) FROM runtime_' + table).fetchone()[0]
                      for table in ('tasks', 'monitors', 'event_meta', 'work', 'tool_requests', 'rooms', 'requests', 'complaints', 'rules', 'plans', 'annotations')}
            ids = (self.a['id'], self.c['id'])
            queries = {f'busy:{table}': busy_query(table, ids)
                       for table in ('tasks', 'monitors', 'requests', 'tool_requests', 'rules')}
            queries.update({f'move:{table}': move_query(table, self.a['id'])
                           for table in ('work', 'plans', 'annotations', 'complaints', 'event_meta', 'rules')})
            queries['rooms:affected'] = rooms_query(ids, ids)
            queries['rooms:radio'] = rooms_query((), ids, radio=True)
            queries['transfers:pending'] = (
                "SELECT record FROM runtime_account_transfers WHERE json_extract(record,'$.status')='pending'", ())
            queries['agents:tree'] = (
                "SELECT record FROM runtime_agents WHERE json_extract(record,'$.rootId') IN (?,?)", ids)
            queries['events:pending'] = (
                "SELECT 1 FROM runtime_events WHERE agent IN (?,?) AND status IN ('pending','reserved','dispatching','uncertain') LIMIT 1", ids)
            queries['workspace:pending'] = (
                "SELECT record FROM runtime_workspace_operations WHERE json_extract(record,'$.agent')=? AND json_extract(record,'$.phase') IN ('provider_pending','provider_ready','local_mutation','recovery_required','capture_pending','capture_running')", (self.a['id'],))
            plans = {}
            for name, (sql, params) in queries.items():
                plan = [row['detail'] for row in db.execute('EXPLAIN QUERY PLAN ' + sql, params)]
                plans[name] = plan
                self.assertFalse(any('SCAN runtime_' in detail for detail in plan), (name, plan))
                self.assertTrue(any('USING INDEX' in detail or 'USING COVERING INDEX' in detail for detail in plan), (name, plan))
            # Include actual affected metadata so migration correctness remains observable.
            self.rt.put(db, 'event_meta', {'id': 'affected', 'rootId': self.a['id'], 'text': 'Saved metadata'})
        measured = MeasuredLock(self.rt.lock)
        self.rt.lock = measured
        result = manage(self.rt, self.body)
        max_hold = max(measured.holds)
        self.assertLess(max_hold, 200, measured.holds)
        with self.rt.db() as db:
            self.assertEqual(json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id='affected'").fetchone()[0])['rootId'], self.c['id'])
        print(json.dumps({'counts': counts, 'lockHoldMs': round(max_hold, 3), 'indexBuildMs': index_build_ms, 'queryPlans': plans}, indent=2))
        self.assertEqual(result['rootId'], self.c['id'])


if __name__ == '__main__':
    suite = unittest.TestSuite([Scale('test_large_fixture_uses_indexes_and_holds_lock_under_200ms')])
    result = unittest.TextTestRunner().run(suite)
    sys.exit(0 if result.wasSuccessful() else 1)

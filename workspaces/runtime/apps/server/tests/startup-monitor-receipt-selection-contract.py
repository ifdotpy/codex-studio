#!/usr/bin/env python3
"""Startup skips rule history before it fetches or decodes joined agent payloads."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import Runtime

spec = importlib.util.spec_from_file_location(
    'startup_selection_fixture', SERVER_TESTS_ROOT / 'supervisor-restore-selection-contract.py')
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class StartupMonitorReceiptSelectionContract(unittest.TestCase):
    setUp = f.SupervisorRestoreSelectionContract.setUp
    tearDown = f.SupervisorRestoreSelectionContract.tearDown
    agent = f.SupervisorRestoreSelectionContract.agent
    record = f.SupervisorRestoreSelectionContract.record
    store = f.SupervisorRestoreSelectionContract.store
    stored = f.SupervisorRestoreSelectionContract.stored

    def test_real_startup_never_decodes_historical_rule_agent_payloads(self):
        self.owner.update(status='paused', autoWake=False, restartRecovery=None, turnId=None)
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.owner)
        owner = self.agent('history-rule-owner', status='paused', autoWake=False,
                           restartRecovery=None, turnId=None, startupRuleAgent=True,
                           prompt='Fixture history ' * 8192)
        self.store('agents', owner)
        history = [self.record('rule-history-' + str(index), owner=owner,
                               status='completed', ruleId='saved-rule', reattachRecovery=None)
                   for index in range(256)]
        self.store('monitors', *history)
        ordinary = self.record('ordinary-terminal', status='completed', ruleId=None)
        self.store('monitors', ordinary)
        self.runtime.close()
        original_loads = json.loads

        def guarded_loads(value, *args, **kwargs):
            if isinstance(value, str) and '"startupRuleAgent": true' in value:
                frame = sys._getframe(1)
                while frame:
                    if frame.f_code is Runtime.recover_monitor_receipts.__code__:
                        raise AssertionError('Startup decoded an excluded rule agent payload')
                    frame = frame.f_back
            return original_loads(value, *args, **kwargs)

        with patch.object(Runtime, 'schedule', lambda _runtime: None), \
                patch('codex_runtime.json.loads', side_effect=guarded_loads):
            restarted = Runtime.__new__(Runtime)
            try:
                Runtime.__init__(restarted, Path(self.temp.name), f.NoNativeServer)
            except BaseException:
                for pool in (restarted.pool, restarted.tool_pool, restarted.coordination_pool,
                             restarted.recovery_pool):
                    pool.shutdown(wait=True, cancel_futures=True)
                restarted.lease.close()
                raise
            self.runtime = restarted
            self.addCleanup(restarted.close)

        with self.runtime.read_db() as db:
            events = db.execute(
                "SELECT id,status,created FROM runtime_events WHERE kind='monitor_exit'").fetchall()
        self.assertEqual([(r['id'], r['status'], r['created']) for r in events],
                         [('monitor:' + ordinary['id'], 'cancelled', ordinary['finished'])])
        self.assertEqual(self.stored('monitors', history[0]['id']), history[0])
        stored_owner = self.stored('agents', owner['id'])
        for field in ('id', 'status', 'autoWake', 'epoch', 'threadId', 'turnId', 'prompt'):
            self.assertEqual(stored_owner[field], owner[field])

    def test_selection_matches_python_rule_id_truthiness_and_uses_status_index(self):
        false_values = [None, False, 0, 0.0, '', [], {}]
        true_values = ['saved-rule', '0', 'false', '[]', '{}', True, 1, -1, [0], {'id': None}]
        records = []
        expected = []
        for index, value in enumerate(false_values + true_values):
            record = self.record('truthiness-' + str(index), status='completed', ruleId=value)
            records.append(record)
            if not value:
                expected.append(record['id'])
        missing = self.record('missing-rule-id', status='failed')
        records.append(missing)
        expected.append(missing['id'])
        self.store('monitors', *records)
        queries = []
        fetched_rules = []
        original_exit = self.runtime._monitor_exit_event

        def observed_exit(db, agent, monitor, **kwargs):
            if monitor.get('ruleId'):
                fetched_rules.append(monitor['id'])
            return original_exit(db, agent, monitor, **kwargs)

        with self.runtime.db() as db, \
                patch.object(self.runtime, '_monitor_exit_event', side_effect=observed_exit):
            db.set_trace_callback(queries.append)
            restored = self.runtime.recover_monitor_receipts(db)
            db.set_trace_callback(None)
            selection = next(q for q in queries if q.startswith('SELECT m.id FROM runtime_monitors'))
            plan = [r[3] for r in db.execute('EXPLAIN QUERY PLAN ' + selection)]

        self.assertEqual(set(restored), set(expected))
        self.assertEqual(fetched_rules, [], 'Excluded monitors must not fetch joined agent payloads')
        self.assertTrue(any('SEARCH m USING INDEX runtime_monitor_status' in step for step in plan), plan)
        for record in records:
            self.assertEqual(self.stored('monitors', record['id']), record)

    def test_ordinary_terminal_receipts_keep_ids_epochs_status_and_no_wake(self):
        owner = self.agent('ordinary-owner', status='waiting', autoWake=True,
                           restartRecovery=None, turnId=None)
        self.store('agents', owner)
        records = [self.record('ordinary-' + status, owner=owner, status=status,
                               ruleId=None, exitCode=0 if status == 'completed' else None)
                   for status in ('completed', 'failed', 'cancelled', 'lost', 'running')]
        self.store('monitors', *records)
        keys = [record['id'] for record in records]
        with self.runtime.db() as db:
            restored = self.runtime.recover_monitor_receipts(db, keys=keys)
            self.assertEqual(set(restored), set(keys[:-1]))
            self.assertEqual(self.runtime.recover_monitor_receipts(db, keys=keys), [])
            events = db.execute("SELECT * FROM runtime_events WHERE kind='monitor_exit'").fetchall()
            self.assertEqual(self.runtime.recover_monitor_receipts(db, keys=[]), [])

        by_id = {record['id']: record for record in records}
        self.assertEqual(len(events), 4)
        for event in events:
            monitor = by_id[event['id'].removeprefix('monitor:')]
            self.assertEqual(event['agent'], owner['id'])
            self.assertEqual(event['status'], 'cancelled')
            self.assertEqual(event['epoch'], monitor['epoch'])
            self.assertEqual(event['created'], monitor['finished'])
            self.assertEqual(json.loads(event['text'])['status'], monitor['status'])
        self.assertEqual(self.stored('agents', owner['id']), owner)
        for record in records:
            self.assertEqual(self.stored('monitors', record['id']), record)


if __name__ == '__main__':
    unittest.main()

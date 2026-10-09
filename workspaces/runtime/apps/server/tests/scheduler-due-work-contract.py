#!/usr/bin/env python3
"""Only due watches and stalled monitors decode payloads; owner holds stay exact."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import Runtime
from codex_rules import RulesMixin

spec = importlib.util.spec_from_file_location('rule_due_fixture',
    Path(__file__).with_name('rules-owner-cache-cpu-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class DueWorkContract(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='scheduler-due-work-')
        self.addCleanup(self.directory.cleanup)
        self.rt = fixture.RuleFixture(self.directory.name)
        self.rt.add_owner()

    def test_future_and_event_watches_skip_owner_reads_but_pause_stopped_owner(self):
        self.rt.add_rule('future')
        self.rt.add_rule('event', kind='event')
        self.rt.rules_tick()
        self.assertEqual(self.rt.owner_loads, [])
        with self.rt.db() as db:
            a = self.rt.agent('owner', db)
            a['autoWake'] = False
            self.rt.put(db, 'agents', a)
        self.rt.owner_loads.clear()
        self.rt.rules_tick()
        self.assertEqual([self.rt.load('rules', key)['status'] for key in ('future', 'event')], ['paused', 'paused'])
        self.assertEqual(self.rt.pool.launched, [])

    def test_epoch_change_selects_future_watch_and_exact_restart_hold_preserves_it(self):
        self.rt.add_rule('future')
        with self.rt.db() as db:
            a = self.rt.agent('owner', db)
            a.update(autoWake=False, restartRecovery=dict(stage='pending', autoWake=True, epoch=1,
                accountKey=a['accountKey'], threadId=a['threadId']))
            self.rt.put(db, 'agents', a)
        self.rt.rules_tick()
        self.assertEqual(self.rt.load('rules', 'future')['status'], 'active')
        with self.rt.db() as db:
            a = self.rt.agent('owner', db)
            a['epoch'] = 2
            self.rt.put(db, 'agents', a)
        self.rt.rules_tick()
        self.assertEqual(self.rt.load('rules', 'future')['status'], 'paused')

    def test_due_watch_reserves_exactly_once(self):
        self.rt.add_rule('future')
        self.rt.add_rule('due', nextAt=100)
        self.rt.rules_tick()
        self.rt.rules_tick()
        self.assertEqual([r['id'] for r in self.rt.pool.launched], ['due'])
        self.assertEqual(self.rt.load('rules', 'due')['checks'], 1)

    def test_file_stall_is_due_even_with_future_normal_check(self):
        path = Path(self.directory.name) / 'watched.txt'
        path.write_text('unchanged')
        now = time.time()
        self.rt.add_rule('stall', kind='file', path=str(path), fingerprint=RulesMixin.file_fingerprint(str(path)),
            nextAt=now + 3600, stallTimeoutSeconds=10, fileActivityAt=now - 30,
            fileGeneration=1, stallWakeGeneration=-1)
        self.rt.rules_tick()
        self.rt.rules_tick()
        self.assertEqual(self.rt.load('rules', 'stall')['stallWakeGeneration'], 1)
        with self.rt.read_db() as db:
            self.assertEqual([row[0] for row in db.execute('SELECT id FROM runtime_events')], ['rule-stall:stall:1'])

    def monitor(self, key, **fields):
        m = dict(id=key, agent='owner', epoch=1, status='running', created=time.time(),
                 activityGeneration=0, stallWakeGeneration=-1, stallTimeoutSeconds=60, tail='x' * 4096)
        m.update(fields)
        with self.rt.db() as db:
            self.rt.put(db, 'monitors', m)
        return m

    def test_monitor_decodes_only_due_unwoken_generation_and_preserves_probe_deadline(self):
        now = time.time()
        self.monitor('future', activityAt=now)
        self.monitor('disabled', activityAt=now - 120, stallTimeoutSeconds=0)
        self.monitor('woken', activityAt=now - 120, stallWakeGeneration=0)
        self.monitor('probe', activityAt=now - 120, stallProbeGeneration=0, stallProbeStarted=now - 80)
        self.monitor('due', activityAt=now - 120)
        decoded = []
        original = json.loads
        def load(raw):
            value = original(raw)
            if isinstance(value, dict) and 'stallTimeoutSeconds' in value:
                decoded.append(value['id'])
            return value
        with patch('codex_runtime.json.loads', side_effect=load):
            Runtime.monitors_tick(self.rt)
            Runtime.monitors_tick(self.rt)
        self.assertEqual(decoded, ['due'])
        with self.rt.read_db() as db:
            self.assertEqual([row[0] for row in db.execute('SELECT id FROM runtime_events')], ['monitor-stall:due:0'])
        with self.rt.db() as db:
            m = self.rt.load('monitors', 'probe')
            m['stallProbeStarted'] = now - 100
            self.rt.put(db, 'monitors', m)
        Runtime.monitors_tick(self.rt)
        self.assertEqual(self.rt.load('monitors', 'probe')['stallWakeGeneration'], 0)

    def test_large_future_roster_selects_only_due_rule_and_owner(self):
        self.rt.add_owner('due-owner')
        with self.rt.db() as db:
            for index in range(512):
                rule = dict(id=str(index), agent='owner', epoch=1, status='active', kind='interval',
                    inFlight=False, name=str(index), text='x' * 4096, command='', created=time.time(),
                    checks=0, wakes=0, nextAt=time.time() + 3600, intervalSeconds=60)
                self.rt.put(db, 'rules', rule)
        self.rt.add_rule('due', 'due-owner', nextAt=0)
        decoded = []
        original = json.loads
        def load(raw):
            value = original(raw)
            if isinstance(value, dict) and value.get('kind') == 'interval':
                decoded.append(value['id'])
            return value
        began = time.thread_time()
        with patch('codex_rules.json.loads', side_effect=load):
            self.rt.rules_tick()
        elapsed = (time.thread_time() - began) * 1000
        self.assertEqual(decoded, ['due', 'due'])
        self.assertEqual([key for _, key, _ in self.rt.owner_loads], ['due-owner', 'due-owner'])
        due_ids = [r['id'] for r in self.rt.pool.launched]
        self.assertEqual(due_ids, ['due'])
        print(json.dumps({'fixture': '512-future-watches-one-due', 'dueSelectionCpuMs': round(elapsed, 3),
                          'dueOwnerReads': len(self.rt.owner_loads),
                          'correctnessSha256': hashlib.sha256(json.dumps(due_ids).encode()).hexdigest()}))


if __name__ == '__main__':
    unittest.main()

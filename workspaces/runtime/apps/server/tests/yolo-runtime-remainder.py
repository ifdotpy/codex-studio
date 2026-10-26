#!/usr/bin/env python3
"""Team permission choices and native request parameters. No model calls."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('defaults', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from studio_api.testing import read_runtime_state


class Yolo(f.WorkerDefaults):
    # Run only this class's cases, not the imported defaults cases.
    def test_turn_overrides_loaded_native_policy_and_monitor_policy(self):
        lead = self.runtime.prepare(self.lead)
        self.assertTrue(self.runtime.monitor_auto_approved(lead))
        self.settings(yolo_mode=False)
        self.assertFalse(self.runtime.monitor_auto_approved(self.runtime.agent(lead['id'])))

    def test_monitor_uses_selected_policy_instead_of_stale_profile(self):
        lead = self.runtime.prepare(self.lead)
        with self.runtime.lock, self.runtime.db() as db:
            lead['profile'] = {'id':'stale-read-only'}
            self.runtime.put(db, 'agents', lead)
        monitor = self.runtime.monitor(lead['id'], {'command':'echo test'}, approved=True)
        f.f.eventually(lambda: any(m == 'command/exec' for m,p in self.runtime.server.calls))
        command = [p for m,p in self.runtime.server.calls if m == 'command/exec'][-1]
        self.assertEqual(command['sandboxPolicy'], {'type':'dangerFullAccess'})
        self.assertNotIn('permissionProfile', command)
        self.runtime.server.gate.set()
        f.f.eventually(lambda: read_runtime_state(self.runtime)['monitors'][0]['status'] not in {'running','starting'})

    def test_rule_rechecks_permission_after_mode_change(self):
        rule = self.runtime.rules({'agent':self.lead['id'], 'name':'Check', 'command':'echo ready'})
        with self.runtime.lock, self.runtime.db() as db:
            rule.update(inFlight=True, checks=1)
            self.runtime.put(db, 'rules', rule)
        self.settings(yolo_mode=False)
        monitor = self.runtime.monitor(self.lead['id'], {'command':'echo ready'},
                                       approved=True, rule=rule)
        self.assertEqual(monitor['status'], 'approval')
        self.assertTrue(read_runtime_state(self.runtime)['requests'])

    def test_restore_blocks_permission_changes(self):
        worker = self.worker()
        with self.runtime.lock, self.runtime.db() as db:
            worker['workspaceOperation'] = 'restore'
            self.runtime.put(db, 'agents', worker)
        with self.assertRaisesRegex(ValueError, 'workspace operations'):
            self.settings(yolo_mode=False)
        self.assertIs(self.runtime.agent(self.lead['id'])['yoloMode'], True)

    def test_invalid_and_busy_changes_are_atomic(self):
        worker = self.worker()
        with self.runtime.lock, self.runtime.db() as db:
            worker.update(inFlight=True, status='running')
            self.runtime.put(db, 'agents', worker)
        with self.assertRaisesRegex(ValueError, 'every team turn'):
            self.settings(yolo_mode=False)
        self.assertIs(self.runtime.agent(self.lead['id'])['yoloMode'], True)
        with self.runtime.lock, self.runtime.db() as db:
            worker.update(inFlight=False, status='waiting')
            self.runtime.put(db, 'agents', worker)
            self.runtime.put(db, 'monitors', {'id':'watch', 'agent':worker['id'], 'status':'running'})
        with self.assertRaisesRegex(ValueError, 'monitors'):
            self.settings(yolo_mode=False)
        self.assertIs(self.runtime.agent(worker['id'])['yoloMode'], True)

    def test_legacy_permissions_unchanged_and_mode_survives_restart(self):
        legacy = dict(self.lead)
        legacy.pop('yoloMode')
        params = self.runtime.new_thread_params(legacy)
        self.assertNotIn('approvalPolicy', params)
        self.assertNotIn('sandbox', params)


if __name__ == '__main__':
    suite = unittest.TestSuite(Yolo(name) for name in Yolo.__dict__ if name.startswith('test_'))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())

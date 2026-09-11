#!/usr/bin/env python3
"""Team permission choices and native request parameters. No model calls."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('defaults', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class Yolo(f.WorkerDefaults):
    # Run only this class's cases, not the imported defaults cases.
    def test_turn_overrides_loaded_native_policy_and_monitor_policy(self):
        lead = self.runtime.prepare(self.lead)
        # Fake resume retains the old policy, like a subscribed native thread.
        self.assertEqual(lead['approvalPolicy'], 'on-request')
        self.assertTrue(self.runtime.monitor_auto_approved(lead))
        self.runtime.send(lead['id'], 'Run with the selected permissions')
        self.runtime.dispatch()
        f.f.eventually(lambda: any(m == 'turn/start' for m, p in self.runtime.server.calls))
        turn = [p for m, p in self.runtime.server.calls if m == 'turn/start'][-1]
        self.assertEqual(turn['approvalPolicy'], 'never')
        self.assertEqual(turn['sandboxPolicy'], {'type': 'dangerFullAccess'})
        f.f.eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        active = self.runtime.agent(lead['id'])
        self.runtime.server.complete(active['threadId'], active['turnId'])
        f.f.eventually(lambda: not self.runtime.agent(lead['id'])['inFlight'])
        self.settings(yolo_mode=False)
        self.runtime.send(lead['id'], 'Now use workspace permissions')
        self.runtime.dispatch()
        f.f.eventually(lambda: len([p for m,p in self.runtime.server.calls if m == 'turn/start']) == 2)
        turn = [p for m,p in self.runtime.server.calls if m == 'turn/start'][-1]
        self.assertEqual(turn['approvalPolicy'], 'on-request')
        self.assertEqual(turn['sandboxPolicy']['type'], 'workspaceWrite')
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
        f.f.eventually(lambda: self.runtime.snapshot()['monitors'][0]['status'] not in {'running','starting'})

    def test_rule_rechecks_permission_after_mode_change(self):
        rule = self.runtime.rules({'agent':self.lead['id'], 'name':'Check', 'command':'echo ready'})
        with self.runtime.lock, self.runtime.db() as db:
            rule.update(inFlight=True, checks=1)
            self.runtime.put(db, 'rules', rule)
        self.settings(yolo_mode=False)
        monitor = self.runtime.monitor(self.lead['id'], {'command':'echo ready'},
                                       approved=True, rule=rule)
        self.assertEqual(monitor['status'], 'approval')
        self.assertTrue(self.runtime.snapshot()['requests'])

    def test_restore_blocks_permission_changes(self):
        worker = self.worker()
        with self.runtime.lock, self.runtime.db() as db:
            worker['workspaceOperation'] = 'restore'
            self.runtime.put(db, 'agents', worker)
        with self.assertRaisesRegex(ValueError, 'workspace operations'):
            self.settings(yolo_mode=False)
        self.assertIs(self.runtime.agent(self.lead['id'])['yoloMode'], True)

    def test_yolo_defaults_and_worker_inheritance(self):
        self.assertIs(self.lead['yoloMode'], True)
        worker = self.worker()
        self.assertIs(worker['yoloMode'], True)
        for agent in [self.lead, worker]:
            params = self.runtime.new_thread_params(agent)
            self.assertEqual(params['approvalPolicy'], 'never')
            self.assertEqual(params['sandbox'], 'danger-full-access')
        with self.assertRaisesRegex(ValueError, 'Only the user'):
            self.worker(yolo_mode=True)
        with self.assertRaisesRegex(ValueError, 'Only a lead'):
            self.runtime.conversation_settings(worker['id'], {'yolo_mode': True})

    def test_off_changes_existing_workers_and_resumes_explicit_policy(self):
        worker = self.runtime.prepare(self.worker())
        self.runtime.loaded.add(self.lead['id'])
        self.settings(yolo_mode=False)
        self.assertNotIn(worker['id'], self.runtime.loaded)
        self.assertNotIn(self.lead['id'], self.runtime.loaded)
        lead = self.runtime.agent(self.lead['id'])
        self.assertEqual(self.runtime.new_thread_params(lead)['sandbox'], 'workspace-write')
        self.assertEqual(self.runtime.new_thread_params(lead)['approvalPolicy'], 'on-request')
        resumed = self.runtime.prepare(worker)
        calls = [p for m, p in self.runtime.server.calls if m == 'thread/resume']
        self.assertEqual(calls[-1]['threadId'], worker['threadId'])
        self.assertEqual(calls[-1]['sandbox'], 'read-only')
        self.assertEqual(calls[-1]['approvalPolicy'], 'on-request')
        self.assertIs(resumed['yoloMode'], False)
        self.assertIs(self.worker()['yoloMode'], False)

    def test_invalid_and_busy_changes_are_atomic(self):
        for value in ['true', 1, None]:
            with self.assertRaisesRegex(ValueError, 'boolean'):
                self.settings(yolo_mode=value)
            with self.assertRaisesRegex(ValueError, 'boolean'):
                self.runtime.new_lead({'yolo_mode': value})
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
        self.settings(yolo_mode=False)
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.root, f.f.FakeServer)
        self.assertIs(self.runtime.agent(self.lead['id'])['yoloMode'], False)

    def test_new_lead_reuse_and_request_identity(self):
        lead = self.runtime.new_lead({'cwd': str(self.root), 'yolo_mode': False})
        reused = self.runtime.new_lead({'previous': lead['id'], 'yolo_mode': True})
        self.assertEqual(lead['id'], reused['id'])
        self.assertIs(reused['yoloMode'], True)
        request = {'id': f.uuid.uuid4().hex, 'cwd':str(self.root), 'yolo_mode': False}
        made = self.runtime.new_lead(request)
        self.assertIs(made['yoloMode'], False)
        with self.assertRaisesRegex(ValueError, 'different settings'):
            self.runtime.new_lead({**request, 'yolo_mode': True})


if __name__ == '__main__':
    suite = unittest.TestSuite(Yolo(name) for name in Yolo.__dict__ if name.startswith('test_'))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())

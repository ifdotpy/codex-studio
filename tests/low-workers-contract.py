#!/usr/bin/env python3
"""Low-worker alerts use durable events and never poll a model."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('workspace_fixture', Path(__file__).with_name('workspace-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class LowWorkersContract(unittest.TestCase):
    setUp = fixture.WorkspaceContract.setUp
    tearDown = fixture.WorkspaceContract.tearDown
    lead = fixture.WorkspaceContract.lead
    worker = fixture.WorkspaceContract.worker
    agent_update = fixture.WorkspaceContract.agent_update
    events = fixture.WorkspaceContract.events

    def alert(self, lead, **options):
        return self.runtime.rules({'agent': lead['id'], 'id': 'shortage', 'name': 'Team capacity',
                                   'kind': 'low_workers', **options})

    def tick(self, now):
        with patch('codex_rules.time.time', return_value=now):
            self.runtime.rules_tick()

    def rule(self):
        return next(r for r in self.runtime.rules()['rules'] if r['id'] == 'shortage')

    def test_default_duration_one_ping_and_recovery(self):
        lead = self.agent_update(self.lead(), autoWake=True)
        rule = self.alert(lead)
        self.assertEqual((rule['minimumWorkers'], rule['durationMinutes']), (8, 30))
        self.tick(100)
        self.tick(1900)
        self.assertEqual(self.events(lead, 'rule'), [])
        self.tick(1901)
        events = self.events(lead, 'rule')
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]['agent'], lead['id'])
        self.assertEqual(events[0]['status'], 'pending')
        self.assertEqual(json.loads(json.loads(events[0]['text'])['output'])['activeSubagents'], 0)
        self.tick(9999)
        self.assertEqual(len(self.events(lead, 'rule')), 1)
        self.assertIsNone(self.runtime.server)
        workers = [self.agent_update(self.worker(lead), status='running') for _ in range(8)]
        self.tick(10000)
        self.assertIsNone(self.rule()['lowSince'])
        for worker in workers:
            self.agent_update(worker, status='completed')
        self.tick(10001)
        self.tick(11802)
        self.assertEqual(len(self.events(lead, 'rule')), 2)

    def test_recovery_before_alert_restarts_the_duration(self):
        lead = self.agent_update(self.lead(), autoWake=True)
        worker = self.worker(lead)
        self.alert(lead, minimumWorkers=1, durationMinutes=1)
        self.tick(100)
        self.agent_update(worker, status='running')
        self.tick(150)
        self.agent_update(worker, status='completed')
        self.tick(151)
        self.tick(200)
        self.assertEqual(self.events(lead, 'rule'), [])
        self.tick(212)
        self.assertEqual(len(self.events(lead, 'rule')), 1)

    def test_event_and_latch_roll_back_together(self):
        lead = self.agent_update(self.lead(), autoWake=True)
        self.alert(lead, durationMinutes=1)
        self.tick(100)
        finish = self.runtime.rule_finished
        def fail_after_enqueue(*args, **kwargs):
            finish(*args, **kwargs)
            raise RuntimeError('fixture transaction failure')
        with patch.object(self.runtime, 'rule_finished', side_effect=fail_after_enqueue):
            with self.assertRaises(RuntimeError):
                self.tick(161)
        self.assertEqual(self.events(lead, 'rule'), [])
        self.assertFalse(self.rule()['alerted'])
        self.tick(162)
        self.tick(163)
        self.assertEqual(len(self.events(lead, 'rule')), 1)

    def test_counts_team_workers_and_commands_once(self):
        lead = self.agent_update(self.lead(), autoWake=True)
        a, b, queued, deleted, panel = [self.worker(lead) for _ in range(5)]
        self.agent_update(a, status='starting')
        self.agent_update(queued, status='queued')
        self.agent_update(deleted, status='running', deletedAt=1)
        other = self.lead('Other')
        self.agent_update(self.worker(other), status='running')
        with self.runtime.lock, self.runtime.db() as db:
            for key, agent, extra in [('one', a, {}), ('two', b, {}), ('three', b, {}), ('panel', panel, {'panelFeed': {'id': 'feed'}})]:
                self.runtime.put(db, 'monitors', {'id': key, 'agent': agent['id'], 'status': 'running', **extra})
        self.alert(lead, minimumWorkers=2, durationMinutes=1)
        self.tick(100)
        self.assertEqual(self.rule()['activeWorkers'], 2)
        self.assertIsNone(self.rule()['lowSince'])
        self.agent_update(a, status='completed')
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("DELETE FROM runtime_monitors WHERE id='one'")
        self.tick(101)
        self.tick(162)
        self.assertEqual(len(self.events(lead, 'rule')), 1)
        for worker in (a, b, queued, deleted, panel):
            self.assertEqual(self.events(worker, 'rule'), [])

    def test_same_save_and_resume_do_not_reset_timer_or_latch(self):
        lead = self.agent_update(self.lead(), autoWake=True)
        self.alert(lead, minimumWorkers=2, durationMinutes=1)
        self.tick(100)
        self.alert(lead, minimumWorkers=2, durationMinutes=1)
        self.runtime.rules({'agent': lead['id'], 'id': 'shortage', 'action': 'resume'})
        self.assertEqual(self.rule()['lowSince'], 100)
        self.tick(161)
        self.alert(lead, minimumWorkers=2, durationMinutes=1)
        self.tick(999)
        self.assertEqual(len(self.events(lead, 'rule')), 1)
        self.runtime.rules({'agent': lead['id'], 'id': 'shortage', 'action': 'pause'})
        self.tick(2000)
        self.assertEqual(len(self.events(lead, 'rule')), 1)
        self.runtime.rules({'agent': lead['id'], 'id': 'shortage', 'action': 'resume'})
        self.tick(3000)
        self.tick(3061)
        self.assertEqual(len(self.events(lead, 'rule')), 2)

    def test_restart_does_not_count_offline_time_or_repeat_sent_alert(self):
        lead = self.agent_update(self.lead(), autoWake=True)
        self.alert(lead, durationMinutes=1)
        self.tick(100)
        self.runtime.close()
        self.runtime = fixture.ControlledRuntime(self.state, fixture.WorkspaceServer)
        self.tick(900)
        self.assertEqual(self.events(lead, 'rule'), [])
        self.tick(961)
        self.assertEqual(len(self.events(lead, 'rule')), 1)
        self.runtime.close()
        self.runtime = fixture.ControlledRuntime(self.state, fixture.WorkspaceServer)
        self.tick(2000)
        self.tick(3000)
        self.assertEqual(len(self.events(lead, 'rule')), 1)

    def test_stop_and_failure_hold_preserved(self):
        lead = self.agent_update(self.lead(), autoWake=True)
        self.alert(lead, durationMinutes=1)
        self.tick(100)
        self.agent_update(lead, autoWake=False, status='paused', epoch=2)
        self.tick(1000)
        self.assertEqual(self.events(lead, 'rule'), [])
        self.assertEqual(self.rule()['status'], 'paused')
        self.agent_update(lead, autoWake=True, nativeFailureHold=True, status='failed')
        self.runtime.rules({'agent': lead['id'], 'id': 'shortage', 'action': 'resume'})
        self.tick(2000)
        self.tick(2061)
        self.assertEqual(self.runtime.agent(lead['id'])['status'], 'failed')
        self.assertEqual(len(self.events(lead, 'rule')), 1)

    def test_validation_and_worker_cannot_own_or_change_alert(self):
        lead = self.agent_update(self.lead(), autoWake=True)
        worker = self.worker(lead)
        for field in ('minimumWorkers', 'durationMinutes'):
            for value in (0, -1, True, 1.5, '8', 525601):
                with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                    self.alert(lead, **{field: value})
        with self.assertRaises(ValueError):
            self.alert(worker)
        with self.assertRaises(ValueError):
            self.alert(lead, command='echo unexpected')
        self.alert(lead)
        with self.assertRaises(ValueError):
            self.runtime.rules({'id': 'shortage', 'action': 'pause'}, actor=worker['id'])
        self.assertEqual(self.rule()['status'], 'active')


if __name__ == '__main__':
    unittest.main()

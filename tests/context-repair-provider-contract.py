#!/usr/bin/env python3
"""Provider boundaries for Codex rollout repair after a portable transfer."""
import copy
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'context_repair_fixture', Path(__file__).with_name('context-repair-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
repair = fixture.repair


class ContextRepairProvider(unittest.TestCase):
    setUp = fixture.ContextRepair.setUp
    tearDown = fixture.ContextRepair.tearDown
    agent_update = fixture.ContextRepair.agent_update
    lead = fixture.ContextRepair.lead
    write_records = fixture.ContextRepair.write_records
    forks = fixture.ContextRepair.forks

    def run_repair(self, entry, agent):
        if entry == 'before_start':
            return repair.repair_before_start(self.runtime, agent)
        return repair.repair_idle(self.runtime, agent['id'])

    def test_claude_keeps_verified_events_and_history_without_native_access(self):
        agent = self.agent_update(self.a, provider='claude')
        source = self.path.read_bytes()
        with self.runtime.db() as db:
            self.assertEqual([e['id'] for e in repair.verified_events(db, agent)],
                             [self.event['id']])
        for entry in ('idle', 'before_start'):
            with self.subTest(entry=entry), patch.object(
                    self.runtime, 'connect', side_effect=AssertionError('Claude must not fork a Codex rollout')):
                self.assertEqual(self.run_repair(entry, agent), agent)
        self.assertEqual(self.runtime.agent(agent['id']), agent)
        self.assertEqual(self.path.read_bytes(), source)
        with self.runtime.db() as db:
            row = db.execute('SELECT text,status,turn_id FROM runtime_events WHERE id=?',
                             (self.event['id'],)).fetchone()
        self.assertEqual(tuple(row), (self.text, 'delivered', 'turn'))
        self.assertEqual(self.server.calls, [])

    def test_claude_before_start_does_not_wait_on_a_codex_callback_barrier(self):
        for phase in ('unchanged', 'completed'):
            with self.subTest(phase=phase):
                receipt = {'id': 'old-codex-repair', 'phase': phase}
                agent = self.agent_update(self.a, provider='claude', contextRepair=receipt)
                with patch.object(self.runtime, 'connect', side_effect=AssertionError('No Codex barrier for Claude')):
                    self.assertEqual(repair.repair_before_start(self.runtime, agent), agent)
                self.assertEqual(self.runtime.agent(agent['id'])['contextRepair'], receipt)

    def test_before_start_uses_the_current_provider(self):
        stale = copy.deepcopy(self.a)
        current = self.agent_update(self.a, provider='claude')
        with patch.object(self.runtime, 'connect', side_effect=AssertionError('The provider changed to Claude')):
            self.assertEqual(repair.repair_before_start(self.runtime, stale), current)

    def test_claude_active_and_unknown_receipts_still_block(self):
        for entry in ('idle', 'before_start'):
            for phase in sorted(repair.ACTIVE):
                with self.subTest(entry=entry, phase=phase):
                    receipt = {'id': 'exact-native-receipt', 'phase': phase}
                    agent = self.agent_update(self.a, provider='claude', contextRepair=receipt)
                    with self.assertRaisesRegex(ValueError, 'exact native receipt: exact-native-receipt'):
                        self.run_repair(entry, agent)
                    self.assertEqual(self.runtime.agent(agent['id']), agent)
        self.assertEqual(self.server.calls, [])

    def test_claude_context_wait_still_blocks(self):
        agent = self.agent_update(self.a, provider='claude', contextRepairWait={
            'error': 'Context repair waits for its pending input receipts'})
        for entry in ('idle', 'before_start'):
            with self.subTest(entry=entry):
                with self.assertRaisesRegex(ValueError, 'pending input receipts'):
                    self.run_repair(entry, agent)
                self.assertEqual(self.runtime.agent(agent['id']), agent)
        self.assertEqual(self.server.calls, [])

    def test_codex_still_requires_a_saved_rollout_path(self):
        call = self.server.call

        def without_rollout(method, params, timeout=10):
            result = call(method, params, timeout)
            if method == 'thread/read':
                result['thread'].pop('path', None)
            return result

        with patch.object(self.server, 'call', side_effect=without_rollout):
            with self.assertRaisesRegex(ValueError, 'no saved rollout path'):
                repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(self.forks(), [])
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(current['threadId'], self.tid)
        self.assertEqual(current['contextRepair']['phase'], 'failed')

    def test_codex_still_repairs_verified_monitor_events(self):
        source = self.path.read_bytes()
        result = repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(result['contextRepair']['phase'], 'completed')
        self.assertNotEqual(result['threadId'], self.tid)
        self.assertEqual(len(self.forks()), 1)
        self.assertEqual(self.path.read_bytes(), source)

    def failed_claude(self):
        error = 'The native context has no saved rollout path'
        attempt = {'id': 'failed-start', 'submitted': False,
                   'epoch': self.a['epoch'], 'accountKey': self.a['accountKey'],
                   'events': ['failed-input-1', 'failed-input-2']}
        agent = self.agent_update(self.a, provider='claude', status='failed',
                                  autoWake=True, inFlight=False, error=error,
                                  startAttempt=attempt, contextRepairWait=None)
        receipt = {'id': 'failed-read-only-repair', 'agent': agent['id'],
                   'phase': 'failed', 'error': error,
                   'source': {key: agent[key] for key in ('id', 'accountKey', 'epoch', 'threadId')},
                   'settings': self.runtime.preparation_settings(agent)}
        receipt['source']['attemptId'] = attempt['id']
        agent = self.agent_update(agent, contextRepair=receipt)
        with self.runtime.db() as db:
            for key in [*attempt['events'], 'unrelated-failed-input']:
                db.execute('INSERT OR REPLACE INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                           (key, agent['id'], 'user', 'Preserve ' + key, 'failed',
                            2, agent['epoch'], None, error))
        return agent

    def recover(self):
        with self.runtime.lock, self.runtime.db() as db:
            repair.recover_context_failures(self.runtime, db, [self.runtime.agent(self.a['id'], db)])
        return self.runtime.agent(self.a['id'])

    def test_exact_failed_claude_batch_is_restored_once_with_its_receipt(self):
        original = self.failed_claude()
        source = self.path.read_bytes()
        result = self.recover()
        self.assertEqual(result['status'], 'queued')
        self.assertEqual(result['threadId'], original['threadId'])
        self.assertEqual(result['contextRepair'], original['contextRepair'])
        self.assertEqual(result['startAttempt'], original['startAttempt'])
        wait = result['contextRepairWait']
        self.assertTrue(wait['historicalFailureRecovered'])
        self.assertEqual(wait['events'], original['startAttempt']['events'])
        self.assertEqual(wait['source'], original['contextRepair']['source'])
        with self.runtime.db() as db:
            for key in original['startAttempt']['events']:
                row = db.execute('SELECT text,status,turn_id,error FROM runtime_events WHERE id=?', (key,)).fetchone()
                self.assertEqual(tuple(row), ('Preserve ' + key, 'pending', None, None))
            row = db.execute('SELECT status,error FROM runtime_events WHERE id=?', ('unrelated-failed-input',)).fetchone()
            self.assertEqual(tuple(row), ('failed', original['error']))
            row = db.execute('SELECT text,status,turn_id FROM runtime_events WHERE id=?', (self.event['id'],)).fetchone()
            self.assertEqual(tuple(row), (self.text, 'delivered', 'turn'))
        self.assertEqual(self.recover(), result)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(original['id'], db)
            current['contextRepairWait']['nextCheckAt'] = 0
            self.runtime.put(db, 'agents', current)
            claimed = repair.claim_context_wait(self.runtime, db, current)
            self.assertEqual(claimed['kind'], 'turn')
            self.assertEqual(claimed['attempt'], original['startAttempt'])
            self.assertEqual([row['id'] for row in claimed['rows']], original['startAttempt']['events'])
            for key in original['startAttempt']['events']:
                row = db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?', (key,)).fetchone()
                self.assertEqual(tuple(row), ('reserved', None))
        current = self.runtime.agent(original['id'])
        self.assertNotIn('contextRepairWait', current)
        self.assertEqual(current['status'], 'starting')
        self.assertEqual(current['contextRepair'], original['contextRepair'])
        with patch.object(self.runtime, 'connect', side_effect=AssertionError('Claude must not repeat Codex repair')):
            self.assertEqual(repair.repair_before_start(self.runtime, current), current)
        self.assertEqual(self.path.read_bytes(), source)
        self.assertEqual(self.server.calls, [])

    def test_recovery_rejects_submitted_changed_or_unproven_failures(self):
        cases = [
            ('provider', lambda a: a.update(provider='codex')),
            ('submitted', lambda a: a['startAttempt'].update(submitted=True)),
            ('native-turn', lambda a: a['startAttempt'].update(turnId='accepted-turn')),
            ('observed-turn', lambda a: a['startAttempt'].update(observedTurnId='observed-turn')),
            ('source-thread', lambda a: a['contextRepair']['source'].update(threadId='old-thread')),
            ('source-account', lambda a: a['contextRepair']['source'].update(accountKey='old-account')),
            ('source-epoch', lambda a: a['contextRepair']['source'].update(epoch=-1)),
            ('source-attempt', lambda a: a['contextRepair']['source'].update(attemptId='old-attempt')),
            ('receipt-owner', lambda a: a['contextRepair'].update(agent='another-agent')),
            ('rpc-method', lambda a: a['contextRepair'].update(rpcMethod='thread/fork')),
            ('rpc-id', lambda a: a['contextRepair'].update(rpcId=19)),
            ('destination-thread', lambda a: a['contextRepair'].update(newThreadId='forked-thread')),
            ('snapshot', lambda a: a['contextRepair'].update(snapshot={'copyPath': '/saved/rollout.jsonl'})),
            ('settings', lambda a: a['contextRepair']['settings'].update(model='different-model')),
            ('receipt-error', lambda a: a['contextRepair'].update(error='different error')),
            ('unknown-receipt', lambda a: a['contextRepair'].update(phase='unknown')),
            ('event-error', None),
            ('event-turn', None),
        ]
        for name, mutate in cases:
            with self.subTest(case=name):
                agent = self.failed_claude()
                if mutate:
                    mutate(agent)
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', agent)
                    if name == 'event-error':
                        db.execute("UPDATE runtime_events SET error='another failure' WHERE id='failed-input-2'")
                    if name == 'event-turn':
                        db.execute("UPDATE runtime_events SET turn_id='accepted-turn' WHERE id='failed-input-2'")
                    before = [tuple(row) for row in db.execute('SELECT * FROM runtime_events ORDER BY id')]
                self.assertEqual(self.recover(), agent)
                with self.runtime.db() as db:
                    after = [tuple(row) for row in db.execute('SELECT * FROM runtime_events ORDER BY id')]
                self.assertEqual(after, before)
        self.assertEqual(self.server.calls, [])


if __name__ == '__main__':
    unittest.main()

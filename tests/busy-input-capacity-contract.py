#!/usr/bin/env python3
"""A busy input retains capacity if its old turn finishes before submission."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('preparation_fixture', Path(__file__).with_name('preparation-writer-lock-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class BusyInputCapacityContract(unittest.TestCase):
    def team(self, concurrency=8, global_concurrency=5):
        global_limit = patch.dict('os.environ', {'CODEX_CANVAS_CONCURRENCY': str(global_concurrency)})
        global_limit.start()
        self.addCleanup(global_limit.stop)
        temp = tempfile.TemporaryDirectory(prefix='studio-busy-capacity-')
        self.addCleanup(temp.cleanup)
        runtime = fixture.Runtime(Path(temp.name), fixture.fixture.FakeServer)
        self.addCleanup(runtime.close)
        lead = runtime.create({'name': 'Lead', 'cwd': temp.name, 'prompt': 'Coordinate', 'concurrency': concurrency})
        runtime.dispatch(lead['id'])
        fixture.fixture.eventually(lambda: runtime.agent(lead['id'])['status'] == 'running')
        lead = runtime.agent(lead['id'])
        children = [runtime.create({'name': 'Child ' + str(index), 'prompt': 'Review', 'role': 'reviewer'},
                                   lead['id']) for index in range(5)]
        runtime.dispatch()
        fixture.fixture.eventually(lambda: sum(runtime.agent(a['id'])['status'] == 'running' for a in children) == 4)
        return runtime, lead, children

    def active(self, runtime):
        with runtime.read_db() as db:
            agents = [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_agents')]
        return [a for a in agents if a.get('inFlight') or a['status'] in {'running', 'starting', 'approval'}]

    def slots(self, runtime):
        with runtime.read_db() as db:
            return runtime.dispatch_active_slots(db)

    def test_completion_keeps_the_reserved_slot_in_fast_and_roster_dispatch(self):
        for fast in (False, True):
            with self.subTest(fast=fast):
                runtime, lead, children = self.team()
                entered, release = threading.Event(), threading.Event()
                original = runtime.server.call
                def call(method, params, timeout=60):
                    if method == 'turn/start' and params.get('clientUserMessageId') == 'busy-input':
                        entered.set()
                        if not release.wait(3):
                            raise RuntimeError('The fixture busy input gate timed out')
                    return original(method, params, timeout)
                with patch.object(runtime.server, 'call', side_effect=call):
                    runtime.send(lead['id'], 'Busy input', 'busy-input', delivery='steer')
                    runtime.dispatch(lead['id'])
                    try:
                        self.assertTrue(entered.wait(2))
                        runtime.server.complete(lead['threadId'], lead['turnId'])
                        self.assertFalse(runtime.agent(lead['id'])['inFlight'])
                        self.assertEqual(len(self.active(runtime)), 4)
                        self.assertEqual(len(self.slots(runtime)), 5)
                        runtime.dispatch(children[-1]['id'] if fast else None)
                        self.assertEqual(runtime.agent(children[-1]['id'])['status'], 'queued')
                        self.assertEqual(len(runtime.server.active_turns), 4)
                    finally:
                        release.set()
                    fixture.fixture.eventually(lambda: runtime.delivery_receipt('busy-input')['status'] == 'delivered')
                self.assertEqual(len(self.active(runtime)), 5)
                self.assertEqual(len(runtime.server.active_turns), 5)
                self.assertEqual(sum(method == 'turn/start' and params.get('clientUserMessageId') == 'busy-input'
                                     for method, params in runtime.server.calls), 1)
                new_lead = runtime.agent(lead['id'])
                self.assertNotEqual(new_lead['turnId'], lead['turnId'])
                runtime.server.complete(new_lead['threadId'], new_lead['turnId'])
                self.assertEqual(len(self.slots(runtime)), 4)
                runtime.dispatch(children[-1]['id'])
                fixture.fixture.eventually(lambda: runtime.agent(children[-1]['id'])['status'] == 'running')
                self.assertEqual(len(self.active(runtime)), 5)

    @unittest.expectedFailure  # Owner decision pending: team-only limit does not retain the reserved slot.
    def test_team_only_limit_keeps_the_reserved_slot(self):
        for fast in (False, True):
            with self.subTest(fast=fast), patch.dict('os.environ', {'CODEX_CANVAS_CONCURRENCY': '32'}):
                runtime, lead, children = self.team(concurrency=5, global_concurrency=32)
                entered, release = threading.Event(), threading.Event()
                original = runtime.server.call
                def call(method, params, timeout=60):
                    if method == 'turn/start' and params.get('clientUserMessageId') == 'busy-input':
                        entered.set()
                        if not release.wait(3):
                            raise RuntimeError('The fixture busy input gate timed out')
                    return original(method, params, timeout)
                with patch.object(runtime.server, 'call', side_effect=call):
                    runtime.send(lead['id'], 'Busy input', 'busy-input', delivery='steer')
                    runtime.dispatch(lead['id'])
                    try:
                        self.assertTrue(entered.wait(2))
                        runtime.server.complete(lead['threadId'], lead['turnId'])
                        self.assertEqual(len(self.active(runtime)), 4)
                        self.assertEqual(len(self.slots(runtime)), 5)
                        runtime.dispatch(children[-1]['id'] if fast else None)
                        self.assertEqual(runtime.agent(children[-1]['id'])['status'], 'queued')
                        self.assertEqual(len(runtime.server.active_turns), 4)
                    finally:
                        release.set()
                    fixture.fixture.eventually(lambda: runtime.delivery_receipt('busy-input')['status'] == 'delivered')
                self.assertEqual(len(self.active(runtime)), 5)
                self.assertEqual(len(runtime.server.active_turns), 5)
                self.assertEqual(sum(method == 'turn/start' and params.get('clientUserMessageId') == 'busy-input'
                                     for method, params in runtime.server.calls), 1)
                new_lead = runtime.agent(lead['id'])
                self.assertNotEqual(new_lead['turnId'], lead['turnId'])
                runtime.server.complete(new_lead['threadId'], new_lead['turnId'])
                self.assertEqual(len(self.slots(runtime)), 4)
                runtime.dispatch(children[-1]['id'])
                fixture.fixture.eventually(lambda: runtime.agent(children[-1]['id'])['status'] == 'running')
                self.assertEqual(len(self.active(runtime)), 5)

    def test_known_busy_rejection_releases_the_slot_without_losing_input(self):
        runtime, lead, children = self.team()
        entered, release = threading.Event(), threading.Event()
        original = runtime.server.call
        def call(method, params, timeout=60):
            if method == 'turn/start' and params.get('clientUserMessageId') == 'rejected-input':
                runtime.server.calls.append((method, params))
                entered.set()
                if not release.wait(3):
                    raise RuntimeError('The fixture rejection gate timed out')
                raise ValueError('The native turn rejected this input')
            return original(method, params, timeout)
        with patch.object(runtime.server, 'call', side_effect=call):
            runtime.send(lead['id'], 'Rejected input', 'rejected-input', delivery='steer')
            runtime.dispatch(lead['id'])
            try:
                self.assertTrue(entered.wait(2))
                runtime.server.complete(lead['threadId'], lead['turnId'])
                self.assertEqual(len(self.slots(runtime)), 5)
            finally:
                release.set()
            fixture.fixture.eventually(lambda: runtime.delivery_receipt('rejected-input')['status'] == 'pending')
        self.assertEqual(len(self.slots(runtime)), 4)
        self.assertNotIn('startAttempt', runtime.agent(lead['id']))
        runtime.dispatch(children[-1]['id'])
        fixture.fixture.eventually(lambda: runtime.agent(children[-1]['id'])['status'] == 'running')
        self.assertEqual(runtime.delivery_receipt('rejected-input')['status'], 'pending')
        self.assertEqual(sum(method == 'turn/start' and params.get('clientUserMessageId') == 'rejected-input'
                             for method, params in runtime.server.calls), 1)

    def test_unknown_busy_response_preserves_capacity_and_never_replays_input(self):
        runtime, lead, children = self.team()
        entered, release = threading.Event(), threading.Event()
        original = runtime.server.call
        def call(method, params, timeout=60):
            if method == 'turn/start' and params.get('clientUserMessageId') == 'unknown-input':
                runtime.server.calls.append((method, params))
                entered.set()
                if not release.wait(3):
                    raise RuntimeError('The fixture unknown input gate timed out')
                raise RuntimeError('Native response timed out; outcome unknown')
            return original(method, params, timeout)
        with patch.object(runtime.server, 'call', side_effect=call):
            runtime.send(lead['id'], 'Unknown input', 'unknown-input', delivery='steer')
            runtime.dispatch(lead['id'])
            try:
                self.assertTrue(entered.wait(2))
                runtime.server.complete(lead['threadId'], lead['turnId'])
                self.assertEqual(len(self.slots(runtime)), 5)
            finally:
                release.set()
            fixture.fixture.eventually(lambda: runtime.delivery_receipt('unknown-input')['status'] == 'uncertain')
            for _ in range(2):
                runtime.dispatch()
                runtime.dispatch(children[-1]['id'])
        self.assertEqual(len(self.slots(runtime)), 5)
        self.assertEqual(runtime.agent(children[-1]['id'])['status'], 'queued')
        self.assertEqual(runtime.agent(lead['id'])['startAttempt']['events'], ['unknown-input'])
        self.assertEqual(runtime.delivery_receipt('unknown-input')['status'], 'uncertain')
        self.assertEqual(sum(method == 'turn/start' and params.get('clientUserMessageId') == 'unknown-input'
                             for method, params in runtime.server.calls), 1)

    def test_only_exact_outstanding_receipts_hold_an_inactive_agent_slot(self):
        runtime, lead, children = self.team()
        with runtime.lock, runtime.db() as db:
            agent = runtime.agent(lead['id'], db)
            agent.update(status='paused', autoWake=False, inFlight=False, turnId=None, epoch=1,
                         startAttempt={'id': 'saved-busy-input', 'epoch': 0,
                                       'activeAtReservation': True, 'events': ['exact-input']})
            runtime.enqueue(db, agent, 'user', 'Exact old-epoch input', 'exact-input')
            db.execute("UPDATE runtime_events SET epoch=0,status='uncertain' WHERE id='exact-input'")
            runtime.put(db, 'agents', agent)
        self.assertEqual(len(self.slots(runtime)), 5, 'Stop cannot erase an old unknown native receipt')
        for status in ('delivered', 'pending', 'failed', 'cancelled'):
            with self.subTest(status=status), runtime.db() as db:
                db.execute('UPDATE runtime_events SET status=? WHERE id=?', (status, 'exact-input'))
            self.assertEqual(len(self.slots(runtime)), 4)
        for changed in ('epoch', 'agent', 'id'):
            with self.subTest(changed=changed), runtime.db() as db:
                db.execute("UPDATE runtime_events SET status='uncertain',epoch=0,agent=?,id='exact-input' "
                           "WHERE id IN ('exact-input','other-input')",
                           (lead['id'],))
                if changed == 'epoch':
                    db.execute("UPDATE runtime_events SET epoch=2 WHERE id='exact-input'")
                elif changed == 'agent':
                    db.execute("UPDATE runtime_events SET agent=? WHERE id='exact-input'", (children[0]['id'],))
                else:
                    db.execute("UPDATE runtime_events SET id='other-input' WHERE id='exact-input'")
            self.assertEqual(len(self.slots(runtime)), 4)

    def test_live_legacy_dispatch_flag_does_not_skip_the_new_index(self):
        runtime, lead, _ = self.team()
        with runtime.db() as db:
            db.execute('DROP INDEX runtime_agent_dispatch_busy_input')
        runtime._dispatch_indexes_ready = True
        runtime.__dict__.pop('_dispatch_busy_input_index_ready', None)
        runtime.dispatch(lead['id'])
        with runtime.read_db() as db:
            self.assertIsNotNone(db.execute("SELECT 1 FROM sqlite_master WHERE type='index' "
                                           "AND name='runtime_agent_dispatch_busy_input'").fetchone())
        self.assertTrue(runtime._dispatch_busy_input_index_ready)


if __name__ == '__main__':
    unittest.main()

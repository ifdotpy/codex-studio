#!/usr/bin/env python3
"""Native start-or-steer delivery and exact local input reservations."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import Runtime
from codex_native_errors import NativeRpcError

spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class CriticalDelivery(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        self.agent = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'First'})['id']
        fixture.eventually(lambda: self.runtime.agent(self.agent)['status'] == 'running')
        self.server = self.runtime.server

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def starts(self):
        return [params for method, params in self.server.calls if method == 'turn/start']

    def event(self, event_id):
        with self.runtime.db() as db:
            return db.execute('SELECT * FROM runtime_events WHERE id=?', (event_id,)).fetchone()

    def test_retired_busy_notification_preserves_live_turn(self):
        from codex_wakeups import reconcile_start
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.agent, db)
            before = (agent['status'], agent['inFlight'], agent['turnId'])
            key = 'obsolete-complaint'
            self.runtime.put(db, 'complaints', {'id':'resolved','leadId':agent['id'],
                'authorRole':'worker','status':'resolved','responses':[{'authorRole':'lead','text':'Done'}]})
            self.runtime.enqueue(db, agent, 'complaint',
                json.dumps({'complaints':[{'complaint_id':'resolved'}]}), key)
            db.execute("UPDATE runtime_events SET status='reserved' WHERE id=?", (key,))
            agent['startAttempt'] = {'id':'busy-attempt','events':[key],
                'epoch':agent['epoch'],'submitted':False,'activeAtReservation':True}
            rows = [dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (key,)).fetchone())]
            self.assertTrue(reconcile_start(self.runtime, db, agent, rows))
        current = self.runtime.agent(self.agent)
        self.assertEqual((current['status'],current['inFlight'],current['turnId']), before)

    def test_busy_user_input_uses_turn_start_and_repeated_id_stays_local(self):
        turn = self.runtime.agent(self.agent)['turnId']
        self.runtime.send(self.agent, 'Second', 'busy-exact', delivery='queue')
        fixture.eventually(lambda: self.event('busy-exact')['status'] == 'delivered')
        self.assertEqual(self.runtime.agent(self.agent)['turnId'], turn)
        self.runtime.send(self.agent, 'Second', 'busy-exact', delivery='steer')
        self.runtime.send(self.agent, 'Second', 'busy-exact', delivery='after_tool')
        self.runtime.dispatch()
        self.assertEqual(len(self.starts()), 2)
        self.assertEqual(self.starts()[-1]['clientUserMessageId'], 'busy-exact')
        self.assertFalse(any(method == 'turn/steer' for method, _ in self.server.calls))

    def test_native_started_turn_is_busy_and_receives_input_at_once(self):
        a = self.runtime.agent(self.agent)
        self.server.notify({'method': 'turn/completed', 'params': {'threadId': a['threadId'],
            'turn': {'id': a['turnId'], 'status': 'completed'}}})
        fixture.eventually(lambda: not self.runtime.agent(self.agent).get('inFlight'))
        self.server.notify({'method': 'turn/started', 'params': {'threadId': a['threadId'],
            'turn': {'id': 'native-own-turn', 'status': 'inProgress'}}})
        fixture.eventually(lambda: self.runtime.agent(self.agent).get('turnId') == 'native-own-turn')
        self.assertTrue(self.runtime.agent(self.agent)['inFlight'])
        starts = len(self.starts())
        self.runtime.send(self.agent, 'During native turn', 'native-busy')
        fixture.eventually(lambda: len(self.starts()) == starts + 1)
        self.assertEqual(self.starts()[-1]['clientUserMessageId'], 'native-busy')

    def test_legacy_queue_notice_field_is_removed_once(self):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.agent, db)
            a['queueNotice'] = {'turnId': a['turnId'], 'at': 1, 'id': 'queue-notice:old'}
            self.runtime.put(db, 'agents', a)
        self.runtime.dispatch()
        self.assertNotIn('queueNotice', self.runtime.agent(self.agent))

    def test_repaired_completed_turn_queues_waiting_input(self):
        a = self.runtime.agent(self.agent)
        turn = a['turnId']
        with self.runtime.lock, self.runtime.db() as db:
            db.execute('INSERT OR IGNORE INTO runtime_completed_turns VALUES (?)', (a['id'] + ':' + turn,))
            a = self.runtime.agent(self.agent, db)
            self.runtime.enqueue(db, a, 'monitor_exit', 'Late monitor result', 'late-monitor')
            a = self.runtime.agent(self.agent, db)
            a.update(inFlight=True, turnId=turn, status='running')
            self.runtime.put(db, 'agents', a)
        self.server.notify({'method': 'turn/completed', 'params': {'threadId': a['threadId'],
            'turn': {'id': turn, 'status': 'completed'}}})
        fixture.eventually(lambda: self.event('late-monitor')['status'] == 'delivered')
        self.assertNotEqual(self.runtime.agent(self.agent)['turnId'], turn)

    def test_busy_agent_message_and_child_result_share_one_batch(self):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.agent, db)
            self.runtime.enqueue(db, a, 'monitor_exit', 'Monitor update', 'monitor-input')
            self.runtime.enqueue(db, a, 'child_result', 'Child result', 'child-input')
        fixture.eventually(lambda: self.event('child-input')['status'] == 'delivered')
        self.assertEqual(len(self.starts()), 2)
        text = self.starts()[-1]['input'][0]['text']
        self.assertLess(text.index('Monitor update'), text.index('Child result'))
        self.assertEqual(self.event('monitor-input')['turn_id'], self.event('child-input')['turn_id'])

    def test_unknown_submission_is_not_sent_again(self):
        self.server.fail_start = True
        self.runtime.send(self.agent, 'Unknown', 'unknown-exact')
        fixture.eventually(lambda: self.event('unknown-exact')['status'] == 'uncertain')
        before = len(self.starts())
        self.assertEqual(before, 2, 'the native request was submitted before its response was lost')
        self.runtime.send(self.agent, 'Unknown', 'unknown-exact')
        self.runtime.dispatch()
        self.assertEqual(len(self.starts()), before)
        self.assertEqual(self.event('unknown-exact')['status'], 'uncertain')

    def test_timing_write_failure_after_submission_does_not_replay(self):
        original = self.runtime.mark_event_timings
        def fail_after_submission(db, event_ids, marks):
            if 'submittedAt' in marks:
                raise sqlite3.OperationalError('timing store unavailable')
            return original(db, event_ids, marks)
        with patch.object(self.runtime, 'mark_event_timings', side_effect=fail_after_submission):
            self.runtime.send(self.agent, 'Once', 'timing-failure')
            fixture.eventually(lambda: self.event('timing-failure')['status'] == 'delivered')
        self.runtime.dispatch()
        self.assertEqual(sum(p.get('clientUserMessageId') == 'timing-failure'
                             for p in self.starts()), 1)

    def test_known_busy_rejection_waits_for_turn_end_then_delivers_once(self):
        original_turn = self.runtime.agent(self.agent)['turnId']
        original_call = self.server.call
        def reject(method, params, timeout=60):
            if method == 'turn/start' and params.get('clientUserMessageId') == 'rejected-exact':
                self.server.calls.append((method, params))
                raise NativeRpcError({'code': -32600, 'message': 'Cannot steer review',
                    'data': {'codexErrorInfo': {'activeTurnNotSteerable': {'turnKind': 'review'}}}})
            return original_call(method, params, timeout)
        with patch.object(self.server, 'call', side_effect=reject):
            self.runtime.send(self.agent, 'Keep this input', 'rejected-exact')
            fixture.eventually(lambda: self.runtime.agent(self.agent).get('steerRejectedTurnId') == original_turn)
        self.assertEqual(self.event('rejected-exact')['status'], 'pending')
        self.assertEqual(self.runtime.agent(self.agent)['status'], 'running')
        self.assertIsNone(self.runtime.agent(self.agent)['error'])
        pending = [item for item in self.runtime.transcript(self.agent)['items']
                   if item.get('clientMessageId') == 'rejected-exact']
        self.assertEqual(len(pending), 1)
        self.assertFalse(pending[0]['materialized'])
        self.assertEqual(len(self.runtime.queue_action(self.agent)['items']), 1)
        for _ in range(3):
            self.runtime.dispatch()
        self.assertEqual(sum(p.get('clientUserMessageId') == 'rejected-exact' for p in self.starts()), 1)
        self.server.complete(self.runtime.agent(self.agent)['threadId'], original_turn)
        fixture.eventually(lambda: self.event('rejected-exact')['status'] == 'delivered')
        self.assertEqual(sum(p.get('clientUserMessageId') == 'rejected-exact' for p in self.starts()), 2)
        self.assertNotEqual(self.event('rejected-exact')['turn_id'], original_turn)

    def test_busy_rejection_does_not_report_start_failed_to_parent(self):
        child = self.runtime.create({'name': 'Worker', 'prompt': 'First child input'}, self.agent)
        fixture.eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        current = self.runtime.agent(child['id'])
        original_call = self.server.call
        def reject(method, params, timeout=60):
            if method == 'turn/start' and params.get('clientUserMessageId') == 'child-rejected':
                raise NativeRpcError({'code': -32600, 'message': 'no active turn to steer'})
            return original_call(method, params, timeout)
        with patch.object(self.server, 'call', side_effect=reject):
            self.runtime.send(child['id'], 'Keep child input', 'child-rejected')
            fixture.eventually(lambda: self.runtime.delivery_receipt('child-rejected')['status'] == 'pending'
                and self.runtime.agent(child['id']).get('steerRejectedTurnId') == current['turnId'])
        with self.runtime.db() as db:
            failures = db.execute("SELECT id FROM runtime_events WHERE kind='child_result' "
                "AND id LIKE 'child:%start-failed:%'").fetchall()
        self.assertEqual(failures, [])
        self.assertEqual(self.runtime.agent(child['id'])['status'], 'running')
        self.assertIsNone(self.runtime.agent(child['id'])['error'])

    def test_busy_input_ignores_full_team_slot(self):
        with patch.dict('os.environ', {'CODEX_CANVAS_CONCURRENCY': '1'}):
            self.runtime.send(self.agent, 'No new slot', 'full-slot')
            fixture.eventually(lambda: self.event('full-slot')['status'] == 'delivered')
        self.assertEqual(len(self.starts()), 2)

    def test_native_review_and_context_repair_hold_input(self):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.agent, db)
            a['startAttempt'] = {'id': 'review-owner', 'epoch': a['epoch'], 'events': [],
                                 'action': 'review', 'submitted': True}
            self.runtime.put(db, 'agents', a)
            self.runtime.enqueue(db, a, 'work_decision', 'Review held', 'review-held')
        self.runtime.dispatch()
        self.assertEqual(self.event('review-held')['status'], 'pending')
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.agent, db)
            a['startAttempt'].pop('action')
            a['contextRepair'] = {'id': 'repair', 'phase': 'unknown'}
            self.runtime.put(db, 'agents', a)
        self.runtime.dispatch()
        self.assertEqual(self.event('review-held')['status'], 'pending')

    def test_account_transfer_and_stop_hold_input(self):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.agent, db)
            a['accountTransferId'] = 'transfer'
            self.runtime.put(db, 'agents', a)
            self.runtime.enqueue(db, a, 'monitor_exit', 'Exit held', 'transfer-held')
        with patch('codex_runtime.transfer_store') as transfer:
            transfer.return_value.tick.return_value = None
            self.runtime.dispatch()
        self.assertEqual(self.event('transfer-held')['status'], 'pending')
        self.runtime.stop(self.agent)
        with patch('codex_runtime.transfer_store') as transfer:
            transfer.return_value.tick.return_value = None
            self.runtime.dispatch()
        self.assertEqual(self.event('transfer-held')['status'], 'cancelled')

    def test_new_native_turn_id_supersedes_stale_active_projection(self):
        old = self.runtime.agent(self.agent)['turnId']
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(db, self.agent, 'old-output', 'assistant', 'Prior answer',
                              'Agent', turnId=old)
        self.server.active_turns.clear()
        with patch.dict('os.environ', {'CODEX_CANVAS_CONCURRENCY': '1'}):
            self.runtime.send(self.agent, 'Boundary input', 'boundary-input')
            fixture.eventually(lambda: self.event('boundary-input')['status'] == 'delivered')
        new = self.runtime.agent(self.agent)['turnId']
        self.assertNotEqual(new, old)
        self.assertTrue(self.runtime.agent(self.agent)['inFlight'])
        self.server.notify({'method': 'turn/completed', 'params': {'threadId': self.runtime.agent(self.agent)['threadId'],
            'turn': {'id': old, 'status': 'completed'}}})
        self.assertEqual(self.runtime.agent(self.agent)['turnId'], new)
        with self.runtime.db() as db:
            self.assertIsNotNone(db.execute('SELECT id FROM runtime_completed_turns WHERE id=?',
                (self.agent + ':' + old,)).fetchone())
            item = json.loads(db.execute('SELECT record FROM runtime_items WHERE id=?',
                (self.agent + ':old-output',)).fetchone()[0])
        self.assertEqual(item['turnStatus'], 'completed')


if __name__ == '__main__':
    unittest.main(verbosity=2)

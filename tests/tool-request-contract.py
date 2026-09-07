#!/usr/bin/env python3
"""Durable request identity, recovery, cancellation, and actor boundaries."""

from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_tool_requests import RequestMixin


class Ledger(RequestMixin):
    def __init__(self, path):
        self.path = path
        self.lock = threading.RLock()
        self.closed = False
        self.connection_ids = {}
        self.offline_accounts = set()
        with self.db() as db:
            db.executescript('''
                CREATE TABLE IF NOT EXISTS runtime_agents(id TEXT PRIMARY KEY, record TEXT);
                CREATE TABLE IF NOT EXISTS runtime_tool_results(id TEXT PRIMARY KEY, result TEXT);
            ''')
            self.setup_tool_requests(db)

    def connection_current(self, account_key, connection_id):
        return connection_id is None or (
            self.connection_ids.get(account_key) == connection_id
            and account_key not in self.offline_accounts
        )

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.path)
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_' + table)]

    def put(self, db, table, record):
        db.execute('INSERT INTO runtime_' + table + '(id,record) VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                   (record['id'], json.dumps(record)))

    def checked_actor(self, db, agent_id):
        row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (agent_id,)).fetchone()
        if not row or json.loads(row[0]).get('deletedAt'):
            raise ValueError('Unknown managed agent')
        return json.loads(row[0])


class RequestContract(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.runtime = Ledger(Path(self.directory.name) / 'state.sqlite3')
        with self.runtime.db() as db:
            for agent_id, thread, account in [('lead', 'thread', 'default'), ('worker', 'child', 'default'), ('other', 'thread', 'account2')]:
                self.runtime.put(db, 'agents', {'id': agent_id, 'threadId': thread, 'accountKey': account, 'epoch': 4})

    def message(self, call='call-1', tool='orchestration_spawn', args=None, thread='thread'):
        return {'id': 9, 'params': {'threadId': thread, 'turnId': 'turn-1', 'callId': call, 'tool': tool,
                                  'arguments': args if args is not None else {'agents': [{'name': 'worker', 'task': 'Inspect'}]}}}

    def result(self, success=True):
        return {'success': success, 'contentItems': [{'type': 'inputText', 'text': json.dumps({'agents': [{'id': 'worker-a'}, {'id': 'worker-b'}]})}]}

    def reserve(self, **kwargs):
        return self.runtime.reserve_tool_request(self.message(**kwargs))

    def test_semantic_retry_and_explicit_identity_aliases(self):
        args = {'request_id': 'batch-17', 'agents': [{'name': 'audit', 'task': 'Inspect'}]}
        first = self.reserve(args=args)
        self.assertEqual(first['id'], 'thread:spawn:batch-17')
        self.assertTrue(self.runtime.begin_tool_request(first['id']))
        message = self.message('call-2', args=json.dumps(dict(reversed(list(args.items())))))
        self.runtime.connection_ids['default'] = 'reconnected'
        second = self.runtime.reserve_tool_request(message, connection_id='reconnected')
        self.assertEqual(first['id'], second['id'])
        self.assertFalse(self.runtime.begin_tool_request(second['id']))
        settled = self.runtime.finish_tool_request(first['id'], self.result())
        for identifier in ['batch-17', 'call-1', 'call-2', 'thread:call-2', first['id']]:
            found = self.runtime.request_action('lead', {'action': 'get', 'request_id': identifier})
            self.assertEqual(found['outcome'], 'applied')
            self.assertEqual(found['agentIds'], ['worker-a', 'worker-b'])
            self.assertEqual(found['result'], settled['result'])
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.reserve(args={**args, 'agents': [{'name': 'other', 'task': 'Inspect'}]})
        self.assertEqual(self.runtime.tool_request(first['id'])['stage'], 'completed')

    def test_latency_separates_callback_reservation_and_execution_waits(self):
        message = self.message('latency')
        message.update(_studioReceivedAt=100.0, _studioDispatchedAt=140.0)
        with patch('codex_tool_requests.time.time', return_value=150.0):
            record = self.runtime.reserve_tool_request(message)
        with patch('codex_tool_requests.time.time', return_value=151.0):
            self.assertTrue(self.runtime.begin_tool_request(record['id']))
        record = self.runtime.tool_request(record['id'])
        self.assertEqual(record['callbackQueueDelayMs'], 40000)
        self.assertEqual(record['reservationDelayMs'], 10000)
        self.assertEqual(record['admissionDelayMs'], 50000)
        self.assertEqual(record['executionQueueDelayMs'], 1000)
        self.assertEqual(record['queueDelayMs'], 51000)

    def test_queued_cancel_proves_nonexecution_and_prevents_begin(self):
        record = self.reserve()
        cancelled = self.runtime.request_action('lead', {'action': 'cancel', 'request_id': 'call-1'})
        self.assertEqual((cancelled['stage'], cancelled['outcome']), ('cancelled', 'not_applied'))
        self.assertFalse(cancelled['result']['success'])
        self.assertFalse(self.runtime.begin_tool_request(record['id']))
        self.assertEqual(self.reserve()['outcome'], 'not_applied')

    def test_running_cancel_retains_uncertainty_until_success(self):
        record = self.reserve()
        self.runtime.begin_tool_request(record['id'])
        cancelled = self.runtime.request_action('lead', {'action': 'cancel', 'request_id': record['id']})
        self.assertEqual((cancelled['stage'], cancelled['outcome']), ('running', 'pending'))
        self.assertTrue(cancelled['cancelRequested'])
        self.assertNotIn('result', cancelled)
        settled = self.runtime.finish_tool_request(record['id'], self.result())
        self.assertEqual(settled['outcome'], 'applied')
        # A response-delivery exception cannot erase a committed success.
        self.assertEqual(self.runtime.finish_tool_request(record['id'], self.result(False)), settled)

    def test_failure_does_not_claim_nonexecution(self):
        record = self.reserve()
        self.runtime.begin_tool_request(record['id'])
        failed = self.runtime.finish_tool_request(record['id'], self.result(False))
        self.assertEqual((failed['stage'], failed['outcome']), ('failed', 'unknown'))
        self.assertFalse(self.runtime.begin_tool_request(record['id']))
        # Later authoritative reconciliation may resolve the earlier ambiguity.
        self.assertEqual(self.runtime.finish_tool_request(record['id'], self.result(), 'applied')['outcome'], 'applied')
        absent = self.runtime.request_action('lead', {'action': 'get', 'request_id': 'missing'})
        self.assertEqual((absent['stage'], absent['outcome']), ('not_found', 'unknown'))

    def test_account_and_actor_boundaries(self):
        lead = self.reserve()
        other = self.runtime.reserve_tool_request(self.message(), 'account2')
        self.assertEqual(other['id'], 'account2:thread:call-1')
        for actor, identifier in [('worker', lead['id']), ('other', lead['id']), ('lead', other['id'])]:
            hidden = self.runtime.request_action(actor, {'action': 'cancel', 'request_id': identifier})
            self.assertEqual(hidden['outcome'], 'unknown')
            self.assertEqual(hidden['stage'], 'not_found')
        self.assertEqual(self.runtime.tool_request(lead['id'])['stage'], 'queued')
        self.assertEqual(len(self.runtime.request_action('lead', {'action': 'list'})['requests']), 1)

    def test_restart_distinguishes_never_started_running_and_cached(self):
        queued = self.reserve(call='queued')
        running = self.reserve(call='running')
        cached = self.reserve(call='cached')
        self.runtime.begin_tool_request(running['id'])
        self.runtime.begin_tool_request(cached['id'])
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', (cached['id'], json.dumps(self.result())))
        self.runtime = Ledger(self.runtime.path)
        self.assertEqual(self.runtime.tool_request(queued['id'])['outcome'], 'not_applied')
        self.assertEqual(self.runtime.tool_request(running['id'])['outcome'], 'unknown')
        self.assertEqual(self.runtime.tool_request(running['id'])['stage'], 'interrupted')
        self.assertEqual(self.runtime.tool_request(cached['id'])['outcome'], 'applied')
        self.assertFalse(self.runtime.begin_tool_request(running['id']))

    def test_atomic_receipt_obeys_transaction_rollback(self):
        record = self.reserve()
        self.runtime.begin_tool_request(record['id'])
        with self.assertRaisesRegex(RuntimeError, 'rollback'):
            with self.runtime.db() as db:
                self.runtime.finish_tool_request(record['id'], self.result(), db=db)
                raise RuntimeError('rollback')
        self.assertEqual(self.runtime.tool_request(record['id'])['outcome'], 'pending')
        with self.runtime.db() as db:
            self.runtime.finish_tool_request(record['id'], self.result(), db=db)
        self.runtime = Ledger(self.runtime.path)
        self.assertEqual(self.runtime.tool_request(record['id'])['outcome'], 'applied')

    def test_legacy_receipts_are_recoverable_without_a_ledger(self):
        with self.runtime.db() as db:
            for call, success in [('legacy-ok', True), ('legacy-error', False)]:
                db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', ('thread:' + call, json.dumps(self.result(success))))
        result = self.runtime.request_action('lead', {'action': 'get', 'request_id': 'legacy-ok'})
        self.assertEqual(result['outcome'], 'applied')
        self.assertTrue(result['legacy'])
        result = self.runtime.request_action('lead', {'action': 'cancel', 'request_id': 'thread:legacy-error'})
        self.assertEqual(result['outcome'], 'unknown')
        self.assertFalse(result['cancelRequested'])
        reserved = self.reserve(call='legacy-ok')
        self.assertEqual(reserved['outcome'], 'applied')
        self.assertFalse(self.runtime.begin_tool_request(reserved['id']))

    def test_list_is_bounded_and_omits_result_bodies(self):
        for n in range(56):
            record = self.reserve(call='call-' + str(n))
            self.runtime.finish_tool_request(record['id'], self.result())
        records = self.runtime.request_action('lead', {'action': 'list'})['requests']
        self.assertEqual(len(records), 50)
        self.assertTrue(all('result' not in row and 'signature' not in row for row in records))
        self.assertEqual(records[0]['callId'], 'call-55')

    def test_delayed_result_reconciles_before_status_or_cancel(self):
        for action in ['get', 'cancel', 'list']:
            record = self.reserve(call=action)
            self.runtime.begin_tool_request(record['id'])
            with self.runtime.db() as db:
                db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', (record['id'], json.dumps(self.result())))
            data = {'action': action}
            if action != 'list':
                data['request_id'] = record['id']
            found = self.runtime.request_action('lead', data)
            if action == 'list':
                found = next(row for row in found['requests'] if row['id'] == record['id'])
            self.assertEqual(found['outcome'], 'applied')
            self.assertFalse(found['cancelRequested'])

    def test_only_one_concurrent_begin_can_execute(self):
        record = self.reserve()
        barrier = threading.Barrier(8)
        values = []
        def claim():
            barrier.wait()
            values.append(self.runtime.begin_tool_request(record['id']))
        workers = [threading.Thread(target=claim) for _ in range(8)]
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join(5)
        self.assertEqual(values.count(True), 1)
        self.assertEqual(values.count(False), 7)

    def test_conflicting_short_alias_is_not_silently_retargeted(self):
        first = self.reserve(call='short', args={'request_id': 'batch', 'agents': []})
        second = self.reserve(call='other', args={'request_id': 'short', 'agents': []})
        found = self.runtime.request_action('lead', {'action': 'get', 'request_id': 'short'})
        self.assertEqual((found['stage'], found['outcome']), ('ambiguous', 'unknown'))
        for record in [first, second]:
            self.assertEqual(self.runtime.request_action('lead', {'action': 'get', 'request_id': record['id']})['id'], record['id'])

    def test_rejects_invalid_explicit_identity_and_nonfinite_arguments(self):
        for request_id in ['', ' ', ' x', 'x\n', 5, 'a' * 201]:
            with self.assertRaises(ValueError):
                self.reserve(args={'request_id': request_id})
        with self.assertRaises(ValueError):
            self.reserve(args={'timeout': float('nan')})

    def test_wire_timestamp_survives_replay_and_excludes_invalid_values(self):
        message = self.message()
        message['_studioReceivedAt'] = 1500000000.25
        record = self.runtime.reserve_tool_request(message)
        message['_studioReceivedAt'] = 1500000100
        replay = self.runtime.reserve_tool_request(message)
        self.assertEqual(record['wireReceivedAt'], replay['wireReceivedAt'])
        self.runtime.begin_tool_request(record['id'])
        started = self.runtime.tool_request(record['id'])
        self.assertEqual(started['queueDelayMs'], (started['started'] - record['wireReceivedAt']) * 1000)
        for n, value in enumerate([float('nan'), float('inf'), True, '1500000000']):
            message = self.message('bad-time-' + str(n))
            message['_studioReceivedAt'] = value
            self.assertNotIn('wireReceivedAt', self.runtime.reserve_tool_request(message))

    def test_closed_or_stale_connection_cannot_reserve(self):
        self.runtime.connection_ids['default'] = 'current'
        with self.assertRaisesRegex(ValueError, 'connection changed'):
            self.runtime.reserve_tool_request(self.message(), connection_id='old')
        self.runtime.offline_accounts.add('default')
        with self.assertRaisesRegex(ValueError, 'connection changed'):
            self.runtime.reserve_tool_request(self.message(), connection_id='current')
        self.runtime.offline_accounts.clear()
        self.runtime.closed = True
        with self.assertRaisesRegex(ValueError, 'connection changed'):
            self.runtime.reserve_tool_request(self.message())
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_tool_requests').fetchone()[0], 0)
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_tool_request_aliases').fetchone()[0], 0)

    def save_operation_receipt(self, key, value):
        with self.runtime.db() as db:
            db.execute('CREATE TABLE IF NOT EXISTS runtime_operation_receipts(id TEXT PRIMARY KEY, signature TEXT, result TEXT)')
            db.execute('INSERT INTO runtime_operation_receipts VALUES (?,?,?)', (key, 'fixture-signature', json.dumps(value)))

    def test_running_request_exposes_committed_operation_without_claiming_tool_completion(self):
        record = self.reserve(tool='orchestration_task', args={'action': 'accept', 'task_id': 'work-1', 'result': 'Verified'})
        self.runtime.begin_tool_request(record['id'])
        operation = {'id': 'work-1', 'status': 'accepted', 'version': 3, 'decisions': [{'decision': 'accept'}]}
        self.save_operation_receipt(record['id'], operation)
        before = self.runtime.tool_request(record['id'])
        for _ in range(3):
            result = self.runtime.request_action('lead', {'action': 'get', 'request_id': 'call-1'})
            self.assertEqual((result['stage'], result['outcome']), ('running', 'pending'))
            self.assertEqual(result['operationResult'], operation)
            self.assertTrue(result['operationApplied'])
            self.assertEqual(result['evidenceSource'], 'operation_receipt')
            self.assertNotIn('result', result)
        self.assertFalse(self.runtime.begin_tool_request(record['id']))
        self.assertEqual(self.runtime.tool_request(record['id']), before)
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_tool_results').fetchone()[0], 0)
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_operation_receipts').fetchone()[0], 1)
        final = self.result()
        self.runtime.finish_tool_request(record['id'], final)
        found = self.runtime.request_action('lead', {'action': 'get', 'request_id': record['id']})
        self.assertEqual(found['result'], final)
        self.assertEqual(found['outcome'], 'applied')
        self.assertNotIn('operationResult', found)

    def test_operation_only_legacy_receipt_is_unknown_and_scoped(self):
        operation = {'id': 'work-legacy', 'status': 'accepted', 'version': 7}
        self.save_operation_receipt('thread:legacy-accept', operation)
        for request_id in ['legacy-accept', 'thread:legacy-accept']:
            found = self.runtime.request_action('lead', {'action': 'get', 'request_id': request_id})
            self.assertEqual((found['stage'], found['outcome']), ('unknown', 'unknown'))
            self.assertTrue(found['legacy'])
            self.assertTrue(found['operationApplied'])
            self.assertEqual(found['operationResult'], operation)
            self.assertNotIn('result', found)
        for actor in ['worker', 'other']:
            found = self.runtime.request_action(actor, {'action': 'get', 'request_id': 'thread:legacy-accept'})
            self.assertEqual(found['stage'], 'not_found')
            self.assertNotIn('operationResult', found)
        missing = self.runtime.request_action('lead', {'action': 'get', 'request_id': 'no-operation'})
        self.assertEqual(missing['outcome'], 'unknown')
        self.assertNotIn('operationApplied', missing)
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_tool_requests').fetchone()[0], 0)
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_tool_results').fetchone()[0], 0)


if __name__ == '__main__':
    unittest.main()

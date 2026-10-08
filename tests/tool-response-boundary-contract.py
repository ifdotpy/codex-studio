#!/usr/bin/env python3
"""Saved tool completions survive failures before and after response delivery."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import AppServer, Runtime


class NoNativeServer:
    calls = 0

    def __init__(self, *_args, **_kwargs):
        type(self).calls += 1
        raise AssertionError('This fixture cannot start a native transport')


class ResponseSink:
    def __init__(self):
        self.messages = []
        self.operation_ids = []
        self.attempts = 0
        self.failure = None
        self.changed = threading.Event()
        self.receipt = None

    def write(self, message, *, operation_id=None):
        self.attempts += 1
        self.operation_ids.append(operation_id)
        if self.failure:
            raise self.failure
        self.messages.append(copy.deepcopy(message))
        self.changed.set()
        return self.receipt

    def close(self):
        pass

    def join_callbacks(self, **_kwargs):
        return True


class ToolResponseBoundaryContract(unittest.TestCase):
    @staticmethod
    def operation_result(result):
        """Compare the saved operation, excluding model-only policy context."""
        return {**result, 'contentItems': [
            item for item in result.get('contentItems', [])
            if not (item.get('type') == 'inputText' and item.get('text', '').startswith((
                '[Studio subagent concurrency, revision ',
                '[Time awareness] Tool result finalized at ')))]}

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-tool-response-')
        self.addCleanup(self.temp.cleanup)
        self.calls_before = NoNativeServer.calls
        with patch.object(Runtime, 'schedule', lambda _runtime: None):
            self.runtime = Runtime(Path(self.temp.name), NoNativeServer)
        self.runtime.image_workspace_support = lambda _repo: (False, 'disabled in protocol fixture')
        self.addCleanup(self.runtime.close)
        self.sink = ResponseSink()
        self.runtime.servers['default'] = self.sink
        self.runtime.connection_ids['default'] = 'original-connection'
        self.actor = {'id': 'fixture-actor', 'name': 'Fixture actor', 'rootId': 'fixture-actor',
            'isLead': True, 'parentId': None, 'threadId': 'fixture-thread', 'turnId': 'fixture-turn',
            'inFlight': True, 'status': 'running', 'autoWake': True, 'epoch': 3, 'turnEpoch': 3,
            'events': 0, 'model': 'fixture-model', 'effort': 'medium', 'accountKey': 'default',
            'provider': 'claude', 'role': 'orchestrator', 'cwd': self.temp.name, 'created': 1,
            'prompt': 'Fixture', 'tokensUsed': 0, 'tokenBudget': None, 'tail': ''}
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.actor)
        self.message = {'id': 'claude:fixture-rpc', 'params': {
            'threadId': self.actor['threadId'], 'turnId': self.actor['turnId'],
            'callId': 'fixture-call', 'tool': 'orchestration_task',
            'arguments': {'action': 'create', 'title': 'Fixture assignment'}}}
        self.key = self.runtime.tool_request_key(self.message)
        self.operations = 0
        self.original_work = self.runtime.model_work
        def work(*args, **kwargs):
            self.operations += 1
            return self.original_work(*args, **kwargs)
        self.work_patch = patch.object(self.runtime, 'model_work', work)
        self.work_patch.start()
        self.addCleanup(self.work_patch.stop)

    def tearDown(self):
        self.assertEqual(NoNativeServer.calls, self.calls_before)

    def run_tool(self):
        self.runtime.dynamic(self.message, 'default', 'original-connection')

    def receipt(self):
        return self.runtime.tool_request(self.key)

    def errors(self):
        path = self.runtime.root / 'runtime-errors.log'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def assert_one_operation_and_saved_response(self, outcome='applied'):
        self.assertEqual(self.operations, 1)
        self.assertEqual(self.sink.attempts, 1)
        self.assertEqual(len(self.sink.messages), 1)
        receipt = self.receipt()
        self.assertEqual(receipt['outcome'], outcome)
        delivered = self.sink.messages[0]
        self.assertEqual(delivered['id'], self.message['id'])
        self.assertEqual(self.operation_result(delivered['result']),
                         self.operation_result(receipt['result']))
        policy = [item for item in delivered['result']['contentItems']
                  if item.get('type') == 'inputText' and item.get('text', '').startswith(
                      '[Studio subagent concurrency, revision ')]
        self.assertLessEqual(len(policy), 1)
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_work').fetchone()[0], 1)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_operation_receipts WHERE id=?',
                (self.key,)).fetchone()[0], 1)

    def test_projection_failure_returns_the_saved_result_once(self):
        def failed(_actor, _key, result):
            result['success'] = False
            result['contentItems'].append({'type': 'inputText', 'text': 'fixture-secret-do-not-retain'})
            raise ValueError('fixture-secret-do-not-retain')
        with patch.object(self.runtime, 'model_tool_result', failed):
            self.run_tool()
        self.assert_one_operation_and_saved_response()
        self.assertTrue(self.sink.messages[0]['result']['success'])
        self.assertEqual(self.errors()[0]['event'], 'tool_response_preparation_failed')
        self.assertEqual(self.errors()[0]['phase'], 'projection')
        self.assertNotIn('fixture-secret-do-not-retain', json.dumps(self.errors()))
        self.assertNotIn('fixture-secret-do-not-retain', json.dumps(self.sink.messages))

    def test_analytics_failure_returns_the_saved_result_once(self):
        with patch.object(self.runtime, 'analytics_safe', side_effect=sqlite3.OperationalError('database is locked')):
            self.run_tool()
        self.assert_one_operation_and_saved_response()
        self.assertEqual(self.errors()[0]['phase'], 'analytics')
        self.assertEqual(self.errors()[0]['errorType'], 'OperationalError')

    def test_unknown_outcome_and_saved_failure_remain_unknown(self):
        def uncertain(*args, **kwargs):
            self.operations += 1
            self.original_work(*args, **kwargs)
            raise RuntimeError('The operation response was lost')
        with patch.object(self.runtime, 'model_work', uncertain), \
                patch.object(self.runtime, 'model_tool_result', side_effect=ValueError('fixture projection failure')):
            self.run_tool()
        self.assert_one_operation_and_saved_response('unknown')
        self.assertFalse(self.sink.messages[0]['result']['success'])
        self.assertEqual(self.receipt()['stage'], 'failed')

    def test_changed_connection_never_receives_the_old_response(self):
        def changed(*_args):
            self.runtime.connection_ids['default'] = 'replacement-connection'
            raise ValueError('fixture projection failure')
        with patch.object(self.runtime, 'model_tool_result', changed):
            self.run_tool()
        self.assertEqual(self.operations, 1)
        self.assertEqual(self.sink.attempts, 0)
        self.assertEqual(self.receipt()['outcome'], 'applied')
        self.assertEqual(self.errors()[-1]['event'], 'tool_response_delivery_failed')
        self.assertEqual(self.errors()[-1]['connectionId'], 'original-connection')

    def test_response_write_failure_preserves_the_receipt_without_replay(self):
        self.sink.failure = BrokenPipeError('fixture pipe failure')
        self.run_tool()
        self.assertEqual(self.operations, 1)
        self.assertEqual(self.sink.attempts, 1)
        self.assertEqual(self.sink.messages, [])
        self.assertEqual(self.receipt()['outcome'], 'applied')
        self.assertEqual(self.errors()[-1]['phase'], 'write')

    def test_confirmation_failure_does_not_send_a_second_response(self):
        with patch.object(self.runtime, 'confirm_model_tool_result',
                side_effect=sqlite3.OperationalError('database is locked')):
            self.run_tool()
        self.assert_one_operation_and_saved_response()
        self.assertEqual(self.errors()[-1]['event'], 'tool_response_confirmation_failed')
        self.assertEqual(self.errors()[-1]['phase'], 'confirmation')

    def test_stop_during_projection_preserves_the_stop_and_the_exact_response(self):
        def stopped(*_args):
            with self.runtime.lock, self.runtime.db() as db:
                actor = self.runtime.agent(self.actor['id'], db)
                actor.update(autoWake=False, epoch=4, status='paused')
                self.runtime.put(db, 'agents', actor)
            raise ValueError('fixture projection failure')
        with patch.object(self.runtime, 'model_tool_result', stopped):
            self.run_tool()
        self.assert_one_operation_and_saved_response()
        actor = self.runtime.agent(self.actor['id'])
        self.assertFalse(actor['autoWake'])
        self.assertEqual(actor['epoch'], 4)
        self.assertEqual(actor['status'], 'paused')

    def test_dynamic_forwards_the_exact_response_operation_id(self):
        with patch('codex_tool_response_recovery.response_operation_id',
                return_value='fixture-response-operation') as identity:
            self.run_tool()
        self.assert_one_operation_and_saved_response()
        identity.assert_called_once_with(self.runtime, 'default', self.message['id'],
                                         'original-connection')
        self.assertEqual(self.sink.operation_ids, ['fixture-response-operation'])

    def test_reply_keeps_the_existing_write_call_without_an_operation_id(self):
        with patch.object(self.sink, 'write', wraps=self.sink.write) as write:
            response = {'id': 'fixture-direct-reply', 'result': {'success': True}}
            self.runtime.reply(response, 'default', 'original-connection')
        write.assert_called_once_with(response)

    def test_legacy_claude_response_uses_the_same_deterministic_operation_id(self):
        response = {'id': 'claude:521a0f98-44ab-487d-b64c-eb5ef5fc14b2',
                    'result': {'success': True}}
        with patch('codex_tool_response_recovery.response_operation_id',
                return_value='fixture-response-operation') as identity:
            self.runtime.reply(response, 'default', 'original-connection')
        identity.assert_called_once_with(self.runtime, 'default', response['id'],
                                         'original-connection')
        self.assertEqual(self.sink.operation_ids, ['fixture-response-operation'])

    def test_legacy_claude_response_without_a_supervisor_keeps_the_old_write_call(self):
        response = {'id': 'claude:521a0f98-44ab-487d-b64c-eb5ef5fc14b2',
                    'result': {'success': True}}
        with patch.object(self.sink, 'write', wraps=self.sink.write) as write:
            self.runtime.reply(response, 'default', 'original-connection')
        write.assert_called_once_with(response)

    def test_claude_notifications_do_not_receive_a_response_operation_id(self):
        message = {'id': 'claude:521a0f98-44ab-487d-b64c-eb5ef5fc14b2',
                   'method': 'fixture-notification'}
        with patch('codex_tool_response_recovery.response_operation_id') as identity:
            self.runtime.reply(message, 'default', 'original-connection')
        identity.assert_not_called()
        self.assertEqual(self.sink.operation_ids, [None])

    def test_terminal_native_error_preserves_the_exact_stop_reason(self):
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.actor['id'], db)
            actor.update(autoWake=False, epoch=4, status='paused', error='Stopped by agent Fixture lead')
            self.runtime.put(db, 'agents', actor)
        native_error = {'message': 'Fixture interruption details'}
        self.runtime.notification({'method': 'turn/completed', 'params': {
            'threadId': self.actor['threadId'], 'turn': {
                'id': self.actor['turnId'], 'status': 'interrupted', 'error': native_error}}},
            'default', 'original-connection')
        actor = self.runtime.agent(self.actor['id'])
        self.assertEqual(actor['error'], 'Stopped by agent Fixture lead')
        self.assertEqual(actor['lastCompletedTurnError'], native_error)
        self.assertEqual(actor['lastCompletedTurnStatus'], 'interrupted')
        self.assertEqual((actor['status'], actor['autoWake'], actor['epoch']), ('paused', False, 4))
        self.assertFalse(actor['inFlight'])
        self.assertEqual(self.sink.attempts, 0)

    def reject_receipt_completion(self):
        with self.runtime.db() as db:
            db.execute("CREATE TRIGGER reject_receipt_completion BEFORE UPDATE ON runtime_tool_requests "
                "WHEN json_extract(NEW.record,'$.stage')='completed' "
                "BEGIN SELECT RAISE(ABORT,'fixture receipt write failure'); END")

    def test_sqlite_metadata_failure_returns_the_committed_result_once(self):
        self.reject_receipt_completion()
        self.run_tool()
        self.assertEqual(self.operations, 1)
        self.assertEqual(self.sink.attempts, 1)
        receipt = self.receipt()
        self.assertEqual(receipt['stage'], 'running')
        self.assertEqual(receipt['outcome'], 'pending')
        self.assertIsNone(receipt.get('result'))
        with self.runtime.read_db() as db:
            saved = self.runtime.tool_result(db, self.key)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_work').fetchone()[0], 1)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_operation_receipts WHERE id=?',
                (self.key,)).fetchone()[0], 1)
        self.assertEqual(len(self.sink.messages), 1)
        self.assertEqual(self.sink.messages[0]['id'], self.message['id'])
        self.assertEqual(self.operation_result(self.sink.messages[0]['result']),
                         self.operation_result(saved))
        self.assertEqual(self.errors()[0]['phase'], 'receipt')
        self.assertEqual(self.errors()[0]['errorType'], 'IntegrityError')

    def test_unavailable_completion_evidence_returns_unknown_without_replay(self):
        self.reject_receipt_completion()
        with patch.object(self.runtime, 'read_db', side_effect=sqlite3.OperationalError('fixture read failure')):
            self.run_tool()
        self.assertEqual(self.operations, 1)
        self.assertEqual(self.sink.attempts, 1)
        result = self.sink.messages[0]['result']
        self.assertFalse(result['success'])
        report = json.loads(next(item['text'] for item in result['contentItems']
            if item['type'] == 'inputText' and not item['text'].startswith('[Time awareness]')))
        self.assertEqual(report['outcome'], 'unknown')
        self.assertEqual(report['requestId'], self.key)
        self.assertEqual(self.receipt()['stage'], 'running')
        self.assertEqual(self.receipt()['outcome'], 'pending')

    def reject_tool_result_inserts(self):
        original = self.runtime.db
        def failed():
            raise sqlite3.OperationalError('fixture result persistence failure')
        @contextmanager
        def registered(**kwargs):
            with original(**kwargs) as db:
                db.create_function('fixture_result_failure', 0, failed)
                yield db
        gate = patch.object(self.runtime, 'db', registered)
        gate.start()
        self.addCleanup(gate.stop)
        with self.runtime.db() as db:
            db.execute("CREATE TRIGGER reject_tool_result BEFORE INSERT ON runtime_tool_results "
                "BEGIN SELECT fixture_result_failure(); END")

    def submit_request(self):
        futures = []
        original = self.runtime.coordination_pool.submit
        def submit(*args, **kwargs):
            future = original(*args, **kwargs)
            futures.append(future)
            return future
        with patch.object(self.runtime.coordination_pool, 'submit', submit):
            self.runtime.request({'method': 'item/tool/call', **self.message},
                'default', 'original-connection')
        self.assertEqual(len(futures), 1)
        return futures[0]

    def test_future_failure_after_both_result_writes_returns_unknown_once(self):
        self.reject_tool_result_inserts()
        future = self.submit_request()
        with self.assertRaises(sqlite3.OperationalError):
            future.result(timeout=3)
        self.assertTrue(self.sink.changed.wait(3))
        self.assertEqual(self.operations, 1)
        self.assertEqual(self.sink.attempts, 1)
        result = self.sink.messages[0]['result']
        report = json.loads(result['contentItems'][0]['text'])
        self.assertFalse(result['success'])
        self.assertEqual(report['requestId'], self.key)
        self.assertEqual(report['outcome'], 'unknown')
        self.assertEqual(self.receipt()['stage'], 'running')
        self.assertEqual(self.receipt()['outcome'], 'pending')
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_work').fetchone()[0], 1)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_tool_results').fetchone()[0], 0)

    def test_future_failure_reads_the_exact_committed_result(self):
        self.run_tool()
        saved = self.receipt()['result']
        self.sink.messages.clear()
        self.sink.attempts = 0
        self.sink.changed.clear()
        with patch.object(self.runtime, 'dynamic', side_effect=sqlite3.OperationalError('fixture old frame failure')):
            future = self.submit_request()
            with self.assertRaises(sqlite3.OperationalError):
                future.result(timeout=3)
        self.assertTrue(self.sink.changed.wait(3))
        self.assertEqual(self.operations, 1)
        self.assertEqual(self.sink.attempts, 1)
        self.assertEqual(self.sink.messages, [{'id': self.message['id'], 'result': saved}])
        self.assertEqual(self.receipt()['outcome'], 'applied')

    def test_future_failure_preserves_a_stop(self):
        self.reject_tool_result_inserts()
        original = self.runtime.model_work
        def stopped(*args, **kwargs):
            result = original(*args, **kwargs)
            with self.runtime.db() as db:
                actor = self.runtime.agent(self.actor['id'], db)
                actor.update(autoWake=False, epoch=4, status='paused')
                self.runtime.put(db, 'agents', actor)
            return result
        with patch.object(self.runtime, 'model_work', stopped):
            future = self.submit_request()
            with self.assertRaises(sqlite3.OperationalError):
                future.result(timeout=3)
            self.assertTrue(self.sink.changed.wait(3))
        self.assertEqual(self.operations, 1)
        self.assertEqual(self.sink.attempts, 1)
        self.assertFalse(self.runtime.agent(self.actor['id'])['autoWake'])
        self.assertEqual(self.runtime.agent(self.actor['id'])['epoch'], 4)

    def test_future_failure_preserves_the_original_connection_guard(self):
        self.runtime.reserve_tool_request(self.message, 'default', 'original-connection')
        self.runtime.connection_ids['default'] = 'replacement-connection'
        self.runtime.dynamic_response_failure(self.message, 'default', 'original-connection',
            sqlite3.OperationalError('fixture handler failure'))
        self.assertEqual(self.operations, 0)
        self.assertEqual(self.sink.attempts, 0)
        self.assertEqual(self.receipt()['outcome'], 'pending')

    def test_future_failure_never_replaces_an_accepted_native_response(self):
        self.runtime.reserve_tool_request(self.message, 'default', 'original-connection')
        path = self.runtime.root / 'supervisor.sqlite3'
        db = sqlite3.connect(path)
        try:
            db.execute('CREATE TABLE handles(id TEXT PRIMARY KEY,generation INTEGER,closed_at REAL)')
            db.execute('CREATE TABLE operations(handle TEXT,operation_id TEXT,native_id TEXT)')
            db.execute("INSERT INTO handles VALUES ('account:default',7,NULL)")
            db.execute("INSERT INTO operations VALUES ('account:default','already-accepted',?)",
                (self.message['id'],))
            db.commit()
        finally:
            db.close()
        self.sink.supervisor_mode = True
        self.sink.proc = SimpleNamespace(root=self.runtime.root, handle='account:default', generation=7)
        self.runtime.dynamic_response_failure(self.message, 'default', 'original-connection',
            sqlite3.OperationalError('fixture handler failure'))
        self.assertEqual(self.operations, 0)
        self.assertEqual(self.sink.attempts, 0)
        self.assertEqual(self.receipt()['outcome'], 'pending')
        self.assertTrue(self.errors()[-1]['nativeAccepted'])

    def test_reply_returns_the_supervisor_acceptance_receipt(self):
        self.sink.receipt = {'accepted': True, 'duplicate': False, 'remoteId': 'fixture-rpc'}
        written = self.runtime.reply({'id': 'fixture-rpc', 'result': {'success': True}},
            'default', 'original-connection', operation_id='fixture-response-operation')
        self.assertIs(written, self.sink.receipt)
        self.assertEqual(self.sink.operation_ids, ['fixture-response-operation'])

    def test_legacy_reply_still_returns_none(self):
        written = self.runtime.reply({'id': 'fixture-rpc', 'result': {'success': True}},
            'default', 'original-connection')
        self.assertIsNone(written)

    def test_app_server_write_returns_the_proxy_receipt(self):
        receipt = {'accepted': True, 'duplicate': True, 'remoteId': 'fixture-rpc'}
        calls = []
        def send(message, **kwargs):
            calls.append((message, kwargs))
            return receipt
        server = AppServer.__new__(AppServer)
        server.closed = False
        server.supervisor_mode = True
        server.write_lock = threading.Lock()
        server.proc = SimpleNamespace(poll=lambda: None, send_write=send)
        server.transcript_capture = SimpleNamespace(record=lambda *_args: None)
        response = {'id': 'fixture-rpc', 'result': {'success': True}}
        self.assertIs(server.write(response, operation_id='fixture-response-operation'), receipt)
        self.assertEqual(calls, [(response, {'operation_id': 'fixture-response-operation'})])


if __name__ == '__main__':
    unittest.main()

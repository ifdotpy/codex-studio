#!/usr/bin/env python3
"""Retain an unadmitted native request without repeating an admitted mutation."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
from contextlib import contextmanager
import copy
import io
import json
from pathlib import Path
import queue
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_process_supervisor import ProcessProxy
from codex_runtime import AppServer, Runtime


class NoNativeServer:
    def __init__(self, *_args, **_kwargs):
        raise AssertionError('This fixture cannot start a native transport')


class ResponseSink:
    def __init__(self):
        self.messages = []

    def write(self, message, **_kwargs):
        self.messages.append(copy.deepcopy(message))
        return {'accepted': True, 'duplicate': False}

    def close(self):
        pass

    def join_callbacks(self, **_kwargs):
        return True


class RetainedFrames:
    def __init__(self, message, following=None):
        self.messages = {7: copy.deepcopy(message)}
        if following is not None:
            self.messages[8] = copy.deepcopy(following)
        self.acknowledged = 6

    def remaining(self):
        return [(seq, copy.deepcopy(message)) for seq, message in self.messages.items()
                if seq > self.acknowledged]


class JournalProxy(ProcessProxy):
    """Use the real contiguous ACK method without sockets or native processes."""
    def __init__(self, frames):
        self.frames = frames
        self.cursor = frames.acknowledged
        self.read_cursor = max(frames.messages)
        self.ack_pending = set()
        self.event_lock = threading.RLock()
        self.detached = False
        self.detach_count = 0
        self.ack_calls = []

    def call(self, action, **values):
        if action != 'ack':
            raise AssertionError('No native or supervisor request is allowed')
        self.ack_calls.append(values['sequence'])
        self.frames.acknowledged = values['sequence']
        return {}

    def poll(self):
        return 0 if self.detached else None

    def detach(self):
        with self.event_lock:
            if not self.detached:
                self.detached = True
                self.detach_count += 1


class StaleToolRequestAckContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-stale-tool-request-')
        self.addCleanup(self.temp.cleanup)
        with patch.object(Runtime, 'schedule', lambda _runtime: None):
            self.runtime = Runtime(Path(self.temp.name), NoNativeServer)
        self.addCleanup(self.runtime.close)
        self.sink = ResponseSink()
        self.runtime.servers['default'] = self.sink
        self.runtime.connection_ids['default'] = 'old'
        self.actor = {'id': 'fixture-actor', 'name': 'Fixture actor', 'rootId': 'fixture-actor',
            'isLead': True, 'parentId': None, 'threadId': 'fixture-thread', 'turnId': 'fixture-turn',
            'inFlight': True, 'status': 'running', 'autoWake': True, 'epoch': 3, 'turnEpoch': 3,
            'events': 0, 'model': 'fixture-model', 'effort': 'medium', 'accountKey': 'default',
            'provider': 'claude', 'role': 'orchestrator', 'cwd': self.temp.name, 'created': 1,
            'prompt': 'Fixture', 'tokensUsed': 0, 'tokenBudget': None, 'tail': ''}
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.actor)
        self.message = {'id': 'claude:fixture-rpc', 'method': 'item/tool/call', 'params': {
            'threadId': self.actor['threadId'], 'turnId': self.actor['turnId'],
            'callId': 'fixture-call', 'tool': 'orchestration_task',
            'arguments': {'action': 'create', 'title': 'Fixture assignment'}}}
        self.key = self.runtime.tool_request_key(self.message)
        self.operations = 0
        original_work = self.runtime.model_work
        def work(*args, **kwargs):
            self.operations += 1
            return original_work(*args, **kwargs)
        self.work_patch = patch.object(self.runtime, 'model_work', work)
        self.work_patch.start()
        self.addCleanup(self.work_patch.stop)
        self.inline_patch = patch.object(self.runtime.coordination_pool, 'submit', self.inline_submit)
        self.inline_patch.start()
        self.addCleanup(self.inline_patch.stop)
        # Response projection is independent of admission and native ACK.
        projection = patch.object(self.runtime, 'model_tool_result', lambda _actor, _key, result: result)
        projection.start()
        self.addCleanup(projection.stop)

    @staticmethod
    def inline_submit(operation, *args):
        future = concurrent.futures.Future()
        try:
            future.set_result(operation(*args))
        except Exception as error:
            future.set_exception(error)
        return future

    def dispatcher(self, frames, connection):
        server = AppServer.__new__(AppServer)
        server.log = io.BytesIO()
        server.lock = threading.RLock()
        server.callback_lock = threading.RLock()
        server.callbacks = queue.Queue()
        server.reader_done = threading.Event()
        server.reader_done.set()
        server.dispatcher_done = threading.Event()
        server.closed = False
        server.transport_error = None
        server.supervisor_mode = True
        server.supervisor_event_applied = None
        server.supervisor_commit = None
        server.proc = JournalProxy(frames)
        server.request = lambda message: self.runtime.request(message, 'default', connection)
        server.notifications = []
        server.notification = lambda message: server.notifications.append(message)
        server.release_slot = lambda _message: False
        server.close_log_if_idle = lambda: None
        server.errors = []
        server.protocol_error = lambda error: server.errors.append(error)
        server.died = lambda: None
        for sequence, message in frames.remaining():
            message['_studioSupervisorSequence'] = sequence
            callback = server.request if 'id' in message else server.notification
            server.callbacks.put((callback, message))
        server.dispatch()
        self.assertTrue(server.dispatcher_done.is_set())
        return server

    def assert_unadmitted(self, frames, server):
        self.assertEqual(frames.acknowledged, 6)
        self.assertEqual(self.operations, 0)
        self.assertEqual(self.sink.messages, [])
        self.assertIsNone(self.runtime.tool_request(self.key))
        self.assertTrue(any(isinstance(error, ConnectionError) for error in server.errors))
        self.assertEqual(server.proc.detach_count, 1)

    def update_actor(self, **values):
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.actor['id'], db)
            actor.update(values)
            self.runtime.put(db, 'agents', actor)

    def test_stale_request_retains_exact_frame_and_replacement_executes_once(self):
        frames = RetainedFrames(self.message)
        self.runtime.connection_ids['default'] = 'new'
        old = self.dispatcher(frames, 'old')
        self.assert_unadmitted(frames, old)
        current = self.dispatcher(frames, 'new')
        self.assertEqual(current.errors, [])
        self.assertEqual(frames.acknowledged, 7)
        self.assertEqual(self.operations, 1)
        receipt = self.runtime.tool_request(self.key)
        self.assertEqual(receipt['callId'], self.message['params']['callId'])
        self.assertEqual(receipt['rpcId'], self.message['id'])
        self.assertEqual(receipt['outcome'], 'applied')
        self.assertEqual(self.sink.messages, [{'id': self.message['id'], 'result': receipt['result']}])
        self.dispatcher(frames, 'new')
        self.assertEqual(self.operations, 1)
        self.assertEqual(len(self.sink.messages), 1)
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_work').fetchone()[0], 1)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_operation_receipts WHERE id=?',
                (self.key,)).fetchone()[0], 1)

    def test_dispatcher_stops_before_following_frame_and_contiguous_ack_holds(self):
        frames = RetainedFrames(self.message, {'method': 'fixture/notification'})
        self.runtime.connection_ids['default'] = 'new'
        old = self.dispatcher(frames, 'old')
        self.assert_unadmitted(frames, old)
        self.assertEqual(old.notifications, [])
        self.assertEqual(old.callbacks.qsize(), 1)
        old.proc.ack(8)
        self.assertEqual(frames.acknowledged, 6)
        self.assertEqual(old.proc.ack_calls, [])
        current = self.dispatcher(frames, 'new')
        self.assertEqual(frames.acknowledged, 8)
        self.assertEqual(len(current.notifications), 1)
        self.assertEqual(self.operations, 1)

    def test_connection_change_before_reservation_retains_frame(self):
        frames = RetainedFrames(self.message)
        reserve = self.runtime.reserve_tool_request
        def changed(*args):
            self.runtime.connection_ids['default'] = 'new'
            return reserve(*args)
        with patch.object(self.runtime, 'reserve_tool_request', changed):
            old = self.dispatcher(frames, 'old')
        self.assert_unadmitted(frames, old)

    def test_runtime_close_before_admission_retains_frame(self):
        frames = RetainedFrames(self.message)
        self.runtime.closed = True
        try:
            server = self.dispatcher(frames, 'old')
        finally:
            self.runtime.closed = False
        self.assert_unadmitted(frames, server)

    def test_connection_change_before_approval_admission_retains_frame(self):
        message = {'id': 'fixture-approval', 'method': 'item/tool/requestUserInput',
                   'params': {'threadId': self.actor['threadId'], 'itemId': 'fixture-item'}}
        frames = RetainedFrames(message)
        original = self.runtime.db
        @contextmanager
        def changed(*args, **kwargs):
            with original(*args, **kwargs) as db:
                self.runtime.connection_ids['default'] = 'new'
                yield db
        with patch.object(self.runtime, 'db', changed):
            old = self.dispatcher(frames, 'old')
        self.assert_unadmitted(frames, old)
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_requests').fetchone()[0], 0)

    def test_replied_validation_failure_still_acknowledges_frame(self):
        message = copy.deepcopy(self.message)
        message['params']['threadId'] = 'unknown-thread'
        frames = RetainedFrames(message)
        server = self.dispatcher(frames, 'old')
        self.assertEqual(server.errors, [])
        self.assertEqual(frames.acknowledged, 7)
        self.assertEqual(self.operations, 0)
        self.assertEqual(len(self.sink.messages), 1)
        self.assertFalse(self.sink.messages[0]['result']['success'])

    def test_durable_admission_acknowledges_before_handler_and_does_not_replay(self):
        pending = []
        def hold(operation, *args):
            future = concurrent.futures.Future()
            pending.append((operation, args, future))
            return future
        frames = RetainedFrames(self.message)
        with patch.object(self.runtime.coordination_pool, 'submit', hold):
            server = self.dispatcher(frames, 'old')
        self.assertEqual(server.errors, [])
        self.assertEqual(frames.acknowledged, 7)
        self.assertEqual(self.runtime.tool_request(self.key)['stage'], 'queued')
        self.runtime.connection_ids['default'] = 'new'
        self.dispatcher(frames, 'new')
        self.assertEqual(len(pending), 1)
        operation, args, future = pending[0]
        future.set_result(operation(*args))
        self.assertEqual(self.operations, 0)
        self.assertEqual(self.runtime.tool_request(self.key)['outcome'], 'not_applied')

    def test_retained_request_cannot_restart_an_explicitly_stopped_actor(self):
        frames = RetainedFrames(self.message)
        self.runtime.connection_ids['default'] = 'new'
        self.assert_unadmitted(frames, self.dispatcher(frames, 'old'))
        self.update_actor(epoch=4, autoWake=False, status='paused', error='Stopped by user')
        self.dispatcher(frames, 'new')
        self.assertEqual(self.operations, 0)
        self.assertEqual(self.runtime.tool_request(self.key)['outcome'], 'not_applied')
        actor = self.runtime.agent(self.actor['id'])
        self.assertEqual((actor['epoch'], actor['autoWake'], actor['status']), (4, False, 'paused'))

    def test_unadmitted_old_turn_cannot_execute_after_stop_and_resume(self):
        self.update_actor(epoch=4, autoWake=True, turnEpoch=3)
        frames = RetainedFrames(self.message)
        self.dispatcher(frames, 'old')
        self.assertEqual(self.operations, 0)
        self.assertEqual(self.runtime.tool_request(self.key)['outcome'], 'not_applied')

    def test_old_queued_receipt_cannot_execute_after_stop_and_resume(self):
        self.runtime.reserve_tool_request(self.message, 'default', 'old')
        self.update_actor(epoch=4, autoWake=True, turnEpoch=4)
        frames = RetainedFrames(self.message)
        self.dispatcher(frames, 'old')
        self.assertEqual(self.operations, 0)
        self.assertEqual(self.runtime.tool_request(self.key)['outcome'], 'not_applied')

    def test_stop_after_admission_before_executor_start_preserves_epoch_guard(self):
        pending = []
        def hold(operation, *args):
            future = concurrent.futures.Future()
            pending.append((operation, args, future))
            return future
        frames = RetainedFrames(self.message)
        with patch.object(self.runtime.coordination_pool, 'submit', hold):
            self.dispatcher(frames, 'old')
        self.assertEqual(frames.acknowledged, 7)
        self.assertEqual(self.runtime.tool_request(self.key)['epoch'], 3)
        self.update_actor(epoch=4, autoWake=True, turnEpoch=4)
        operation, args, future = pending[0]
        future.set_result(operation(*args))
        self.assertEqual(self.operations, 0)
        self.assertEqual(self.runtime.tool_request(self.key)['outcome'], 'not_applied')

    def test_completed_receipt_keeps_exact_result_after_epoch_change(self):
        self.dispatcher(RetainedFrames(self.message), 'old')
        saved = copy.deepcopy(self.runtime.tool_request(self.key))
        self.update_actor(epoch=4, autoWake=True, turnEpoch=4)
        frames = RetainedFrames(self.message)
        self.dispatcher(frames, 'old')
        self.assertEqual(frames.acknowledged, 7)
        self.assertEqual(self.operations, 1)
        self.assertEqual(self.runtime.tool_request(self.key), saved)
        self.assertEqual(self.sink.messages, [
            {'id': self.message['id'], 'result': saved['result']},
            {'id': self.message['id'], 'result': saved['result']}])

    def test_durable_result_precedes_incomplete_old_queued_metadata(self):
        self.dispatcher(RetainedFrames(self.message), 'old')
        receipt = self.runtime.tool_request(self.key)
        saved_result = copy.deepcopy(receipt['result'])
        # Model completion is durable, but its metadata row is incomplete.
        receipt.update(stage='queued', outcome='pending')
        for field in ['result', 'started', 'finished']:
            receipt.pop(field, None)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'tool_requests', receipt)
        self.update_actor(epoch=4, autoWake=True, turnEpoch=4)
        self.sink.messages.clear()
        self.dispatcher(RetainedFrames(self.message), 'old')
        self.assertEqual(self.operations, 1)
        self.assertEqual(self.runtime.tool_request(self.key)['outcome'], 'applied')
        self.assertEqual(self.sink.messages, [{'id': self.message['id'], 'result': saved_result}])

    def test_operation_receipt_prevents_false_not_applied_and_repeat(self):
        self.runtime.reserve_tool_request(self.message, 'default', 'old')
        self.runtime.model_work(self.actor['id'], self.message['params']['arguments'], self.key,
                                self.actor['epoch'])
        with self.runtime.read_db() as db:
            self.assertIsNone(self.runtime.tool_result(db, self.key))
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_operation_receipts WHERE id=?',
                (self.key,)).fetchone()[0], 1)
        self.update_actor(epoch=4, autoWake=True, turnEpoch=4)
        self.dispatcher(RetainedFrames(self.message), 'old')
        self.assertEqual(self.operations, 1)
        receipt = self.runtime.tool_request(self.key)
        self.assertEqual((receipt['stage'], receipt['outcome']), ('queued', 'pending'))
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_work').fetchone()[0], 1)

    def test_exact_unknown_receipt_remains_unknown_without_mutation_replay(self):
        record = self.runtime.reserve_tool_request(self.message, 'default', 'old')
        record.update(stage='interrupted', outcome='unknown', started=1)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'tool_requests', record)
        self.update_actor(epoch=4, autoWake=True, turnEpoch=4)
        frames = RetainedFrames(self.message)
        self.dispatcher(frames, 'old')
        self.assertEqual(frames.acknowledged, 7)
        self.assertEqual(self.operations, 0)
        receipt = self.runtime.tool_request(self.key)
        self.assertEqual((receipt['stage'], receipt['outcome'], receipt['epoch']),
                         ('interrupted', 'unknown', 3))
        value = json.loads(self.sink.messages[0]['result']['contentItems'][0]['text'])
        self.assertEqual(value['requestId'], self.key)
        self.assertEqual(value['outcome'], 'unknown')

    def test_ambiguous_queued_receipt_is_not_changed_to_not_applied(self):
        record = self.runtime.reserve_tool_request(self.message, 'default', 'old')
        record.update(outcome='unknown')
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'tool_requests', record)
        self.update_actor(epoch=4, autoWake=True, turnEpoch=4)
        self.dispatcher(RetainedFrames(self.message), 'old')
        self.assertEqual(self.operations, 0)
        self.assertEqual(self.runtime.tool_request(self.key), record)
        report = json.loads(self.sink.messages[0]['result']['contentItems'][0]['text'])
        self.assertEqual(report['outcome'], 'unknown')


if __name__ == '__main__':
    unittest.main()

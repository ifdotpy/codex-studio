#!/usr/bin/env python3
"""Admit supervised tools before a stream tail without crossing durable barriers."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from collections import deque
import concurrent.futures
import copy
import io
from pathlib import Path
import queue
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_process_supervisor import ProcessProxy
from codex_runtime import AppServer, Runtime


def stream(sequence, method='item/agentMessage/delta', **values):
    return {'method': method, '_studioSupervisorSequence': sequence,
            'params': {'threadId': 'fixture-thread', 'turnId': 'fixture-turn',
                       'itemId': 'fixture-text', 'delta': 'x', **values}}


def tool(sequence, call='fixture-call'):
    return {'id': 'claude:' + call, 'method': 'item/tool/call',
            '_studioSupervisorSequence': sequence, 'params': {
                'threadId': 'fixture-thread', 'turnId': 'fixture-turn',
                'callId': call, 'tool': 'orchestration_task',
                'arguments': {'action': 'create', 'title': 'Fixture task'}}}


def server_fixture(supervised=True, limit=AppServer.CALLBACK_QUEUE_LIMIT):
    server = AppServer.__new__(AppServer)
    server.supervisor_mode = supervised
    server.callbacks = queue.Queue(maxsize=limit)
    server.callback_lock = threading.RLock()
    server.lock = threading.RLock()
    server.dispatch_stopped = False
    server.notification = lambda _message: None
    server.request = lambda _message: None
    server.tool_requests = queue.Queue()
    server.clock_replies = queue.Queue()
    server.errors = []
    server.fail_transport = lambda error: server.errors.append(error)
    return server


def drain_queue(server):
    entries = []
    while not server.callbacks.empty():
        entries.append(server.callbacks.get_nowait())
        server.callbacks.task_done()
    server.callbacks.join()
    return entries


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


class ReceiptProxy(ProcessProxy):
    """Exercise contiguous ACK and stream batches without sockets or children."""
    def __init__(self, count):
        self.handle = 'fixture-handle'
        self.cursor = 0
        self.read_cursor = count
        self.ack_pending = set()
        self.ack_calls = []
        self.before_ack = lambda _sequence: None

    def call(self, action, **values):
        if action != 'ack':
            raise AssertionError('No native or supervisor command is allowed')
        self.ack_calls.append(values['sequence'])
        return {}

    def ack(self, sequence):
        self.before_ack(sequence)
        return super().ack(sequence)

    def ack_applied_deltas(self, _applied):
        return 0


class TailPriorityContract(unittest.TestCase):
    def test_tool_moves_before_3000_stream_fragments_after_lifecycle(self):
        server = server_fixture()
        lifecycle = {'method': 'turn/started', '_studioSupervisorSequence': 1,
                     'params': {'threadId': 'fixture-thread', 'turn': {'id': 'fixture-turn'}}}
        server.enqueue(server.notification, lifecycle)
        fragments = [stream(sequence) for sequence in range(2, 3002)]
        for message in fragments:
            server.enqueue(server.notification, message)
        request = tool(3002)
        server.enqueue(server.request, request)
        self.assertEqual(server.callbacks.unfinished_tasks, 3002)
        entries = drain_queue(server)
        self.assertIs(entries[0][1], lifecycle)
        self.assertIs(entries[1][1], request)
        self.assertEqual([message for callback, message in entries if callback == server.notification],
                         [lifecycle, *fragments])
        self.assertEqual(server.callbacks.unfinished_tasks, 0)
        self.assertEqual(server.tool_requests.qsize(), 0)

    def test_barriers_hold_every_earlier_callback_and_notification_order(self):
        barriers = [
            {'method': 'turn/started'}, {'method': 'turn/completed'},
            {'method': 'item/started'}, {'method': 'item/completed'},
            {'method': 'error'}, {'method': 'thread/tokenUsage/updated'},
            {'id': 'approval', 'method': 'item/tool/requestUserInput'},
            {'_studioReattachBarrier': True}, {}, None,
        ]
        for barrier in barriers:
            with self.subTest(barrier=barrier):
                server = server_fixture()
                before, after, request = stream(1), stream(3), tool(4)
                server.enqueue(server.notification, before)
                server.enqueue(server.notification, barrier)
                server.enqueue(server.notification, after)
                server.enqueue(server.request, request)
                self.assertEqual([message for _, message in drain_queue(server)],
                                 [before, barrier, request, after])

    def test_receipts_after_events_and_callback_aliases_are_barriers(self):
        for opaque in [lambda _value: None, lambda _value: None]:
            server = server_fixture()
            server.enqueue(server.notification, stream(1))
            barrier = stream(2)
            server.enqueue(opaque, barrier)
            tail, request = stream(3), tool(4)
            server.enqueue(server.notification, tail)
            server.enqueue(server.request, request)
            entries = drain_queue(server)
            self.assertIs(entries[1][0], opaque)
            self.assertIs(entries[1][1], barrier)
            self.assertEqual([message for _, message in entries[2:]], [request, tail])

    def test_invalid_or_merged_stream_fragment_remains_a_barrier(self):
        invalid = [
            {'params': None}, {'params': {'delta': 'x'}},
            {'params': {**stream(2)['params'], 'delta': 1}},
            {'params': {**stream(2)['params'], 'threadId': ''}},
            {'params': {**stream(2)['params'], 'turnId': None}},
            {'params': {**stream(2)['params'], 'itemId': 7}},
            {'id': 'not-a-notification'}, {'_studioSupervisorSequence': None},
            {'_studioSupervisorSequence': True}, {'_studioSupervisorSequence': 0},
            {'_studioSupervisorSequence': 5}, {'_studioSlot': {}},
            {'_studioLatestSlot': {}}, {'_studioNotificationSamples': []},
            {'method': 'item/reasoning/textDelta'},
            {'method': 'command/exec/outputDelta'},
        ]
        for changes in invalid:
            with self.subTest(changes=changes):
                server = server_fixture()
                barrier, tail, request = {**stream(2), **changes}, stream(3), tool(4)
                server.enqueue(server.notification, barrier)
                server.enqueue(server.notification, tail)
                server.enqueue(server.request, request)
                self.assertEqual([message for _, message in drain_queue(server)],
                                 [barrier, request, tail])

    def test_multiple_tools_keep_request_and_notification_order(self):
        server = server_fixture()
        fragments = [stream(1), stream(2, 'item/commandExecution/outputDelta')]
        first, second, third = tool(3, 'first'), tool(4, 'second'), tool(6, 'third')
        for message in fragments:
            server.enqueue(server.notification, message)
        server.enqueue(server.request, first)
        server.enqueue(server.request, second)
        last = stream(5)
        server.enqueue(server.notification, last)
        server.enqueue(server.request, third)
        entries = drain_queue(server)
        self.assertEqual([message for callback, message in entries if callback == server.request],
                         [first, second, third])
        self.assertEqual([message for callback, message in entries if callback == server.notification],
                         [*fragments, last])
        self.assertEqual([message for _, message in entries], [first, second, third, *fragments, last])

    def test_capacity_failure_retains_entries_and_unfinished_tasks(self):
        server = server_fixture(limit=2)
        for sequence in (1, 2):
            server.enqueue(server.notification, stream(sequence))
        with self.assertRaisesRegex(RuntimeError, 'queue saturated'):
            server.enqueue(server.request, tool(3))
        self.assertEqual(len(server.errors), 1)
        self.assertEqual(server.callbacks.unfinished_tasks, 2)
        self.assertEqual([message['_studioSupervisorSequence'] for _, message in drain_queue(server)], [1, 2])

    def test_non_supervised_coalescing_and_tool_queue_stay_unchanged(self):
        server = server_fixture(supervised=False)
        server.enqueue(server.notification, stream(1))
        server.enqueue(server.notification, stream(2))
        self.assertEqual(server.callbacks.qsize(), 1)
        request = tool(3)
        server.enqueue(server.request, request)
        self.assertIs(server.tool_requests.get_nowait(), request)
        server.enqueue(server.notification, stream(4))
        entries = drain_queue(server)
        self.assertEqual([message['params']['delta'] for _, message in entries], ['xx', 'x'])
        self.assertEqual([len(message.get('_studioNotificationSamples', [None])) for _, message in entries], [2, 1])

    def test_only_exact_sequenced_tool_requests_receive_priority(self):
        invalid = [
            {'method': 'item/tool/requestUserInput'}, {'method': 'fixture/request'},
            {'_studioSupervisorSequence': None}, {'_studioSupervisorSequence': True},
            {'_studioSupervisorSequence': 0}, {'id': None}, {'id': True},
        ]
        for changes in invalid:
            with self.subTest(changes=changes):
                server = server_fixture()
                fragment, request = stream(1), {**tool(2), **changes}
                server.enqueue(server.notification, fragment)
                server.enqueue(server.request, request)
                self.assertEqual([message for _, message in drain_queue(server)], [fragment, request])

    def test_queue_put_wakes_a_waiting_consumer(self):
        server = server_fixture()
        waiting, received = threading.Event(), threading.Event()
        entries = []
        def consume():
            waiting.set()
            entries.append(server.callbacks.get(timeout=2))
            server.callbacks.task_done()
            received.set()
        consumer = threading.Thread(target=consume)
        consumer.start()
        self.assertTrue(waiting.wait(1))
        request = tool(1)
        server.enqueue(server.request, request)
        consumer.join(2)
        self.assertFalse(consumer.is_alive())
        self.assertTrue(received.is_set())
        self.assertEqual(entries, [(server.request, request)])
        self.assertEqual(server.callbacks.unfinished_tasks, 0)

    def test_maximum_queue_scan_is_linear_and_bounded(self):
        class CountedDeque(deque):
            visited = 0
            def __reversed__(self):
                for entry in super().__reversed__():
                    self.visited += 1
                    yield entry
        server = server_fixture()
        with server.callbacks.mutex:
            server.callbacks.queue = CountedDeque(
                (server.notification, stream(sequence)) for sequence in range(1, AppServer.CALLBACK_QUEUE_LIMIT))
            server.callbacks.unfinished_tasks = len(server.callbacks.queue)
        request = tool(AppServer.CALLBACK_QUEUE_LIMIT)
        began = time.monotonic()
        server.enqueue(server.request, request)
        duration = time.monotonic() - began
        self.assertEqual(server.callbacks.queue.visited, AppServer.CALLBACK_QUEUE_LIMIT - 1)
        self.assertIs(server.callbacks.queue[0][1], request)
        self.assertEqual(server.callbacks.unfinished_tasks, AppServer.CALLBACK_QUEUE_LIMIT)
        self.assertLess(duration, 2, 'The bounded queue scan must not take seconds')


class DurableDispatchContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-supervised-tool-tail-')
        self.addCleanup(self.temp.cleanup)
        with patch.object(Runtime, 'schedule', lambda _runtime: None):
            self.runtime = Runtime(Path(self.temp.name), NoNativeServer)
        self.addCleanup(self.runtime.close)
        self.sink = ResponseSink()
        self.runtime.servers['default'] = self.sink
        self.runtime.connection_ids['default'] = 'fixture-connection'
        self.actor = {'id': 'fixture-actor', 'name': 'Fixture actor', 'rootId': 'fixture-actor',
            'isLead': True, 'parentId': None, 'threadId': 'fixture-thread', 'turnId': None,
            'inFlight': True, 'status': 'starting', 'autoWake': True, 'epoch': 3, 'turnEpoch': 3,
            'events': 0, 'model': 'fixture-model', 'effort': 'medium', 'accountKey': 'default',
            'provider': 'claude', 'role': 'orchestrator', 'cwd': self.temp.name, 'created': 1,
            'prompt': 'Fixture', 'tokensUsed': 0, 'tokenBudget': None, 'tail': ''}
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.actor)
        self.server = server_fixture()
        self.server.closed = True
        self.server.reader_done = threading.Event()
        self.server.reader_done.set()
        self.server.dispatcher_done = threading.Event()
        self.server.close_log_if_idle = lambda: None
        self.server.log = io.BytesIO()
        self.server.protocol_error = lambda error: self.server.errors.append(error)
        self.server.supervisor_event_applied = None
        self.server.proc = ReceiptProxy(20)
        self.delivered = []
        def notification(message):
            self.delivered.append(message['_studioSupervisorSequence'])
            self.runtime.notification(message, 'default', 'fixture-connection')
        self.server.notification = notification
        self.server.request = lambda message: self.runtime.request(message, 'default', 'fixture-connection')
        self.server.supervisor_commit = lambda message, sequence: self.runtime.commit_supervisor_event(
            self.server.proc.handle, message, sequence, 'default', 'fixture-connection')

    def populate(self):
        started = {'method': 'turn/started', '_studioSupervisorSequence': 1,
                   'params': {'threadId': 'fixture-thread', 'turn': {'id': 'fixture-turn'}}}
        item = {'id': 'fixture-call', 'type': 'dynamicToolCall', 'tool': 'orchestration_task',
                'arguments': {'action': 'create', 'title': 'Fixture task'}}
        item_started = {'method': 'item/started', '_studioSupervisorSequence': 2,
                        'params': {'threadId': 'fixture-thread', 'turnId': 'fixture-turn', 'item': item}}
        for message in [started, item_started, *[stream(sequence) for sequence in range(3, 19)]]:
            self.server.enqueue(self.server.notification, message)
        self.request = tool(19)
        self.key = self.runtime.tool_request_key(self.request)
        self.server.enqueue(self.server.request, self.request)

    def test_dispatch_reserves_exact_receipt_before_ack_and_retains_lower_gaps(self):
        self.populate()
        held = []
        def hold(operation, *args):
            future = concurrent.futures.Future()
            held.append((operation, args, future))
            return future
        observed = []
        def before_ack(sequence):
            if sequence == 19:
                receipt = self.runtime.tool_request(self.key)
                with self.runtime.read_db() as db:
                    cursor = db.execute('SELECT sequence FROM runtime_supervisor_cursor WHERE handle=?',
                        (self.server.proc.handle,)).fetchone()[0]
                observed.append((receipt['id'], receipt['stage'], receipt['epoch'], receipt['turnId'],
                                 self.server.proc.cursor, cursor, list(self.delivered)))
        self.server.proc.before_ack = before_ack
        with patch.object(self.runtime.coordination_pool, 'submit', hold):
            self.server.dispatch()
        self.assertEqual(self.server.errors, [])
        self.assertEqual(observed, [(self.key, 'queued', 3, 'fixture-turn', 2, 2, [1, 2])])
        self.assertEqual(self.server.proc.cursor, 19)
        self.assertEqual(self.server.proc.ack_pending, set())
        self.assertEqual(self.server.callbacks.unfinished_tasks, 0)
        self.assertEqual(len(held), 1)
        self.assertEqual(self.sink.messages, [])
        operation, args, future = held[0]
        future.set_result(operation(*args))
        receipt = self.runtime.tool_request(self.key)
        self.assertEqual((receipt['stage'], receipt['outcome']), ('completed', 'applied'))
        self.assertEqual(len(self.sink.messages), 1)
        self.assertEqual(self.delivered, list(range(1, 19)))
        # The native result notice follows every earlier stream fragment.
        completed = {'method': 'item/completed', '_studioSupervisorSequence': 20, 'params': {
            'threadId': 'fixture-thread', 'turnId': 'fixture-turn', 'item': {
                'id': 'fixture-call', 'type': 'dynamicToolCall', 'tool': 'orchestration_task',
                'success': True, 'contentItems': receipt['result']['contentItems']}}}
        self.server.dispatch_stopped = False
        self.server.enqueue(self.server.notification, completed)
        self.server.dispatch()
        self.assertEqual(self.server.proc.cursor, 20)
        self.assertEqual(self.runtime.agent(self.actor['id'])['activeTools'], [])
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_work').fetchone()[0], 1)
        # A repeated exact call reads its receipt without a second task mutation.
        def inline(operation, *args):
            future = concurrent.futures.Future()
            future.set_result(operation(*args))
            return future
        with patch.object(self.runtime.coordination_pool, 'submit', inline):
            self.runtime.request(copy.deepcopy(self.request), 'default', 'fixture-connection')
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_work').fetchone()[0], 1)
        self.assertEqual(len(self.sink.messages), 2)

    def test_explicit_stop_before_admission_is_preserved(self):
        self.populate()
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.actor['id'], db)
            actor.update(autoWake=False, status='paused', epoch=4, turnEpoch=3, error='Stopped by user')
            self.runtime.put(db, 'agents', actor)
        future = concurrent.futures.Future()
        def inline(operation, *args):
            future.set_result(operation(*args))
            return future
        with patch.object(self.runtime.pool, 'submit', lambda *_args: concurrent.futures.Future()), \
                patch.object(self.runtime.coordination_pool, 'submit', inline):
            self.server.dispatch()
        actor = self.runtime.agent(self.actor['id'])
        self.assertEqual((actor['autoWake'], actor['epoch'], actor['status']), (False, 4, 'paused'))
        self.assertEqual(self.runtime.tool_request(self.key)['outcome'], 'not_applied')
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_work').fetchone()[0], 0)


if __name__ == '__main__':
    unittest.main()

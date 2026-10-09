#!/usr/bin/env python3
"""A durable supervisor write publishes its response ID before native delivery."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
from pathlib import Path
import queue
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_process_supervisor as supervisor
from codex_runtime import AppServer, SubmissionUnknown


class ReceiptGate(dict):
    def __init__(self, result, entered, release):
        super().__init__(result)
        self.entered, self.release = entered, release

    def __getitem__(self, key):
        if key == 'durableMs':
            self.entered.set()
            if not self.release.wait(2):
                raise TimeoutError('The fixture did not release the write receipt')
        return super().__getitem__(key)


class ResponseMap(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='supervisor-response-map-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.events = queue.Queue()
        self.writes, self.requests, self.frames, self.errors = [], [], [], []
        self.receipt_entered = threading.Event()
        self.release_receipt = threading.Event()
        self.response_fetched = threading.Event()
        self.response_recorded = threading.Event()
        self.gate_receipt = False
        self.lose_receipt = False
        self.journal = supervisor.Journal(self.root)
        self.handle = 'account:fixture'
        with self.journal.db() as db:
            db.execute('INSERT INTO handles(id,signature,pid,rpc_sequence,created,generation) '
                       'VALUES (?,?,?,?,?,?)', (self.handle, 'fixture', 1, 40, 1, 1))
        self.child = supervisor.Child.__new__(supervisor.Child)
        self.child.handle = self.handle
        self.child.lock = threading.RLock()
        self.child.process = SimpleNamespace(supervisor=SimpleNamespace(journal=self.journal),
            stdin=SimpleNamespace(write=self.native_write, flush=lambda:None))
        self.proxy = supervisor.ProcessProxy.__new__(supervisor.ProcessProxy)
        self.proxy.handle = self.handle
        self.proxy.root = self.root
        self.proxy.socket = self.proxy.reader = None
        self.proxy.write_lock = threading.Lock()
        self.proxy.event_lock = threading.RLock()
        self.proxy.request_id = 0
        self.proxy.detached = False
        self.proxy.generation = 1
        self.proxy.cursor = self.proxy.read_cursor = self.proxy.sequence = 0
        self.proxy.ack_pending = set()
        self.proxy.remote_to_local = {}
        self.proxy._returncode = None
        self.proxy.stdout = supervisor._Stdout(self.proxy)
        self.proxy.stderr_sink = lambda data:None
        self.server = AppServer.__new__(AppServer)
        self.server.proc = self.proxy
        self.server.closed = False
        self.server.supervisor_mode = True
        self.server.lock = threading.RLock()
        self.server.write_lock = threading.RLock()
        self.server.sequence = 0
        self.server.pending = {}
        self.server.transport_error = None
        self.server.transcript_capture = SimpleNamespace(record=self.record)
        self.server.enqueue = lambda callback, message:callback(message)
        self.server.protocol_error = self.errors.append
        self.server.reader_done = threading.Event()
        self.server.close_log_if_idle = lambda:None
        self.threads = []
        self.ticket, self.failure = None, None
        self.patches = [patch.object(supervisor, '_send', side_effect=self.send),
                        patch.object(supervisor, '_recv', side_effect=self.receive)]
        for replacement in self.patches:
            replacement.start()
            self.addCleanup(replacement.stop)
        self.addCleanup(self.stop_threads)

    def native_write(self, raw):
        message = json.loads(raw)
        self.writes.append(message)
        if 'method' in message:
            self.events.put({'sequence':len(self.writes), 'generation':1, 'kind':'stdout',
                'payload':json.dumps({'id':message['id'], 'result':{'nativeId':message['id']}})})
        return len(raw)

    def send(self, socket, request, lock=None):
        self.requests.append(request)

    def receive(self, socket, stream):
        request = self.requests[-1]
        action = request['action']
        if action == 'write':
            try:
                result = self.child.write(request['operationId'], request['nativeId'], request['message'])
            except Exception as error:
                return {'requestId':request['requestId'], 'error':str(error)}
            if self.lose_receipt:
                self.lose_receipt = False
                raise ConnectionError('The exact accepted fixture write receipt was lost')
            if self.gate_receipt:
                result = ReceiptGate(result, self.receipt_entered, self.release_receipt)
        elif action == 'next':
            try:
                event = self.events.get(timeout=.02)
            except queue.Empty:
                event = None
            if event:
                self.response_fetched.set()
            result = {'event':event, 'returnCode':None}
        elif action == 'ack':
            result = {}
        else:
            raise AssertionError('Unexpected fixture action: ' + action)
        return {'requestId':request['requestId'], 'result':result}

    def record(self, direction, message):
        if direction == 'in':
            self.frames.append(message)
            self.response_recorded.set()

    def thread(self, function):
        thread = threading.Thread(target=function, daemon=True)
        self.threads.append(thread)
        thread.start()
        return thread

    def stop_threads(self):
        self.release_receipt.set()
        self.proxy.detached = True
        for thread in self.threads:
            thread.join(2)
            self.assertFalse(thread.is_alive(), 'The fixture thread did not finish')

    def submit(self, params=None):
        return self.server.submit('thread/read', params or {'threadId':'fixture-thread'},
                                  operation_id='read:fixture-exact')

    def gated_submit(self):
        try:
            self.ticket = self.submit()
        except Exception as error:
            self.failure = error

    def test_immediate_native_response_waits_for_local_id_publication(self):
        self.gate_receipt = True
        writer = self.thread(self.gated_submit)
        self.assertTrue(self.receipt_entered.wait(1))
        self.assertEqual(self.writes[0]['id'], 41)
        self.thread(self.server.read)
        try:
            self.assertTrue(self.response_fetched.wait(1))
            self.assertFalse(self.response_recorded.wait(.1),
                             'The accepted response escaped before its local ID was published')
        finally:
            self.release_receipt.set()
        writer.join(1)
        self.assertFalse(writer.is_alive())
        self.assertIsNone(self.failure)
        self.assertEqual(self.server.wait(self.ticket, timeout=1), {'nativeId':41})
        self.assertEqual([frame['id'] for frame in self.frames], [self.ticket[0]])
        self.assertEqual(len(self.writes), 1)
        self.assertEqual(self.errors, [])
        self.assertEqual(self.proxy.remote_to_local, {41:self.ticket[0]})

    def test_lost_acceptance_receipt_stays_unknown_and_exact_retry_does_not_resend(self):
        self.lose_receipt = True
        with self.assertRaises(SubmissionUnknown) as caught:
            self.submit()
        original = caught.exception.submitted
        self.assertFalse(original[2].done())
        self.assertEqual(self.proxy.remote_to_local, {})
        recovered = self.submit()
        self.assertNotEqual(original[0], recovered[0])
        self.thread(self.server.read)
        self.assertEqual(self.server.wait(recovered, timeout=1), {'nativeId':41})
        self.assertFalse(original[2].done())
        self.assertEqual(len(self.writes), 1)
        with self.journal.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM operations').fetchone()[0], 1)
        self.assertEqual(self.proxy.remote_to_local, {41:recovered[0]})

    def test_reused_operation_with_changed_body_preserves_original_binding(self):
        original = self.submit()
        with self.assertRaisesRegex(RuntimeError, 'different content'):
            self.proxy.send_write({'id':99, 'method':'thread/read', 'params':{'threadId':'other'}},
                                  operation_id='read:fixture-exact')
        self.assertEqual(self.proxy.remote_to_local, {41:original[0]})
        self.thread(self.server.read)
        self.assertEqual(self.server.wait(original, timeout=1), {'nativeId':41})
        self.assertEqual(len(self.writes), 1)

    def test_native_request_ids_are_not_remapped_as_responses(self):
        self.events.put({'sequence':1,'generation':1,'kind':'stdout',
            'payload':json.dumps({'id':41,'method':'approval/request','params':{}})})
        sequence, raw = self.proxy.next_event()
        self.assertEqual(sequence, 1)
        self.assertEqual(json.loads(raw)['id'], 41)
        self.assertEqual(self.proxy.remote_to_local, {})

    def test_integer_native_reply_is_durable_once_without_allocating_a_client_id(self):
        reply = {'id':41, 'result':{'clock':'fixture-clock'}}
        accepted = self.child.write('reply:fixture-exact', 41, reply)
        duplicate = self.child.write('reply:fixture-exact', 41, reply)
        self.assertEqual((accepted['accepted'], accepted['duplicate'], accepted['remoteId']),
                         (True, False, 41))
        self.assertEqual((duplicate['accepted'], duplicate['duplicate'], duplicate['remoteId']),
                         (True, True, 41))
        self.assertEqual(self.writes, [reply])
        with self.journal.db() as db:
            receipt = db.execute('SELECT native_id,generation FROM operations '
                                 'WHERE handle=? AND operation_id=?',
                                 (self.handle, 'reply:fixture-exact')).fetchone()
            self.assertEqual(tuple(receipt), (41, 1))
            self.assertEqual(db.execute('SELECT rpc_sequence FROM handles WHERE id=?',
                                       (self.handle,)).fetchone()[0], 40)

    def test_integer_native_reply_cannot_replace_a_pending_client_response_binding(self):
        original = self.submit()
        # Isolate the client guard from the independent server generation bug.
        with patch.object(self.child, 'write', return_value={
                'accepted':True, 'duplicate':False, 'remoteId':41, 'durableMs':0}):
            receipt = self.proxy.send_write({'id':41, 'result':{'clock':'fixture-clock'}},
                                           operation_id='reply:fixture-exact')
        self.assertTrue(receipt['accepted'])
        self.assertEqual(self.proxy.remote_to_local, {41:original[0]})
        self.thread(self.server.read)
        self.assertEqual(self.server.wait(original, timeout=1), {'nativeId':41})
        self.assertEqual(len(self.writes), 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)

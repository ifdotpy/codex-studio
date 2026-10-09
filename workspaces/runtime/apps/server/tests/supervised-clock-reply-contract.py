#!/usr/bin/env python3
"""Clock replies bypass callbacks without advancing past unapplied events."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import json
import os
from pathlib import Path
import queue
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_process_supervisor import Child, Journal, ProcessProxy
from codex_runtime import AppServer


class SavedPipe:
    def __init__(self, process):
        self.process = process
        self.lines = queue.Queue()
        self.current_sequence = None

    def __iter__(self):
        while (event := self.lines.get()) is not None:
            self.current_sequence, line = event
            self.process.read_cursor = self.current_sequence
            yield line


class SavedProcess:
    """Use the production journal write and ACK code with an in-memory child."""
    send_write = ProcessProxy.send_write
    _operation_identity = ProcessProxy._operation_identity

    def __init__(self, root):
        self.root = root
        self.handle = 'account:default'
        self.generation = 1
        self.initialize_result = {}
        self.resumed = False
        self.remote_to_local = {}
        self.stderr = None
        self.stdout = SavedPipe(self)
        self.cursor = self.read_cursor = self.sequence = 0
        self.ack_pending = set()
        self.event_lock = threading.RLock()
        self.condition = threading.Condition()
        self.writes = []
        self.reply_started = threading.Event()
        self.reply_release = threading.Event()
        self.reply_release.set()
        self.fail_reply = False
        self.fail_ack = False
        self.pause_ack = False
        self.ack_started = threading.Event()
        self.ack_release = threading.Event()
        self.ack_release.set()
        self.exited = threading.Event()
        self.journal = Journal(root)
        with self.journal.db() as db:
            db.execute('INSERT INTO handles(id,signature,pid,created,generation) VALUES (?,?,?,?,?)',
                       (self.handle, 'fixture', 0, time.time(), 1))
        self.child = Child.__new__(Child)
        self.child.handle = self.handle
        self.child.lock = threading.RLock()
        self.child.process = SimpleNamespace(
            supervisor=SimpleNamespace(journal=self.journal), stdin=self)

    def emit(self, message):
        self.sequence += 1
        payload = json.dumps(message)
        with self.journal.db() as db:
            db.execute('INSERT INTO events VALUES (?,?,?,?,?,?)',
                       (self.handle, self.sequence, 'stdout', payload, len(payload.encode()), 1))
            db.execute('UPDATE handles SET sequence=? WHERE id=?', (self.sequence, self.handle))
        self.stdout.lines.put((self.sequence, payload + '\n'))

    def write(self, text):
        with self.condition:
            self.writes.append(json.loads(text))
            self.condition.notify_all()

    def flush(self):
        pass

    def ack(self, sequence):
        if sequence == 2:
            if self.fail_ack:
                raise OSError('Fixture clock ACK failed')
            if self.pause_ack:
                self.ack_started.set()
                if not self.ack_release.wait(3):
                    raise AssertionError('Fixture ACK was not released')
        return ProcessProxy.ack(self, sequence)

    def call(self, action, **values):
        if action == 'write':
            message = values['message']
            if 'result' in message:
                self.reply_started.set()
                if not self.reply_release.wait(3):
                    raise AssertionError('Fixture reply was not released')
                if self.fail_reply:
                    raise OSError('Fixture durable reply failed')
            return self.child.write(values['operationId'], values['nativeId'], copy.deepcopy(message))
        if action != 'ack':
            raise AssertionError(action)
        with self.journal.db() as db:
            db.execute('UPDATE handles SET acknowledged=? WHERE id=?',
                       (values['sequence'], self.handle))
            db.execute('DELETE FROM events WHERE handle=? AND sequence<=?',
                       (self.handle, values['sequence']))
        return {}

    def poll(self):
        return 0 if self.exited.is_set() else None

    def terminate(self):
        if not self.exited.is_set():
            self.exited.set()
            self.stdout.lines.put(None)

    def detach(self):
        self.exited.set()
        self.stdout.lines.put(None)

    def wait(self, timeout=None):
        if not self.exited.wait(timeout):
            raise AssertionError('Fixture transport did not close')
        return 0


class SupervisedClock(unittest.TestCase):
    CLOCK_ID = 317

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-supervised-clock-')
        self.root = Path(self.temp.name)
        self.release = threading.Event()
        self.entered = threading.Event()
        self.applied = []
        self.proc = SavedProcess(self.root)
        def notification(message):
            if message['method'] == 'blocked':
                self.entered.set()
                if not self.release.wait(3):
                    raise AssertionError('Fixture callback was not released')
            self.applied.append(message['method'])
        def request(message):
            self.server.write({'id':message['id'], 'result':{'currentTimeAt':int(time.time())}})
        with patch.dict(os.environ, CODEX_AGENTS_SUPERVISOR_MODE='1'), \
                patch('codex_process_supervisor.attach', return_value=self.proc):
            self.server = AppServer(self.root, notification, request, lambda:None,
                                    supervisor_handle=self.proc.handle)
        self.proc.emit({'method':'blocked'})
        self.assertTrue(self.entered.wait(1))

    def tearDown(self):
        self.release.set()
        self.proc.reply_release.set()
        self.proc.ack_release.set()
        self.server.close()
        self.assertTrue(self.server.join_callbacks(2))
        self.temp.cleanup()

    def clock(self):
        self.proc.emit({'id':self.CLOCK_ID, 'method':'currentTime/read'})

    def replied(self):
        with self.proc.condition:
            return self.proc.condition.wait_for(
                lambda:any(frame.get('id') == self.CLOCK_ID for frame in self.proc.writes), .5)

    def wait_for(self, condition):
        end = time.monotonic() + 1
        while not condition() and time.monotonic() < end:
            self.proc.exited.wait(.005)
        self.assertTrue(condition())

    def durable_reply(self):
        with self.proc.journal.db() as db:
            return db.execute('SELECT native_id FROM operations WHERE native_id=?',
                              (self.CLOCK_ID,)).fetchone()

    def test_clock_reply_bypasses_blocked_callbacks_and_keeps_the_ack_gap(self):
        self.clock()
        self.assertTrue(self.replied(), 'The clock reply waits behind the blocked callback')
        self.wait_for(lambda:2 in self.proc.ack_pending)
        self.assertEqual(self.durable_reply()[0], self.CLOCK_ID)
        self.assertEqual(self.proc.cursor, 0)
        self.assertEqual(self.applied, [])
        self.release.set()
        self.wait_for(lambda:self.proc.cursor == 2)
        self.assertEqual(self.applied, ['blocked'])

    def test_clock_event_stays_unacked_until_the_durable_reply(self):
        pending = self.server.submit('config/read', {})
        self.proc.reply_release.clear()
        self.clock()
        self.assertTrue(self.proc.reply_started.wait(.5))
        self.assertIsNone(self.durable_reply())
        self.assertNotIn(2, self.proc.ack_pending)
        self.proc.emit({'id':pending[0], 'result':{'ready':True}})
        self.assertEqual(self.server.wait(pending, .5), {'ready':True})
        self.proc.reply_release.set()
        self.assertTrue(self.replied())
        self.wait_for(lambda:2 in self.proc.ack_pending)
        self.assertEqual(self.proc.cursor, 0)

    def test_failed_clock_reply_preserves_the_exact_event_for_replay(self):
        self.proc.fail_reply = True
        self.clock()
        self.assertTrue(self.proc.exited.wait(.5))
        self.assertIn('clock reply failed', self.server.transport_error)
        self.assertIsNone(self.durable_reply())
        self.assertNotIn(2, self.proc.ack_pending)
        self.release.set()
        self.assertTrue(self.server.join_callbacks(1))
        with self.proc.journal.db() as db:
            row = db.execute('SELECT payload FROM events WHERE sequence=2').fetchone()
        self.assertEqual(json.loads(row[0])['id'], self.CLOCK_ID)
        self.assertEqual(self.proc.cursor, 1)

    def test_failed_ack_retains_only_the_captured_clock_receipt(self):
        self.proc.fail_ack = True
        self.clock()
        self.assertTrue(self.proc.exited.wait(.5))
        self.assertIsNotNone(self.durable_reply())
        self.assertEqual(self.server._clock_reply_sequences, {(int,self.CLOCK_ID):[2]})
        self.assertNotIn(2, self.proc.ack_pending)
        self.release.set()
        self.assertTrue(self.server.join_callbacks(1))
        with self.proc.journal.db() as db:
            self.assertIsNotNone(db.execute('SELECT 1 FROM events WHERE sequence=2').fetchone())

    def test_reused_and_typed_ids_keep_each_exact_supervisor_sequence(self):
        self.proc.pause_ack = True
        self.proc.ack_release.clear()
        self.clock()
        self.assertTrue(self.proc.ack_started.wait(.5))
        self.clock()
        self.proc.emit({'id':str(self.CLOCK_ID), 'method':'currentTime/read'})
        self.wait_for(lambda:self.server._clock_reply_sequences.get((int,self.CLOCK_ID)) == [2,3]
                      and self.server._clock_reply_sequences.get((str,str(self.CLOCK_ID))) == [4])
        self.assertEqual(self.proc.cursor, 0)
        self.proc.ack_release.set()
        self.wait_for(lambda:self.proc.ack_pending == {2,3,4})
        self.wait_for(lambda:not self.server._clock_reply_sequences)
        frames = [frame for frame in self.proc.writes if 'result' in frame]
        self.assertEqual([frame['id'] for frame in frames], [self.CLOCK_ID,self.CLOCK_ID,str(self.CLOCK_ID)])
        self.assertEqual(self.proc.remote_to_local, {})
        self.release.set()
        self.wait_for(lambda:self.proc.cursor == 4)

    def test_clock_ledger_stays_bounded_and_preserves_the_rejected_event(self):
        self.server.CLOCK_QUEUE_LIMIT = 1
        with self.server.clock_replies.mutex:
            self.server.clock_replies.maxsize = 1
        self.proc.reply_release.clear()
        self.clock()
        self.assertTrue(self.proc.reply_started.wait(.5))
        self.clock()
        self.wait_for(lambda:self.server._clock_reply_sequences.get((int,self.CLOCK_ID)) == [2,3])
        self.clock()
        self.assertTrue(self.proc.exited.wait(.5))
        self.assertIn('clock receipt ledger saturated', self.server.transport_error)
        self.assertEqual(sum(len(saved) for saved in self.server._clock_reply_sequences.values()), 2)
        self.assertNotIn(4, self.proc.ack_pending)
        with self.proc.journal.db() as db:
            self.assertIsNotNone(db.execute('SELECT 1 FROM events WHERE sequence=4').fetchone())


if __name__ == '__main__':
    unittest.main()

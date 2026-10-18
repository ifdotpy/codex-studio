#!/usr/bin/env python3
"""A saved applied-event cursor must not skip the events of a replaced supervisor journal.

The runtime skips a journal event whose sequence is at or below its saved cursor.
A new supervisor journal numbers events from one again. Before the attach-time
rebase, every event of the new journal was acknowledged as applied and dropped,
so live items, turn starts, and answers never reached the transcript.
"""

from test_isolation import isolate_supervisor_environment

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from server_test_waits import HANG_GUARD_SECONDS
from codex_layout import SERVER_SOURCE_ROOT

isolate_supervisor_environment()
from runtime_test_environment import make_runtime

import ast
import json
import queue
import tempfile
import threading
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import AppServer, Runtime

HANDLE = 'fixture:account'
# A cursor saved for the previous journal of the same handle.
STALE_CURSOR = 488267


class QuietRuntime(Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait()
            self.changed.clear()


class JournalLines:
    def __init__(self, proxy):
        self.proxy = proxy
        self.current_sequence = 0

    def __iter__(self):
        while True:
            event = self.proxy.pending.get()
            if event is None:
                return
            self.current_sequence, message = event
            yield json.dumps(message) + '\n'


class JournalProxy:
    """The supervisor open receipt and event stream of one journal handle."""

    def __init__(self, events, acknowledged=0):
        self.handle = HANDLE
        self.generation = 1
        self.resumed = False
        self.initialize_result = {'userAgent': 'fixture/1.0.0'}
        # The journal's latest sequence and its saved ACK cursor.
        self.sequence = len(events)
        self.cursor = acknowledged
        self.stderr = None
        self.remote_to_local = {}
        self.acks = []
        self.changed = threading.Event()
        self.pending = queue.Queue()
        for sequence, message in enumerate(events, 1):
            if sequence > acknowledged:
                self.pending.put((sequence, message))
        self.stdout = JournalLines(self)
        self.detached = False

    def poll(self):
        return 0 if self.detached else None

    def send_write(self, _value, operation_id=None):
        return {'accepted': True, 'duplicate': False}

    def ack(self, sequence):
        self.acks.append(sequence)
        self.changed.set()

    def register_event_batch(self, _sequences):
        pass

    def ack_applied_deltas(self, _lookup):
        return 0

    def detach(self):
        if not self.detached:
            self.detached = True
            self.pending.put(None)
        self.changed.set()

    terminate = kill = detach

    def wait(self, _timeout=None):
        return 0


def answer(item_id, text):
    return {'method': 'item/completed',
            'params': {'threadId': 'thread', 'turnId': 'turn',
                       'item': {'id': item_id, 'type': 'agentMessage', 'text': text,
                                'phase': 'final_answer'}}}


class SupervisorJournalReplacement(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix='studio-journal-replacement-')
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.runtime = make_runtime(QuietRuntime, self.root, server_factory=lambda *args: None)
        self.addCleanup(self.runtime.close)
        agent = self.runtime.create({'name': 'Worker', 'cwd': directory.name, 'prompt': ''},
                                    draft=True, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            agent.update(threadId='thread', turnId='turn', status='running', inFlight=True,
                         autoWake=False, events=0)
            self.runtime.put(db, 'agents', agent)
        self.agent = agent

    def save_cursor(self, sequence):
        with self.runtime.lock, self.runtime.db() as db:
            db.execute('INSERT INTO runtime_supervisor_cursor VALUES (?,?) ON CONFLICT(handle) '
                       'DO UPDATE SET sequence=excluded.sequence', (HANDLE, sequence))

    def cursor(self):
        with self.runtime.read_db() as db:
            row = db.execute('SELECT sequence FROM runtime_supervisor_cursor WHERE handle=?',
                             (HANDLE,)).fetchone()
            return row[0] if row else None

    def answer_text(self, item_id):
        with self.runtime.read_db() as db:
            row = db.execute('SELECT record FROM runtime_items WHERE id=?',
                             (self.agent['id'] + ':' + item_id,)).fetchone()
            return json.loads(row[0])['text'] if row else None

    def attach(self, proxy):
        runtime = self.runtime
        server = AppServer(
            self.root / 'account', lambda message: runtime.notification(message),
            lambda _message: None, lambda: None,
            supervisor_handle=HANDLE, process_factory=lambda _stderr: proxy,
            supervisor_commit=lambda message, sequence: runtime.commit_supervisor_event(
                HANDLE, message, sequence, 'default', None),
            supervisor_event_applied=lambda sequence: runtime.supervisor_event_applied(HANDLE, sequence),
            supervisor_cursor_rebase=lambda sequence, acknowledged: runtime.rebase_supervisor_cursor(
                HANDLE, sequence, acknowledged),
            notification_dispatcher_closed=runtime.close_notification_database)
        self.addCleanup(server.close)
        deadline = time.monotonic() + HANG_GUARD_SECONDS
        while proxy.acks[-1:] != [proxy.sequence] and not server.transport_error:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not proxy.changed.wait(remaining):
                raise AssertionError('The journal events were not acknowledged')
            proxy.changed.clear()
        self.assertIsNone(server.transport_error)
        return server

    def test_events_of_a_replaced_journal_reach_the_transcript(self):
        (self.root / 'account').mkdir()
        self.save_cursor(STALE_CURSOR)
        proxy = JournalProxy([answer('msg-first', 'First answer'), answer('msg-second', 'Second answer')])
        self.attach(proxy)
        self.assertEqual(self.answer_text('msg-first'), 'First answer')
        self.assertEqual(self.answer_text('msg-second'), 'Second answer')
        self.assertEqual(self.cursor(), 2)
        log = (self.root / 'account' / 'app-server.log').read_text()
        self.assertIn('"kind": "supervisorCursorRebased"', log)
        self.assertIn('"staleCursor": %d' % STALE_CURSOR, log)

    def test_a_cursor_inside_the_journal_still_skips_applied_events(self):
        (self.root / 'account').mkdir()
        self.save_cursor(1)
        proxy = JournalProxy([answer('msg-applied', 'Applied before'), answer('msg-new', 'New answer')])
        self.attach(proxy)
        # Event 1 was applied before the reattach; it is acknowledged, not applied again.
        self.assertIsNone(self.answer_text('msg-applied'))
        self.assertEqual(self.answer_text('msg-new'), 'New answer')
        self.assertEqual(proxy.acks, [1, 2])
        self.assertEqual(self.cursor(), 2)
        log = (self.root / 'account' / 'app-server.log').read_text()
        self.assertNotIn('supervisorCursorRebased', log)

    def test_every_supervised_connection_rebases_its_cursor(self):
        """A connection that skips applied events must also reject a foreign cursor."""
        missing = []
        for path in sorted(SERVER_SOURCE_ROOT.glob('*.py')):
            tree = ast.parse(path.read_text(encoding='utf-8'), filename=str(path))
            for node in ast.walk(tree):
                if not isinstance(node, ast.Call):
                    continue
                names = {keyword.arg for keyword in node.keywords}
                if 'supervisor_event_applied' in names and 'supervisor_cursor_rebase' not in names:
                    missing.append(f'{path.name}:{node.lineno}')
        self.assertEqual(missing, [])


if __name__ == '__main__':
    unittest.main()

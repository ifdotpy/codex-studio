#!/usr/bin/env python3
"""A saved applied-event cursor is valid only inside the journal epoch that issued it.

The runtime skips a journal event whose sequence is at or below its saved cursor.
A new supervisor journal, and a recreated handle row, number events from one
again. Before journal epochs, every event of a new journal up to the old cursor
was acknowledged as applied and dropped, so live items, turn starts, and answers
never reached the transcript.
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
import sqlite3
import tempfile
import threading
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import AppServer, Runtime
import codex_process_supervisor as supervisor
from codex_shared_values import SUPERVISOR_JOURNAL_ID_KEY

HANDLE = 'fixture:account'
OLD_EPOCH = 'old-journal:1000000'
NEW_EPOCH = 'new-journal:2000000'


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

    def __init__(self, events, *, epoch, acknowledged=0):
        self.handle = HANDLE
        self.generation = 1
        self.resumed = False
        self.initialize_result = {'userAgent': 'fixture/1.0.0'}
        # The journal's latest sequence, its saved ACK cursor, and its epoch.
        self.sequence = len(events)
        self.cursor = acknowledged
        self.journal_epoch = epoch
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


def answer(index):
    return {'method': 'item/completed',
            'params': {'threadId': 'thread', 'turnId': 'turn',
                       'item': {'id': f'msg-{index}', 'type': 'agentMessage', 'text': f'Answer {index}',
                                'phase': 'final_answer'}}}


def answers(count):
    return [answer(index) for index in range(1, count + 1)]


class CursorEpochs(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix='studio-journal-epoch-')
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        (self.root / 'account').mkdir()
        self.runtime = make_runtime(QuietRuntime, self.root, server_factory=lambda *args: None)
        self.addCleanup(self.runtime.close)
        agent = self.runtime.create({'name': 'Worker', 'cwd': directory.name, 'prompt': ''},
                                    draft=True, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            agent.update(threadId='thread', turnId='turn', status='running', inFlight=True,
                         autoWake=False, events=0)
            self.runtime.put(db, 'agents', agent)
        self.agent = agent

    def save_cursor(self, sequence, epoch):
        with self.runtime.lock, self.runtime.db() as db:
            db.execute('INSERT INTO runtime_supervisor_cursor(handle,sequence,epoch) VALUES (?,?,?) '
                       'ON CONFLICT(handle) DO UPDATE SET sequence=excluded.sequence,epoch=excluded.epoch',
                       (HANDLE, sequence, epoch))

    def cursor(self):
        with self.runtime.read_db() as db:
            row = db.execute('SELECT sequence,epoch FROM runtime_supervisor_cursor WHERE handle=?',
                             (HANDLE,)).fetchone()
            return tuple(row) if row else None

    def saved_answers(self):
        with self.runtime.read_db() as db:
            prefix = self.agent['id'] + ':msg-'
            return sorted(int(row[0].removeprefix(prefix)) for row in db.execute(
                'SELECT id FROM runtime_items WHERE id LIKE ?', (prefix + '%',)))

    def log(self):
        path = self.root / 'account' / 'app-server.log'
        return [json.loads(line) for line in path.read_text().splitlines()
                if line.startswith('{"kind": "supervisorCursorAttach"')] if path.exists() else []

    def attach(self, proxy):
        runtime = self.runtime
        server = AppServer(
            self.root / 'account', lambda message: runtime.notification(message),
            lambda _message: None, lambda: None,
            supervisor_handle=HANDLE, process_factory=lambda _stderr: proxy,
            supervisor_commit=lambda message, sequence: runtime.commit_supervisor_event(
                HANDLE, message, sequence, 'default', None),
            supervisor_event_applied=lambda sequence: runtime.supervisor_event_applied(HANDLE, sequence),
            supervisor_cursor_attach=lambda epoch, sequence, acknowledged: runtime.attach_supervisor_cursor(
                HANDLE, epoch, sequence, acknowledged),
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

    def test_new_journal_below_the_old_cursor_applies_every_event(self):
        self.save_cursor(488267, OLD_EPOCH)
        self.attach(JournalProxy(answers(2), epoch=NEW_EPOCH))
        self.assertEqual(self.saved_answers(), [1, 2])
        self.assertEqual(self.cursor(), (2, NEW_EPOCH))
        self.assertEqual([entry['outcome'] for entry in self.log()], ['voidedOtherEpoch'])

    def test_new_journal_above_the_old_cursor_applies_every_event(self):
        # The old cursor (5) is below the new journal's latest sequence (8).
        # Only the epoch shows that events 1 to 5 of this journal were never applied.
        self.save_cursor(5, OLD_EPOCH)
        self.attach(JournalProxy(answers(8), epoch=NEW_EPOCH))
        self.assertEqual(self.saved_answers(), list(range(1, 9)))
        self.assertEqual(self.cursor(), (8, NEW_EPOCH))

    def test_other_epoch_starts_at_the_journal_acknowledged_sequence(self):
        self.save_cursor(5, OLD_EPOCH)
        proxy = JournalProxy(answers(4), epoch=NEW_EPOCH, acknowledged=2)
        self.attach(proxy)
        # Events 1 and 2 were applied by a backend attached to this journal.
        self.assertEqual(self.saved_answers(), [3, 4])
        self.assertEqual(self.cursor(), (4, NEW_EPOCH))

    def test_same_epoch_keeps_the_cursor(self):
        self.save_cursor(1, NEW_EPOCH)
        proxy = JournalProxy(answers(2), epoch=NEW_EPOCH)
        self.attach(proxy)
        self.assertEqual(self.saved_answers(), [2])
        self.assertEqual(proxy.acks, [1, 2])
        self.assertEqual(self.cursor(), (2, NEW_EPOCH))
        self.assertEqual(self.log(), [])

    def test_legacy_cursor_inside_the_journal_is_adopted_for_its_epoch(self):
        self.save_cursor(1, None)
        self.attach(JournalProxy(answers(2), epoch=NEW_EPOCH))
        self.assertEqual(self.saved_answers(), [2])
        self.assertEqual(self.cursor(), (2, NEW_EPOCH))
        self.assertEqual([entry['outcome'] for entry in self.log()], ['adoptedLegacy'])

    def test_legacy_cursor_above_the_journal_is_void(self):
        self.save_cursor(488267, None)
        self.attach(JournalProxy(answers(2), epoch=NEW_EPOCH))
        self.assertEqual(self.saved_answers(), [1, 2])
        self.assertEqual(self.cursor(), (2, NEW_EPOCH))
        self.assertEqual([entry['outcome'] for entry in self.log()], ['voidedAboveJournal'])

    def test_supervisor_without_epochs_keeps_the_legacy_rule_and_logs_it(self):
        self.save_cursor(488267, None)
        self.attach(JournalProxy(answers(2), epoch=None))
        self.assertEqual(self.saved_answers(), [1, 2])
        self.assertEqual(self.cursor(), (2, None))
        entries = self.log()
        self.assertEqual([(entry['outcome'], entry['epoch']) for entry in entries],
                         [('voidedAboveJournal', None)])

    def test_every_supervised_connection_attaches_its_cursor(self):
        """A connection that skips applied events must also give the runtime its journal epoch."""
        missing = []
        for path in sorted(SERVER_SOURCE_ROOT.glob('*.py')):
            tree = ast.parse(path.read_text(encoding='utf-8'), filename=str(path))
            for node in ast.walk(tree):
                if not isinstance(node, ast.Call):
                    continue
                names = {keyword.arg for keyword in node.keywords}
                if 'supervisor_event_applied' in names and 'supervisor_cursor_attach' not in names:
                    missing.append(f'{path.name}:{node.lineno}')
        self.assertEqual(missing, [])


class JournalEpochSource(unittest.TestCase):
    """The Python supervisor journal names each handle row instance."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix='studio-journal-id-')
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)

    def journal_id(self):
        db = sqlite3.connect(self.root / 'supervisor.sqlite3')
        try:
            return db.execute('SELECT value FROM supervisor_state WHERE key=?',
                              (SUPERVISOR_JOURNAL_ID_KEY,)).fetchone()[0]
        finally:
            db.close()

    def test_epoch_format_matches_the_rust_journal(self):
        # runtime-store supervisor_journal tests assert the same literal.
        self.assertEqual(supervisor.journal_epoch('journal', 1792051725.8936651), 'journal:1792051725893665')
        self.assertIsNone(supervisor.journal_epoch(None, 1.0))
        self.assertIsNone(supervisor.journal_epoch('journal', float('nan')))

    def test_journal_identity_persists_and_a_recreated_handle_row_is_a_new_epoch(self):
        journal = supervisor.Journal(self.root)
        first_id = self.journal_id()
        with journal.db() as db:
            db.execute('INSERT INTO handles(id,signature,pid,created) VALUES (?,?,0,?)', (HANDLE, 's', 100.25))
        first = supervisor.supervisor_launch_snapshot(self.root, HANDLE)['journal_epoch']
        self.assertEqual(first, f'{first_id}:100250000')
        with journal.db() as db:
            # retireCommand deletes the row; a later open inserts it again.
            db.execute('DELETE FROM handles WHERE id=?', (HANDLE,))
            db.execute('INSERT INTO handles(id,signature,pid,created) VALUES (?,?,0,?)', (HANDLE, 's', 200.5))
        second = supervisor.supervisor_launch_snapshot(self.root, HANDLE)['journal_epoch']
        self.assertNotEqual(second, first)
        journal.close()
        supervisor.Journal(self.root).close()
        self.assertEqual(self.journal_id(), first_id)

    def test_a_replaced_journal_file_is_a_new_epoch(self):
        journal = supervisor.Journal(self.root)
        with journal.db() as db:
            db.execute('INSERT INTO handles(id,signature,pid,created) VALUES (?,?,0,?)', (HANDLE, 's', 100.25))
        first = supervisor.supervisor_launch_snapshot(self.root, HANDLE)['journal_epoch']
        journal.close()
        for suffix in ('', '-wal', '-shm'):
            (self.root / ('supervisor.sqlite3' + suffix)).unlink(missing_ok=True)
        journal = supervisor.Journal(self.root)
        with journal.db() as db:
            db.execute('INSERT INTO handles(id,signature,pid,created) VALUES (?,?,0,?)', (HANDLE, 's', 100.25))
        self.assertNotEqual(supervisor.supervisor_launch_snapshot(self.root, HANDLE)['journal_epoch'], first)
        journal.close()


if __name__ == '__main__':
    unittest.main()

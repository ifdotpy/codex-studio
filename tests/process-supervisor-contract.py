#!/usr/bin/env python3
"""Private-state process-supervisor contracts with a deterministic fake model."""
import json
import os
from pathlib import Path
import signal
import select
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import uuid
from contextlib import closing, contextmanager
from unittest.mock import patch

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "desktop"))
from codex_runtime import AppServer, ResponseTimeout, Runtime
from codex_process_supervisor import finish_fallback, process_start_time, status
import codex_process_supervisor as process_supervisor
import recover_backend

FAKE_NATIVE = r'''#!/usr/bin/env python3
import base64, json, os, sys, time
from pathlib import Path
Path(os.environ['FAKE_NATIVE_PID']).write_text(str(os.getpid()))
for line in sys.stdin:
    request=json.loads(line)
    method=request.get('method')
    if not method: continue
    with open(os.environ['FAKE_NATIVE_OPS'],'a') as log: log.write(json.dumps({'method':method,'params':request.get('params')})+'\n')
    if method == 'initialized':
        Path(os.environ['FAKE_NATIVE_INITIALIZED']).touch()
        continue
    if 'id' not in request: continue
    if method == 'initialize':
        result={'userAgent':'fake-model/1.0.0'}
    elif method == 'model/list':
        if not Path(os.environ['FAKE_NATIVE_INITIALIZED']).exists():
            continue
        result={'data':[{'model':'fake'}]}
    elif method == 'burst':
        for delta in ['a', 'b', 'c', 'd', 'e', 'f']:
            print(json.dumps({'method':'item/agentMessage/delta','params':{'threadId':'thread','turnId':'burst-turn','itemId':request['params']['itemId'],'delta':delta}}),flush=True)
        print(json.dumps({'method':'item/completed','params':{'threadId':'thread','turnId':'burst-turn','itemId':request['params']['itemId']}}),flush=True)
        result={'ok':True}
    elif method == 'outputBurst':
        for index in range(request['params']['count']):
            print(json.dumps({'method':'item/commandExecution/outputDelta','params':{'threadId':'thread','turnId':'burst-turn','itemId':request['params']['itemId'],'delta':str(index)+','}}),flush=True)
        print(json.dumps({'method':'item/completed','params':{'threadId':'thread','turnId':'burst-turn','item':{'id':request['params']['itemId'],'type':'commandExecution','exitCode':0}}}),flush=True)
        result={'ok':True}
    elif method == 'monitorBurst':
        for index in range(request['params']['count']):
            chunk=base64.b64encode((str(index)+',').encode()).decode()
            print(json.dumps({'method':'command/exec/outputDelta','params':{'processId':'monitor-item','stream':'stdout','deltaBase64':chunk}}),flush=True)
        result={'ok':True}
    elif method == 'turn/start':
        print(json.dumps({'method':'item/agentMessage/delta','params':{'threadId':'thread','turnId':'turn','itemId':'item','delta':'retained-output'}}),flush=True)
        print('stderr-between-output',file=sys.stderr,flush=True)
        release=Path(os.environ['FAKE_RELEASE'])
        deadline=time.time()+10
        while not release.exists() and time.time()<deadline: time.sleep(.01)
        result={'turn':{'id':'turn','status':'completed'}}
    elif method == 'longTurn':
        params=request.get('params',{})
        Path(os.environ['FAKE_PHASE_ONE']).touch()
        release=Path(os.environ['FAKE_RELEASE'])
        deadline=time.time()+10
        while not release.exists() and time.time()<deadline: time.sleep(.01)
        print(json.dumps({'method':'item/agentMessage/delta','params':{'threadId':params['threadId'],'turnId':'long-turn','itemId':'long-item','delta':'buffered-'} }),flush=True)
        finish=Path(os.environ['FAKE_FINISH'])
        deadline=time.time()+10
        while not finish.exists() and time.time()<deadline: time.sleep(.01)
        print(json.dumps({'method':'item/completed','params':{'threadId':params['threadId'],'turnId':'long-turn','item':{'id':'long-item','type':'agentMessage','text':'buffered-final-answer','phase':'final_answer'}}}),flush=True)
        print(json.dumps({'method':'turn/completed','params':{'threadId':params['threadId'],'turn':{'id':'long-turn','status':'completed'}}}),flush=True)
        result={'turn':{'id':'long-turn','status':'completed'}}
    elif method == 'command/exec':
        if request['params']['processId'] == 'live-monitor':
            Path(os.environ['FAKE_PHASE_ONE']).touch()
            release=Path(os.environ['FAKE_RELEASE'])
            deadline=time.time()+10
            while not release.exists() and time.time()<deadline: time.sleep(.01)
        print(json.dumps({'method':'command/exec/outputDelta','params':{'processId':request['params']['processId'],'delta':'monitor-output'}}),flush=True)
        result={'exitCode':0}
    elif method == 'process/spawn':
        handle=request['params']['processHandle']
        print(json.dumps({'method':'process/outputDelta','params':{'processHandle':handle,'deltaBase64':'dGVybWluYWwtb3V0cHV0'}}),flush=True)
        result={}
    else:
        result={'ok':True}
    print(json.dumps({'id':request['id'],'result':result}),flush=True)
if os.environ.get('FAKE_STAY_ALIVE')=='1':
    while True: time.sleep(1)
'''

BACKEND_HARNESS = r'''import os,sys,time
from pathlib import Path
sys.path.insert(0,sys.argv[1]+'/scripts')
from codex_runtime import AppServer,ResponseTimeout
root=Path(os.environ['CODEX_AGENTS_STATE_DIR'])
target=os.environ['FAKE_TARGET_METHOD']
started=root/'callback-blocked'
def notify(message):
    if message.get('method')==target:
        started.touch()
        while not (root/'release-callback').exists(): time.sleep(.01)
server=AppServer(root,notify,lambda _:None,lambda:None,executable=os.environ['CODEX_BIN'],supervisor_handle=os.environ['FAKE_HANDLE'])
method=os.environ['FAKE_TRIGGER_METHOD']
params={'threadId':'thread','clientUserMessageId':'exact-operation','processId':'monitor-1','processHandle':'terminal-1'}
server.submit(method,params,operation_id='exact-operation:'+method)
while True: time.sleep(1)
'''


def wait_for(fn, timeout=5):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        try: result=fn()
        except (OSError,RuntimeError): result=None
        if result: return result
        time.sleep(.02)
    raise AssertionError('timed out waiting for private process fixture')


class ProcessProxyTailDeliveryContract(unittest.TestCase):
    def test_process_exit_does_not_overtake_final_stdout_event(self):
        proxy = process_supervisor.ProcessProxy.__new__(process_supervisor.ProcessProxy)
        proxy.detached = False
        proxy.event_lock = threading.RLock()
        proxy.cursor = 0
        proxy.read_cursor = 0
        proxy.sequence = 0
        proxy.generation = 1
        proxy.remote_to_local = {}
        proxy.ack_pending = set()
        proxy._returncode = None
        stderr = []
        proxy.stderr_sink = stderr.append
        frames = iter([
            {'event': None, 'returnCode': 0,
             'stdoutReaderAlive': True, 'stderrReaderAlive': True},
            {'event': {'sequence': 1, 'kind': 'stdout',
                       'payload': json.dumps({'method': 'item/completed',
                                              'params': {'itemId': 'final-tail'}}),
                       'generation': 1}, 'returnCode': 0,
             'stdoutReaderAlive': True, 'stderrReaderAlive': True},
            {'event': {'sequence': 2, 'kind': 'exit',
                       'payload': json.dumps({'returnCode': 0}),
                       'generation': 1}, 'returnCode': 0,
             'stdoutReaderAlive': False, 'stderrReaderAlive': True},
            {'event': {'sequence': 3, 'kind': 'stderr',
                       'payload': json.dumps({'data': 'final-stderr-tail'}),
                       'generation': 1}, 'returnCode': 0,
             'stdoutReaderAlive': False, 'stderrReaderAlive': True},
            {'event': None, 'returnCode': 0,
             'stdoutReaderAlive': False, 'stderrReaderAlive': False},
        ])

        def call(action, **values):
            if action == 'ack':
                return {}
            return next(frames)

        proxy.call = call
        self.assertEqual(proxy.next_event(),
                         (1, json.dumps({'method': 'item/completed',
                                         'params': {'itemId': 'final-tail'}}) + '\n'))
        self.assertIsNone(proxy.next_event())
        self.assertEqual(proxy._returncode, 0)
        self.assertEqual(stderr, ['final-stderr-tail'])

    def test_finished_readers_are_terminal_when_exit_receipt_is_missing(self):
        proxy = process_supervisor.ProcessProxy.__new__(process_supervisor.ProcessProxy)
        proxy.detached = False
        proxy.event_lock = threading.RLock()
        proxy.cursor = 0
        proxy.read_cursor = 0
        proxy.sequence = 0
        proxy.generation = 1
        proxy.ack_pending = set()
        proxy.remote_to_local = {}
        proxy._returncode = None
        calls = []

        def call(action, **values):
            calls.append(action)
            if action != 'next' or len(calls) > 1:
                raise AssertionError('terminal proxy must stop after both readers finish')
            return {'event': None, 'returnCode': 1,
                    'stdoutReaderAlive': False, 'stderrReaderAlive': False}

        proxy.call = call
        self.assertIsNone(proxy.next_event())
        self.assertEqual(proxy._returncode, 1)
        self.assertEqual(calls, ['next'])

    def test_legacy_exit_receipt_remains_terminal_without_reader_status(self):
        proxy = process_supervisor.ProcessProxy.__new__(process_supervisor.ProcessProxy)
        proxy.detached = False
        proxy.event_lock = threading.RLock()
        proxy.cursor = 0
        proxy.read_cursor = 0
        proxy.sequence = 0
        proxy.generation = 1
        proxy.ack_pending = set()
        proxy.remote_to_local = {}
        proxy._returncode = None
        calls = []

        def call(action, **values):
            calls.append(action)
            if action == 'ack':
                return {}
            return {'event': {'sequence': 1, 'kind': 'exit',
                              'payload': json.dumps({'returnCode': 0}),
                              'generation': 1}, 'returnCode': 0}

        proxy.call = call
        self.assertIsNone(proxy.next_event())
        self.assertEqual(proxy._returncode, 0)
        self.assertEqual(calls, ['next', 'ack'])


class SupervisorNextReaderFenceContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='supervisor-next-fence-')
        self.root = Path(self.temp.name)
        self.supervisor = process_supervisor.Supervisor(self.root)
        self.supervisor.journal = process_supervisor.Journal(self.root)
        self.handle = 'account:next-fence-fixture'
        with self.supervisor.journal.db() as db:
            db.execute('INSERT INTO handles(id,signature,pid,created,generation) VALUES (?,?,?,?,1)',
                       (self.handle, 'fixture-signature', 42, time.time()))

        class Reader:
            alive = True
            def is_alive(self):
                return self.alive

        self.child = type('ChildFixture', (), {})()
        self.child.handle = self.handle
        self.child.process = type('ProcessFixture', (), {'poll': lambda _: 0})()
        self.child.paused = threading.Event()
        self.child.reader = Reader()
        self.child.stderr = Reader()
        self.supervisor.children[self.handle] = self.child
        original_db = self.supervisor.journal.db
        child = self.child

        class Cursor:
            def __init__(self, cursor, db, race):
                self.cursor, self.db, self.race = cursor, db, race
            def fetchone(self):
                row = self.cursor.fetchone()
                if self.race and row is None:
                    payload = json.dumps({'method': 'item/completed',
                                          'params': {'itemId': 'arrived-after-select'}})
                    self.db.execute('UPDATE handles SET sequence=1 WHERE id=?', (child.handle,))
                    self.db.execute('INSERT INTO events(handle,sequence,kind,payload,size,generation) '
                                    'VALUES (?,?,?,?,?,1)',
                                    (child.handle, 1, 'stdout', payload, len(payload.encode())))
                    self.db.commit()
                    child.reader.alive = False
                    child.stderr.alive = False
                return row

        class Connection:
            def __init__(self, db):
                self.db = db
            def __getattr__(self, name):
                return getattr(self.db, name)
            def execute(self, sql, params=()):
                cursor = self.db.execute(sql, params)
                race = sql.startswith('SELECT sequence,kind,payload,generation FROM events')
                return Cursor(cursor, self.db, race)

        @contextmanager
        def database():
            with original_db() as db:
                yield Connection(db)

        self.supervisor.journal.db = database
        self.addCleanup(self.cleanup)

    def cleanup(self):
        self.supervisor.journal.close()
        self.temp.cleanup()

    def test_reader_completion_sample_precedes_empty_event_query(self):
        first = self.supervisor.handle({'action': 'next', 'handle': self.handle, 'cursor': 0})
        self.assertIsNone(first['event'])
        self.assertTrue(first['stdoutReaderAlive'])
        self.assertTrue(first['stderrReaderAlive'])
        second = self.supervisor.handle({'action': 'next', 'handle': self.handle, 'cursor': 0})
        self.assertEqual(second['event']['kind'], 'stdout')
        self.assertEqual(json.loads(second['event']['payload'])['params']['itemId'],
                         'arrived-after-select')
        self.assertFalse(second['stdoutReaderAlive'])
        self.assertFalse(second['stderrReaderAlive'])


class StdoutPersistenceContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='supervisor-storage-')
        self.root = Path(self.temp.name)
        self.supervisor = process_supervisor.Supervisor(self.root)
        self.supervisor.journal = process_supervisor.Journal(self.root)
        self.journal = self.supervisor.journal
        self.release = threading.Event()
        self.failed = threading.Event()
        self.attempts = []
        self.process = subprocess.Popen([sys.executable, '-u', '-c',
            'import os,sys\nfor line in sys.stdin:\n'
            ' if line == ":close-stdout\\n": os.close(1)\n'
            ' else: print(line, end="", flush=True)'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.process.supervisor = self.supervisor
        self.handle = 'account:private-storage-fixture'
        with self.journal.db() as db:
            db.execute('INSERT INTO handles(id,signature,pid,created,generation) VALUES (?,?,?,?,1)',
                       (self.handle, 'exact-signature', self.process.pid, time.time()))
            db.execute('INSERT INTO operations(handle,operation_id,digest,native_id,accepted,generation) '
                       'VALUES (?,?,?,?,?,1)', (self.handle, 'monitor:exact-request', 'exact-digest', 1, time.time()))
        self.original_db = self.journal.db
        self.addCleanup(self.cleanup)

    def cleanup(self):
        self.release.set()
        if self.process.stdin and not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=3)
        if hasattr(self, 'child'):
            self.child.reader.join(timeout=3)
            self.child.stopping.set()
            self.child.stderr.join(timeout=3)
        for stream in (self.process.stdout, self.process.stderr):
            stream.close()
        self.temp.cleanup()

    def inject(self, error, *, statement='INSERT INTO events', after_commit=False):
        fixture = self
        class Connection:
            def __init__(self, db):
                self.db = db
            def __getattr__(self, name):
                return getattr(self.db, name)
            def execute(self, sql, params=()):
                if after_commit and fixture.failed.is_set() and not fixture.release.is_set():
                    raise error
                if sql.startswith('INSERT INTO events') and params[2] == 'stdout':
                    fixture.attempts.append(params[3])
                if sql.startswith(statement) and not fixture.release.is_set() and not after_commit:
                    if not sql.startswith('INSERT INTO events') or params[2] == 'stdout':
                        fixture.failed.set()
                        raise error
                return self.db.execute(sql, params)
            def commit(self):
                self.db.commit()
                if after_commit and not fixture.release.is_set():
                    fixture.failed.set()
                    raise error
        @contextmanager
        def database():
            with self.original_db() as db:
                yield Connection(db)
        self.journal.db = database
        self.child = process_supervisor.Child(self.handle, self.process, 'exact-signature')
        self.supervisor.children[self.handle] = self.child

    def send_frames(self):
        self.frames = [{'id':1, 'result':{'userAgent':'private-model'}},
                       {'method':'item/completed', 'params':{'itemId':'exact-next-item'}}]
        self.process.stdin.write(''.join(json.dumps(frame) + '\n' for frame in self.frames))
        self.process.stdin.flush()

    def saved(self):
        with self.original_db() as db:
            handle = dict(db.execute('SELECT * FROM handles WHERE id=?', (self.handle,)).fetchone())
            events = [dict(row) for row in db.execute('SELECT * FROM events WHERE handle=? ORDER BY sequence',
                                                     (self.handle,))]
            operation = dict(db.execute('SELECT * FROM operations WHERE handle=?', (self.handle,)).fetchone())
        return handle, events, operation

    def assert_paused_without_exit(self):
        self.assertTrue(self.failed.wait(2))
        wait_for(self.child.paused.is_set)
        self.assertTrue(self.child.reader.is_alive())
        self.assertIsNone(self.process.poll())
        handle, events, operation = self.saved()
        self.assertEqual(handle['sequence'], 0)
        self.assertIsNone(handle['init_result'])
        self.assertEqual(events, [])
        self.assertIsNone(operation['response'])
        health = self.supervisor.handle({'action':'health'})['handles'][0]
        self.assertTrue(health['stdoutReaderAlive'])
        self.assertTrue(health['backpressure'])
        self.assertIn('stdout', health['persistenceErrors'])
        self.assertIsNone(health['stdoutReaderError'])
        # Storage waits release the output and journal locks.
        self.assertTrue(self.child.lock.acquire(timeout=.5))
        self.child.lock.release()
        self.assertTrue(self.journal.lock.acquire(timeout=.5))
        self.journal.lock.release()

    def assert_recovered_once(self):
        self.release.set()
        wait_for(lambda:len(self.saved()[1]) == 2)
        handle, events, operation = self.saved()
        self.assertEqual(handle['sequence'], 2)
        self.assertEqual([event['sequence'] for event in events], [1, 2])
        self.assertTrue(all(event['kind'] == 'stdout' for event in events))
        payloads = [json.loads(event['payload']) for event in events]
        self.assertEqual(payloads[0], self.frames[0])
        payloads[1].pop('_studioSupervisorReceivedAt')
        self.assertEqual(payloads[1], self.frames[1])
        self.assertEqual(json.loads(handle['init_result']), self.frames[0]['result'])
        self.assertEqual(json.loads(operation['response']), self.frames[0])
        self.assertEqual(operation['response_sequence'], 1)
        first_attempts = [raw for raw in self.attempts if json.loads(raw).get('id') == 1]
        self.assertTrue(first_attempts)
        self.assertEqual(len(set(first_attempts)), 1)
        self.assertTrue(self.child.reader.is_alive())
        self.assertFalse(self.child.paused.is_set())
        self.assertEqual(self.child.persistence_errors, {})
        self.process.stdin.close()
        self.process.wait(timeout=2)
        self.child.reader.join(timeout=2)
        self.assertFalse(self.child.reader.is_alive())
        exit_events = [event for event in self.saved()[1] if event['kind'] == 'exit']
        self.assertEqual(len(exit_events), 1)
        self.assertEqual(json.loads(exit_events[0]['payload']), {'returnCode':0})

    def test_full_insert_rolls_back_and_keeps_stdout_reader(self):
        error = sqlite3.OperationalError('database or disk is full')
        error.sqlite_errorcode = sqlite3.SQLITE_FULL
        self.inject(error)
        self.send_frames()
        self.assert_paused_without_exit()
        self.assert_recovered_once()

    def test_init_result_failure_retries_the_same_transaction(self):
        error = sqlite3.OperationalError('disk I/O error')
        error.sqlite_errorcode = sqlite3.SQLITE_IOERR_WRITE
        self.inject(error, statement='UPDATE handles SET init_result')
        self.send_frames()
        self.assert_paused_without_exit()
        self.assert_recovered_once()

    def test_busy_insert_keeps_the_same_frame_until_storage_recovers(self):
        error = sqlite3.OperationalError('database is locked')
        error.sqlite_errorcode = sqlite3.SQLITE_BUSY_RECOVERY
        self.inject(error)
        self.send_frames()
        self.assert_paused_without_exit()
        self.assert_recovered_once()

    def test_locked_insert_keeps_the_same_frame_until_storage_recovers(self):
        error = sqlite3.OperationalError('database table is locked')
        error.sqlite_errorcode = sqlite3.SQLITE_LOCKED_SHAREDCACHE
        self.inject(error)
        self.send_frames()
        self.assert_paused_without_exit()
        self.assert_recovered_once()

    def test_disk_oserror_keeps_the_same_frame_until_storage_recovers(self):
        self.inject(OSError('injected disk failure'))
        self.send_frames()
        self.assert_paused_without_exit()
        self.assert_recovered_once()

    def test_successful_commit_with_lost_confirmation_does_not_duplicate_frame(self):
        error = sqlite3.OperationalError('disk I/O error')
        error.sqlite_errorcode = sqlite3.SQLITE_IOERR
        self.inject(error, after_commit=True)
        self.send_frames()
        self.assertTrue(self.failed.wait(2))
        wait_for(self.child.paused.is_set)
        handle, events, operation = self.saved()
        self.assertEqual(handle['sequence'], 1)
        self.assertEqual(len(events), 1)
        self.assertEqual(operation['response_sequence'], 1)
        self.assertTrue(self.child.reader.is_alive())
        self.assert_recovered_once()

    def test_process_exit_waits_for_delayed_final_stdout_persistence(self):
        self.inject(OSError(28, 'fixture storage delay'))
        frame = {'method': 'item/completed', 'params': {'itemId': 'delayed-tail'}}
        self.process.stdin.write(json.dumps(frame) + '\n')
        self.process.stdin.close()
        self.assertTrue(self.failed.wait(2))
        self.process.wait(timeout=2)
        self.assertTrue(self.child.reader.is_alive())

        proxy = process_supervisor.ProcessProxy.__new__(process_supervisor.ProcessProxy)
        proxy.handle = self.handle
        proxy.detached = False
        proxy.event_lock = threading.RLock()
        proxy.cursor = 0
        proxy.read_cursor = 0
        proxy.sequence = 0
        proxy.generation = 1
        proxy.ack_pending = set()
        proxy.remote_to_local = {}
        proxy._returncode = None
        proxy.stderr_sink = lambda _: None
        queried = threading.Event()

        def call(action, **values):
            result = self.supervisor.handle({'action': action, 'handle': self.handle, **values})
            if action == 'next':
                queried.set()
            return result

        proxy.call = call
        delivered = []
        reader = threading.Thread(target=lambda: delivered.append(proxy.next_event()), daemon=True)
        reader.start()
        self.assertTrue(queried.wait(2))
        self.assertIsNotNone(self.process.poll())
        self.assertEqual(delivered, [])
        self.release.set()
        reader.join(timeout=2)
        self.assertFalse(reader.is_alive())
        self.assertEqual(len(delivered), 1)
        sequence, raw = delivered[0]
        payload = json.loads(raw)
        self.assertEqual(sequence, 1)
        payload.pop('_studioSupervisorReceivedAt')
        self.assertEqual(payload, frame)

    def test_stdout_end_does_not_report_exit_while_native_child_is_alive(self):
        error = sqlite3.OperationalError('database or disk is full')
        error.sqlite_errorcode = sqlite3.SQLITE_FULL
        waiting = threading.Event()
        original_wait = self.process.wait
        def wait(*args, **kwargs):
            waiting.set()
            return original_wait(*args, **kwargs)
        self.process.wait = wait
        self.inject(error)
        self.process.stdin.write(':close-stdout\n')
        self.process.stdin.flush()
        self.assertTrue(waiting.wait(2))
        self.assertIsNone(self.process.poll())
        self.assertTrue(self.child.reader.is_alive())
        self.assertEqual(self.saved()[1], [])
        self.process.stdin.close()
        original_wait(timeout=2)
        self.child.reader.join(timeout=2)
        events = self.saved()[1]
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]['kind'], 'exit')
        self.assertEqual(json.loads(events[0]['payload']), {'returnCode':0})

    def test_retryable_storage_codes_and_disk_errors(self):
        for code in (sqlite3.SQLITE_FULL, sqlite3.SQLITE_BUSY_RECOVERY,
                     sqlite3.SQLITE_LOCKED_SHAREDCACHE, sqlite3.SQLITE_IOERR_WRITE):
            with self.subTest(code=code):
                error = sqlite3.OperationalError('injected storage failure')
                error.sqlite_errorcode = code
                self.assertTrue(process_supervisor._retryable_storage_error(error))
        self.assertTrue(process_supervisor._retryable_storage_error(OSError('disk failure')))
        self.assertFalse(process_supervisor._retryable_storage_error(sqlite3.OperationalError('syntax error')))


class JournalRetentionContract(unittest.TestCase):
    def test_operation_receipts_can_exceed_one_handles_output_limit(self):
        with tempfile.TemporaryDirectory(prefix='studio-receipt-retention-') as directory:
            journal = process_supervisor.Journal(directory)
            with patch.object(process_supervisor, 'HANDLE_LIMIT', 64 * 1024):
                with journal.db() as db:
                    db.executemany('INSERT INTO operations(handle,operation_id,digest,native_id,accepted,generation) '
                                   'VALUES (?,?,?,?,?,?)',
                                   [('account:fixture', str(i), 'a' * 64, i, 1.0, 1) for i in range(1024)])
                with journal.db() as db:
                    self.assertEqual(db.execute('SELECT count(*) FROM operations').fetchone()[0], 1024)
                    pages = db.execute('PRAGMA page_count').fetchone()[0]
                    page_size = db.execute('PRAGMA page_size').fetchone()[0]
                    self.assertGreater(pages * page_size, process_supervisor.HANDLE_LIMIT)
                    self.assertEqual(journal.outstanding_bytes(db, 'account:fixture'), 0)


class ProcessSupervisorContract(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(prefix='o6-')
        self.root=Path(self.temp.name).resolve()
        self.binary=self.root/'fake-native'
        # The Apple developer launcher adds SDK environment variables. Use the
        # test interpreter so a fixture exec preserves its accepted environment.
        self.binary.write_text(FAKE_NATIVE.replace('#!/usr/bin/env python3', '#!' + sys.executable, 1))
        self.binary.chmod(0o700)
        self.release=self.root/'release'
        self.pid_file=self.root/'native.pid'
        self.env_patch=patch.dict(os.environ, {
            'CODEX_AGENTS_STATE_DIR':str(self.root),
            'CODEX_AGENTS_SUPERVISOR_MODE':'1',
            'CODEX_AGENTS_BACKEND_ID':'backend-'+str(uuid.uuid4()),
            'CODEX_BIN':str(self.binary),
            'FAKE_NATIVE_PID':str(self.pid_file),
            'FAKE_NATIVE_OPS':str(self.root/'native-ops.jsonl'),
            'FAKE_NATIVE_INITIALIZED':str(self.root/'native-initialized'),
            'FAKE_RELEASE':str(self.release),
            'FAKE_PHASE_ONE':str(self.root/'phase-one'),
            'FAKE_FINISH':str(self.root/'finish'),
        })
        self.env_patch.start()
        self.supervisor_log=self.root/'supervisor.stderr'
        self.supervisor_log_stream=self.supervisor_log.open('w')
        self.supervisor=subprocess.Popen([sys.executable,'-B',str(ROOT/'scripts/codex_process_supervisor.py'),
            '--state',str(self.root)],stdout=subprocess.DEVNULL,stderr=self.supervisor_log_stream)
        self.extra_supervisors=[]
        self.addCleanup(self.cleanup)
        try: wait_for(lambda: status(self.root))
        except Exception:
            if self.supervisor.poll() is not None:
                raise AssertionError(self.supervisor_log.read_text())
            raise
        self.delivered=[]
        self.callback_started=threading.Event()
        self.servers=[]

    def cleanup(self):
        for process in getattr(self,'servers',[]):
            try: process.close()
            except Exception: pass
        if getattr(self,'supervisor',None) and self.supervisor.poll() is None:
            self.supervisor.terminate()
            self.supervisor.wait(timeout=5)
        for process in getattr(self,'extra_supervisors',[]):
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
            if process.stderr:
                process.stderr.close()
        if getattr(self,'supervisor_log_stream',None): self.supervisor_log_stream.close()
        if getattr(self,'pid_file',None) and self.pid_file.exists():
            try: os.kill(int(self.pid_file.read_text()),signal.SIGKILL)
            except (ProcessLookupError,ValueError): pass
        self.env_patch.stop()
        self.temp.cleanup()

    def server(self, handle='account:default', delay=False):
        active={'value':True}
        def notification(message):
            if delay:
                self.callback_started.set()
                deadline=time.monotonic()+1.5
                while active['value'] and time.monotonic()<deadline: time.sleep(.01)
            if active['value']: self.delivered.append(message)
        server=AppServer(self.root,notification,lambda message:None,lambda:None,
            executable=str(self.binary),supervisor_handle=handle)
        if not hasattr(self,'servers'): self.servers=[]
        self.servers.append(server)
        self.deactivate=getattr(self,'deactivate',[])
        self.deactivate.append(lambda:active.update(value=False))
        return server

    def test_explicit_root_refuses_decoy_environment_without_connecting(self):
        decoy = self.root / 'decoy'
        decoy.mkdir()
        decoy_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        decoy_socket.bind(str(decoy / 'supervisor.sock'))
        decoy_socket.listen(1)
        decoy_socket.settimeout(.1)
        try:
            with patch.dict(os.environ, {
                'CODEX_AGENTS_SUPERVISOR_MODE': '1',
                'CODEX_AGENTS_STATE_DIR': str(decoy),
                'CODEX_AGENTS_SUPERVISOR_FALLBACK': '1',
            }):
                with self.assertRaisesRegex(RuntimeError, 'state root mismatch.*refusing to contact'):
                    process_supervisor.attach(self.root, 'guard', [str(self.binary)], dict(os.environ))
            with self.assertRaises(socket.timeout):
                decoy_socket.accept()
        finally:
            decoy_socket.close()

    def test_live_child_rejects_changed_signature_with_operator_guidance(self):
        first = self.server()
        other = self.root / 'other-native'
        other.write_text(self.binary.read_text() + '\n')
        other.chmod(0o700)
        with patch.object(process_supervisor, 'native_launch_environment',
                          side_effect=lambda root, handle, command, env, cwd: dict(env)):
            with self.assertRaisesRegex(RuntimeError, 'active with PID.*operator CLI'):
                AppServer(self.root, lambda _: None, lambda _: None, lambda: None,
                          executable=str(other), supervisor_handle='account:default')
        self.assertEqual(first.call('model/list', {})['data'][0]['model'], 'fake')

    def test_supervisor_rpc_reply_passes_a_blocked_notification(self):
        entered, release = threading.Event(), threading.Event()
        def notification(message):
            if message.get('method') == 'command/exec/outputDelta':
                entered.set()
                release.wait(3)
        server = AppServer(self.root, notification, lambda _: None, lambda: None,
                           executable=str(self.binary), supervisor_handle='account:reply-lane')
        self.servers.append(server)
        try:
            submitted = server.submit('command/exec', {'processId': 'reply-lane'})
            self.assertTrue(entered.wait(2))
            self.assertEqual(server.wait(submitted, 1)['exitCode'], 0)
        finally:
            release.set()

    def test_exited_child_is_replaced_in_the_same_supervisor_generation(self):
        first = self.server()
        old_pid = int(self.pid_file.read_text())
        first.close()
        os.kill(old_pid, signal.SIGKILL)
        wait_for(lambda: process_start_time(old_pid) is None)
        second = self.server()
        self.assertEqual(second.proc.generation, 2)
        self.assertNotEqual(int(self.pid_file.read_text()), old_pid)
        self.assertEqual(second.call('model/list', {})['data'][0]['model'], 'fake')

    def test_operator_close_requires_exact_identity_and_closes_verified_fixture(self):
        server = self.server(handle='test:operator-close')
        live = next(row for row in status(self.root)['handles'] if row['id'] == 'test:operator-close')
        with self.assertRaisesRegex(RuntimeError, 'identity changed'):
            process_supervisor.admin_close_handle(self.root, live['id'], live['pid'] + 1,
                                                  live['startTime'], live['signature'])
        self.assertIsNotNone(process_start_time(live['pid']))
        result = process_supervisor.admin_close_handle(self.root, live['id'], live['pid'],
                                                       live['startTime'], live['signature'])
        self.assertEqual(result, {'closed': True, 'handle': live['id'], 'pid': live['pid']})
        self.assertIsNone(process_start_time(live['pid']))

    @unittest.skipUnless(sys.platform == 'darwin', 'macOS process identity precision contract')
    def test_process_start_time_distinguishes_children_started_in_same_second(self):
        children = []
        try:
            for _ in range(5):
                children = [subprocess.Popen(['/bin/sleep', '3']) for _ in range(2)]
                starts = [subprocess.check_output(
                    ['/bin/ps', '-p', str(child.pid), '-o', 'lstart='], text=True).strip()
                    for child in children]
                if starts[0] == starts[1]:
                    break
                for child in children:
                    child.terminate()
                    child.wait(timeout=3)
                children = []
            self.assertTrue(children, 'could not create same-second process fixtures')
            self.assertNotEqual(process_start_time(children[0].pid), process_start_time(children[1].pid))
        finally:
            for child in children:
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=3)

    def test_backend_detach_reattaches_to_same_fake_turn_and_replays_output(self):
        first=self.server(delay=True)
        native_pid=wait_for(lambda: int(self.pid_file.read_text()) if self.pid_file.exists() else None)
        submitted=first.submit('turn/start',{'clientUserMessageId':'turn-once','threadId':'thread'})
        with self.assertRaises(ResponseTimeout): first.wait(submitted,timeout=.1)
        self.assertTrue(self.callback_started.wait(3))
        first.close()  # backend transport closes; the supervisor retains the child and event journal
        self.deactivate[0]()  # the killed backend cannot finish its in-memory callback
        second=self.server()
        self.assertEqual(second.call('model/list',{} )['data'][0]['model'],'fake')
        self.release.touch()
        eventually=lambda: any(m.get('params',{}).get('delta')=='retained-output' for m in self.delivered)
        wait_for(eventually)
        logged_stderr = lambda: (self.root/'app-server.log').exists() and b'stderr-between-output' in (self.root/'app-server.log').read_bytes()
        try:
            wait_for(logged_stderr)
        except AssertionError:
            with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
                journal_stderr = db.execute("SELECT count(*) FROM events WHERE kind='stderr'").fetchone()[0]
            log = (self.root/'app-server.log').read_bytes() if (self.root/'app-server.log').exists() else b'<missing>'
            self.fail(f"journal stderr events={journal_stderr}; app-server.log={log!r}")
        self.assertEqual(int(self.pid_file.read_text()),native_pid)
        time.sleep(.2)
        self.assertEqual(sum(m.get('params',{}).get('delta')=='retained-output' for m in self.delivered),1)
        with (self.root/'supervisor.sqlite3').open('rb') as f: self.assertTrue(f.read(16).startswith(b'SQLite format 3'))

    def test_runtime_restart_reattaches_turn_and_replays_buffered_events_once(self):
        from codex_runtime import Runtime
        first = Runtime(self.root, AppServer)
        self.addCleanup(first.close)
        agent = first.create({'name':'Lead','cwd':str(self.root),'prompt':''}, draft=True, defer=True)
        with first.lock, first.db() as db:
            current = first.agent(agent['id'], db)
            current.update(threadId='thread', turnId='long-turn', status='running',
                           autoWake=True, inFlight=True, error=None)
            first.put(db, 'agents', current)
            first.put(db, 'tasks', {'id':'background-task','agent':agent['id'],
                'epoch':current['epoch'],'turnId':'long-turn','kind':'command',
                'status':'running','processId':'background-process'})
            first.put(db, 'monitors', {'id':'native-monitor','agent':agent['id'],
                'epoch':current['epoch'],'turnId':'long-turn','status':'running',
                'operation':{'accountKey':'default','epoch':current['epoch'],
                    'agent':agent['id'],'connectionId':'old-backend'}})
        with patch('codex_native_runtime.executable_for', return_value={'path':str(self.binary)}):
            original_server = first.connect()
        native_pid = int(self.pid_file.read_text())
        original_server.submit('longTurn', {'threadId':'thread'}, operation_id='long-turn-once')
        wait_for(lambda: (self.root/'phase-one').exists())

        # Detach the backend while the model turn remains live. Its first delta
        # is journaled after detach and must be replayed by the replacement.
        first.close()
        self.release.touch()
        wait_for(lambda: self._journal_method_count('item/agentMessage/delta') == 1)
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            payload, = db.execute("SELECT payload FROM events WHERE kind='stdout' "
                "AND payload LIKE '%item/agentMessage/delta%' ORDER BY sequence LIMIT 1").fetchone()
        receipt = json.loads(payload).get('_studioSupervisorReceivedAt')
        self.assertIsInstance(receipt, (int, float), 'Journal the supervisor receipt time before replay')

        entered, release_restore, constructed = threading.Event(), threading.Event(), threading.Event()
        observed, result, errors = [], [], []
        original_restore = Runtime.supervisor_reattached
        def restore(runtime, *args):
            observed.append(runtime)
            entered.set()
            if not release_restore.wait(4):
                raise RuntimeError('The fixture restore gate timed out')
            return original_restore(runtime, *args)
        def construct():
            try:
                with patch('codex_native_runtime.executable_for', return_value={'path': str(self.binary)}):
                    result.append(Runtime(self.root, AppServer))
            except Exception as error:
                errors.append(error)
            finally:
                constructed.set()
        with patch.object(Runtime, 'supervisor_reattached', restore):
            worker = threading.Thread(target=construct)
            worker.start()
            try:
                self.assertTrue(entered.wait(3))
                self.assertFalse(constructed.wait(.1), 'Runtime must restore before exposing the API')
                for lock in (observed[0].lock, observed[0].start_lock):
                    self.assertTrue(lock.acquire(timeout=1), 'Startup restore must wait outside both locks')
                    lock.release()
            finally:
                release_restore.set()
                worker.join(5)
                if result:
                    self.addCleanup(result[0].close)
        self.assertTrue(constructed.is_set())
        self.assertEqual(errors, [])
        second = result[0]
        self.addCleanup(second.close)
        with patch('codex_native_runtime.executable_for', return_value={'path':str(self.binary)}):
            replacement = second.connect()
        self.assertTrue(replacement.supervisor_resumed)
        self.assertEqual(int(self.pid_file.read_text()), native_pid)
        current = second.agent(agent['id'])
        self.assertEqual(current['status'], 'running')
        self.assertTrue(current['inFlight'])
        self.assertEqual(current['turnId'], 'long-turn')
        self.assertNotIn('Server restarted during a turn', current.get('error') or '')
        self.assertEqual(current['supervisorRestore']['status'], 'restored')
        self.assertEqual(current['supervisorRestore']['reason'], 'live_handle_resumed')
        with second.db() as db:
            task = json.loads(db.execute('SELECT record FROM runtime_tasks WHERE id=?',
                                         ('background-task',)).fetchone()[0])
            monitor = json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?',
                                            ('native-monitor',)).fetchone()[0])
        self.assertEqual(task['status'], 'running')
        # This fixture saves a monitor row but never starts a command/exec RPC.
        # The resumed model turn cannot restore that monitor's missing Future.
        self.assertEqual(monitor['status'], 'lost')
        self.assertIsNone(monitor.get('exitCode'))
        self.assertEqual(monitor['reattachRecovery']['proof'], 'operation_missing')
        with second.db() as db:
            notices = db.execute("SELECT id,text FROM runtime_events WHERE id='monitor:native-monitor'").fetchall()
        self.assertEqual(len(notices), 1)
        self.assertEqual(json.loads(notices[0]['text'])['status'], 'lost')
        # Consume this independent notice so it does not queue a second turn.
        with second.db() as db:
            notice = db.execute("SELECT status FROM runtime_events WHERE id='monitor:native-monitor'").fetchone()
            self.assertEqual(notice['status'], 'pending')
            delivered = db.execute("UPDATE runtime_events SET status='delivered' "
                                   "WHERE id='monitor:native-monitor' AND status='pending'")
            self.assertEqual(delivered.rowcount, 1)
        wait_for(lambda: self._stored_runtime_item(second, agent['id'], 'long-item') is not None)
        partial = self._stored_runtime_item(second, agent['id'], 'long-item')
        self.assertEqual(partial['text'], 'buffered-')
        self.root.joinpath('finish').touch()
        wait_for(lambda: second.agent(agent['id']).get('lastCompletedTurn') == 'long-turn')

        with second.db() as db:
            item_rows = db.execute('SELECT record FROM runtime_items WHERE id=?',
                                   (agent['id'] + ':long-item',)).fetchall()
            completed = db.execute('SELECT count(*) FROM runtime_completed_turns WHERE id=?',
                                   (agent['id'] + ':long-turn',)).fetchone()[0]
            restart_errors = db.execute('SELECT count(*) FROM runtime_items WHERE agent=? '
                "AND json_extract(record,'$.text') LIKE '%Server restarted during a turn%'",
                (agent['id'],)).fetchone()[0]
        self.assertEqual(len(item_rows), 1)
        self.assertEqual(json.loads(item_rows[0][0])['text'], 'buffered-final-answer')
        self.assertEqual(completed, 1)
        self.assertEqual(restart_errors, 0)
        # This monitor was never submitted, so it cannot hold the completed turn.
        self.assertEqual(second.agent(agent['id'])['status'], 'completed')
        self.assertNotIn('Server restarted during a turn', second.agent(agent['id']).get('error') or '')
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('longTurn'), 1)
        self.assertEqual(operations.count('command/exec'), 0)

    def test_runtime_restart_reattaches_live_monitor_and_delivers_exact_reply(self):
        from codex_runtime import Runtime
        first = Runtime(self.root, AppServer)
        self.addCleanup(first.close)
        agent = first.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': ''}, draft=True, defer=True)
        with patch('codex_native_runtime.executable_for', return_value={'path': str(self.binary)}):
            server = first.connect()
        operation = {'agent': agent['id'], 'epoch': agent['epoch'], 'accountKey': 'default',
                     'connectionId': first.connection_ids['default']}
        with first.lock, first.db() as db:
            first.put(db, 'monitors', {'id': 'live-monitor', 'agent': agent['id'], 'epoch': agent['epoch'],
                'status': 'running', 'created': time.time(), 'command': 'fixture command',
                'operation': operation, 'successExitCodes': [0], 'tail': '', 'bytes': 0,
                'log': str(self.root / 'monitor-logs' / 'live-monitor.log')})
        server.submit('command/exec', {'processId': 'live-monitor'},
                      operation_id='monitor:live-monitor')
        wait_for(lambda: (self.root / 'phase-one').exists())
        native_pid = int(self.pid_file.read_text())
        first.close()
        with patch('codex_native_runtime.executable_for', return_value={'path': str(self.binary)}):
            second = Runtime(self.root, AppServer)
            self.addCleanup(second.close)
            second.connect()
        with second.db() as db:
            monitor = json.loads(db.execute("SELECT record FROM runtime_monitors WHERE id='live-monitor'").fetchone()[0])
            notice = db.execute("SELECT status FROM runtime_events WHERE id='monitor:live-monitor'").fetchone()
        self.assertEqual(monitor['status'], 'running')
        self.assertEqual(notice['status'], 'cancelled')
        self.assertEqual(int(self.pid_file.read_text()), native_pid)
        self.release.touch()
        def completed():
            with second.db() as db:
                row = db.execute("SELECT record FROM runtime_monitors WHERE id='live-monitor'").fetchone()
            return json.loads(row[0])['status'] == 'completed'
        wait_for(completed)
        with second.db() as db:
            monitor = json.loads(db.execute("SELECT record FROM runtime_monitors WHERE id='live-monitor'").fetchone()[0])
            notices = db.execute("SELECT id,status,text FROM runtime_events WHERE id='monitor:live-monitor'").fetchall()
        self.assertEqual(monitor['exitCode'], 0)
        self.assertEqual(len(notices), 1)
        self.assertEqual(json.loads(notices[0]['text'])['status'], 'completed')
        methods = [json.loads(line)['method'] for line in (self.root / 'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(methods.count('command/exec'), 1)

    def test_runtime_restart_recovers_monitor_reply_after_supervisor_ack(self):
        from codex_runtime import Runtime
        first = Runtime(self.root, AppServer)
        self.addCleanup(first.close)
        agent = first.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': ''}, draft=True, defer=True)
        with patch('codex_native_runtime.executable_for', return_value={'path': str(self.binary)}):
            server = first.connect()
        operation = {'agent': agent['id'], 'epoch': agent['epoch'], 'accountKey': 'default',
                     'connectionId': first.connection_ids['default']}
        with first.lock, first.db() as db:
            first.put(db, 'monitors', {'id': 'acked-monitor', 'agent': agent['id'], 'epoch': agent['epoch'],
                'status': 'running', 'created': time.time(), 'command': 'fixture command',
                'operation': operation, 'successExitCodes': [0], 'tail': '', 'bytes': 0,
                'log': str(self.root / 'monitor-logs' / 'acked-monitor.log')})
        submitted = server.submit('command/exec', {'processId': 'acked-monitor'},
                                  operation_id='monitor:acked-monitor')
        self.assertEqual(server.wait(submitted)['exitCode'], 0)
        wait_for(lambda: server.proc.call('operationStatus',
            operationId='monitor:acked-monitor')['response'] is not None)
        first.close()
        with patch('codex_native_runtime.executable_for', return_value={'path': str(self.binary)}):
            second = Runtime(self.root, AppServer)
            self.addCleanup(second.close)
        def completed():
            with second.db() as db:
                row = db.execute("SELECT record FROM runtime_monitors WHERE id='acked-monitor'").fetchone()
            return json.loads(row[0])['status'] == 'completed'
        wait_for(completed)
        with second.db() as db:
            notices = db.execute("SELECT id,text FROM runtime_events WHERE id='monitor:acked-monitor'").fetchall()
        self.assertEqual(len(notices), 1)
        self.assertEqual(json.loads(notices[0]['text'])['exitCode'], 0)
        methods = [json.loads(line)['method'] for line in (self.root / 'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(methods.count('command/exec'), 1)

    def test_claude_bridge_transport_reports_the_same_live_child_reattach(self):
        observed = []
        fake_transport = ([str(self.binary), 'app-server', '--listen', 'stdio://'],
                          os.environ.copy())
        with patch('codex_claude.transport', return_value=fake_transport):
            first = AppServer(self.root, lambda _: None, lambda _: None, lambda: None,
                provider='claude', provider_options={'provider':'claude'},
                supervisor_handle='account:claude-fixture')
            native_pid = int(self.pid_file.read_text())
            self.assertFalse(first.supervisor_resumed)
            first.close()
            second = AppServer(self.root, lambda _: None, lambda _: None, lambda: None,
                provider='claude', provider_options={'provider':'claude'},
                supervisor_handle='account:claude-fixture',
                supervisor_reattached=observed.append)
        try:
            self.assertTrue(second.supervisor_resumed)
            deadline = time.monotonic() + 3
            while not observed and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertEqual(observed, [True])
            self.assertEqual(int(self.pid_file.read_text()), native_pid)
        finally:
            second.close()

    def test_reattach_barrier_does_not_block_constructor_and_precedes_native_events(self):
        first = self.server()
        native_pid = int(self.pid_file.read_text())
        first.close()
        gate = threading.Lock()
        gate.acquire()
        constructed = threading.Event()
        observed, result, errors = [], [], []

        def restore(resumed):
            with gate:
                observed.append(('restore', resumed))

        def construct():
            try:
                result.append(AppServer(self.root, lambda _: observed.append(('native', True)),
                    lambda _: None, lambda: None, supervisor_handle='account:default',
                    supervisor_reattached=restore))
            except Exception as error:
                errors.append(error)
            finally:
                constructed.set()

        worker = threading.Thread(target=construct, daemon=True)
        worker.start()
        try:
            self.assertTrue(constructed.wait(3), 'The constructor must not wait for the runtime restore lock')
            self.assertEqual(errors, [])
            second = result[0]
            self.servers.append(second)
            self.assertFalse(second.supervisor_reattach_future.done())
            second.call('burst', {'itemId': 'reattach-order'}, timeout=3)
            self.assertEqual(observed, [])
            gate.release()
            deadline = time.monotonic() + 3
            while len(observed) < 2 and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertEqual(observed[0], ('restore', True))
            self.assertIsNone(second.supervisor_reattach_future.result(timeout=1))
            self.assertTrue(any(kind == 'native' for kind, _ in observed[1:]))
            self.assertEqual(int(self.pid_file.read_text()), native_pid)
        finally:
            if gate.locked():
                gate.release()
            worker.join(timeout=4)
            for server in result:
                server.close()

    def _journal_method_count(self, method):
        try:
            with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
                rows = db.execute('SELECT payload FROM events WHERE kind="stdout"').fetchall()
            return sum(json.loads(row[0]).get('method') == method for row in rows)
        except sqlite3.OperationalError:
            return 0

    def test_reattach_future_preserves_original_failure_and_holds_native_callbacks(self):
        first = self.server()
        native_pid = int(self.pid_file.read_text())
        first.close()
        gate = threading.Event()
        failure = OSError('The fixture restore failed')
        observed = []
        def restore(_resumed):
            if not gate.wait(3):
                raise RuntimeError('The fixture restore gate timed out')
            raise failure
        second = AppServer(self.root, observed.append, lambda _: None, lambda: None,
                           supervisor_handle='account:default', supervisor_reattached=restore)
        self.servers.append(second)
        try:
            second.call('burst', {'itemId': 'failed-restore'}, timeout=3)
            self.assertEqual(observed, [])
            gate.set()
            with self.assertRaises(OSError) as result:
                second.supervisor_reattach_future.result(timeout=3)
            self.assertIs(result.exception, failure)
            wait_for(lambda: second.dispatcher_done.is_set())
            self.assertEqual(observed, [])
            self.assertIn(str(failure), second.transport_error)
            self.assertEqual(int(self.pid_file.read_text()), native_pid)
            self.assertGreater(self._journal_method_count('item/agentMessage/delta'), 0)
        finally:
            gate.set()

    @staticmethod
    def _stored_runtime_item(runtime, agent_id, suffix):
        with runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_items WHERE id=?',
                              (agent_id + ':' + suffix,)).fetchone()
        return json.loads(row[0]) if row else None

    def test_backend_identity_changes_reattach_the_same_native_process(self):
        first = self.server()
        native_pid = int(self.pid_file.read_text())
        first.close()
        os.environ['CODEX_AGENTS_BACKEND_ID'] = 'replacement-' + str(uuid.uuid4())
        second = self.server()
        self.assertEqual(second.call('model/list', {})['data'][0]['model'], 'fake')
        self.assertEqual(int(self.pid_file.read_text()), native_pid)
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 1)
        self.assertNotIn('turn/start', operations)

    def test_reattach_completes_initialized_if_backend_dies_after_initialize_result(self):
        crash = "\n".join([
            "import os,sys",
            "from pathlib import Path",
            "sys.path.insert(0,sys.argv[1]+'/scripts')",
            "from codex_runtime import AppServer",
            "original=AppServer.write",
            "def crash_before_initialized(self,value,operation_id=None):",
            "    if value.get('method')=='initialized': os._exit(73)",
            "    return original(self,value,operation_id=operation_id)",
            "AppServer.write=crash_before_initialized",
            "AppServer(Path(sys.argv[2]),lambda _:None,lambda _:None,lambda:None,",
            "          executable=os.environ['CODEX_BIN'],supervisor_handle='account:default')",
        ])
        crashed = subprocess.run([sys.executable, '-B', '-c', crash, str(ROOT), str(self.root)],
                                 env=os.environ.copy(), timeout=10, check=False)
        self.assertEqual(crashed.returncode, 73)
        wait_for(self._initialize_result_saved)
        self.assertFalse(self._operation_accepted('initialized:account:default'))

        second = self.server()
        self.assertEqual(second.call('model/list', {}, timeout=1)['data'][0]['model'], 'fake')
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 1)
        self.assertEqual(operations.count('initialized'), 1)
        self.assertTrue(self._operation_accepted('initialized:account:default'))
        second.close()
        third = self.server()
        self.assertEqual(third.call('model/list', {}, timeout=1)['data'][0]['model'], 'fake')
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 1)
        self.assertEqual(operations.count('initialized'), 1)

    def _initialize_result_saved(self):
        try:
            with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
                row = db.execute("SELECT init_result FROM handles WHERE id='account:default'").fetchone()
            return bool(row and row[0])
        except sqlite3.OperationalError:
            return False

    def _operation_accepted(self, operation_id):
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            return db.execute('SELECT 1 FROM operations WHERE handle=? AND operation_id=?',
                              ('account:default', operation_id)).fetchone() is not None

    def test_reopened_handle_completes_handshake_for_new_child_generation(self):
        first = self.server()
        old_pid = int(self.pid_file.read_text())
        first.close()
        os.kill(old_pid, signal.SIGKILL)
        wait_for(lambda: process_start_time(old_pid) is None)
        self.restart_supervisor()
        self.assertTrue(status(self.root)['recovery']['blocked'] is None)
        (self.root/'native-initialized').unlink(missing_ok=True)

        second = self.server()
        new_pid = wait_for(lambda: pid if (pid := int(self.pid_file.read_text())) != old_pid else None)
        self.assertNotEqual(new_pid, old_pid)
        self.assertEqual(second.call('model/list', {}, timeout=1)['data'][0]['model'], 'fake')
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 2)
        self.assertEqual(operations.count('initialized'), 2)
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            initialized = [row[0] for row in db.execute(
                "SELECT operation_id FROM operations WHERE handle='account:default' "
                "AND operation_id LIKE 'initialized:%' ORDER BY operation_id")]
        self.assertEqual(initialized, ['initialized:account:default', 'initialized:account:default:2'])

    def restart_supervisor(self):
        self.supervisor.terminate()
        self.supervisor.wait(timeout=5)
        self.supervisor_log_stream.close()
        self.supervisor_log_stream = self.supervisor_log.open('a')
        self.supervisor = subprocess.Popen([sys.executable, '-B', str(ROOT/'scripts/codex_process_supervisor.py'),
            '--state', str(self.root)], stdout=subprocess.DEVNULL, stderr=self.supervisor_log_stream)
        wait_for(lambda: status(self.root))

    def test_adjacent_deltas_keep_each_journal_receipt(self):
        server = self.server()
        runtime = Runtime(self.root/'runtime', server_factory=lambda *args: None)
        def close_runtime():
            self.assertTrue(all(server is None for server in runtime.servers.values()))
            self.assertEqual(runtime.__dict__.get('_late_servers', []), [])
            self.assertEqual(runtime.__dict__.get('_native_tools_retiring', {}), {})
            runtime.servers.clear()
            runtime.server = None
            runtime.close()
        self.addCleanup(close_runtime)
        agent = runtime.create({'name': 'Burst', 'cwd': str(self.root), 'prompt': ''}, draft=True, defer=True)
        with runtime.lock, runtime.db() as db:
            agent = runtime.agent(agent['id'], db)
            agent.update(threadId='thread', turnId='burst-turn', status='running', inFlight=True, autoWake=False)
            runtime.put(db, 'agents', agent)
        def notification(message):
            if message.get('method') == 'item/agentMessage/delta':
                runtime.notification(message)
            self.delivered.append(message)
        server.notification = notification
        server.supervisor_commit = lambda message, sequence: runtime.commit_supervisor_event(
            server.proc.handle, message, sequence, 'default', None)
        server.supervisor_event_applied = lambda sequence: runtime.supervisor_event_applied(server.proc.handle, sequence)
        gate, started = threading.Event(), threading.Event()
        server.enqueue(lambda _: (started.set(), gate.wait(3)), {})
        self.assertTrue(started.wait(3))
        submitted = server.submit('burst', {'itemId': 'burst-item'})
        server.wait(submitted, timeout=3)
        wait_for(lambda: server.callbacks.qsize() >= 7)
        gate.set()
        wait_for(lambda: server.proc.cursor == server.proc.read_cursor)
        fragments = [m['params']['delta'] for m in self.delivered
                     if m.get('method') == 'item/agentMessage/delta']
        self.assertEqual(fragments, list('abcdef'))
        stamped = [m for m in self.delivered if m.get('method') == 'item/agentMessage/delta']
        self.assertTrue(all(isinstance(m.get('_studioSupervisorReceivedAt'), (int, float)) for m in stamped))
        self.assertTrue(all(m['_studioSupervisorReceivedAt'] <= m['_studioReceivedAt'] for m in stamped))
        self.assertFalse(server.proc.ack_pending)
        item = next(i for i in runtime.transcript(agent['id'])['items'] if i['id'].endswith(':burst-item'))
        self.assertEqual(item['text'], 'abcdef')
        self.assertTrue(runtime.supervisor_event_applied(
            server.proc.handle, self.delivered[-1]['_studioSupervisorSequence']))
        self.assertTrue(runtime.supervisor_event_applied(server.proc.handle,
                                                          self.delivered[-1]['_studioSupervisorSequence']))
        def counts():
            with runtime.db() as db:
                return db.execute('SELECT coalesce(sum(count),0) FROM analytics_notifications WHERE agent=? AND method=?',
                                  (agent['id'], 'item/agentMessage/delta')).fetchone()[0]
        wait_for(lambda: counts() == 6)

    def test_adjacent_command_output_deltas_batch_without_losing_receipts(self):
        server = self.server()
        runtime = Runtime(self.root/'runtime', server_factory=lambda *args: None)
        def close_runtime():
            runtime.servers.clear()
            runtime.__dict__.setdefault('_native_tools_retiring', {}).clear()
            runtime.server = None
            runtime.close()
        self.addCleanup(close_runtime)
        agent = runtime.create({'name': 'Output burst', 'cwd': str(self.root), 'prompt': ''}, draft=True, defer=True)
        with runtime.lock, runtime.db() as db:
            agent = runtime.agent(agent['id'], db)
            agent.update(threadId='thread', turnId='burst-turn', status='running', inFlight=True)
            runtime.put(db, 'agents', agent)
            runtime.put(db, 'monitors', {'id': 'monitor-item', 'agent': agent['id'],
                'status': 'running', 'log': str(self.root/'monitor.log'), 'bytes': 0, 'tail': ''})
        runtime.notification({'method': 'item/started', 'params': {
            'threadId': 'thread', 'turnId': 'burst-turn',
            'item': {'id': 'command-item', 'type': 'commandExecution', 'command': 'fixture'}}})
        runtime.notification({'method': 'item/started', 'params': {
            'threadId': 'thread', 'turnId': 'burst-turn',
            'item': {'id': 'baseline-item', 'type': 'commandExecution', 'command': 'fixture'}}})
        count = 320
        baseline_at = time.time()
        baseline_delays, baseline_durations = [], []
        for index in range(count):
            message = {'method': 'item/commandExecution/outputDelta', 'params': {
                'threadId': 'thread', 'turnId': 'burst-turn', 'itemId': 'baseline-item',
                'delta': f'{index},'}, '_studioReceivedAt': baseline_at}
            started_at = time.time()
            message['_studioDispatchedAt'] = started_at
            runtime.notification(message, 'default', None)
            runtime.commit_supervisor_event('baseline', message, index + 1, 'default', None)
            # The unbatched baseline needs a consumer barrier for the bounded
            # asynchronous analytics queue. Overflow has a separate contract.
            self.assertTrue(runtime._analytics_capture_idle.wait(5))
            baseline_delays.append((started_at - baseline_at) * 1000)
            baseline_durations.append((time.time() - started_at) * 1000)
        terminal = {'method': 'item/completed', 'params': {'threadId': 'thread', 'turnId': 'burst-turn',
                    'item': {'id': 'baseline-item', 'type': 'commandExecution', 'exitCode': 0}}}
        terminal_started = time.time()
        runtime.notification(terminal, 'default', None)
        runtime.commit_supervisor_event('baseline', terminal, count + 1, 'default', None)
        baseline_delays.append((terminal_started - baseline_at) * 1000)
        baseline_durations.append((time.time() - terminal_started) * 1000)
        delivered = []
        after_delays, after_durations = [], []
        commit_sizes, commit_durations = [], []
        release_at = [None]
        def notification(message):
            started_at = time.time()
            runtime.notification(message, 'default', None)
            after_delays.append((message.get('_studioDispatchedAt', started_at)
                                 - (release_at[0] or started_at)) * 1000)
            after_durations.append((time.time() - started_at) * 1000)
            delivered.append(message)
        server.notification = notification
        def commit(message, sequence):
            started_at = time.time()
            runtime.commit_supervisor_event(server.proc.handle, message, sequence, 'default', None)
            commit_sizes.append(message.get('_studioSupervisorBatchCount', 1))
            commit_durations.append((time.time() - started_at) * 1000)
        server.supervisor_commit = commit
        server.supervisor_event_applied = lambda sequence: runtime.supervisor_event_applied(
            server.proc.handle, sequence)
        gate, started = threading.Event(), threading.Event()
        server.enqueue(lambda _: (started.set(), gate.wait(30)), {})
        self.assertTrue(started.wait(3))
        submitted = server.submit('outputBurst', {'itemId': 'command-item', 'count': count})
        server.wait(submitted, timeout=15)
        wait_for(lambda: server.callbacks.qsize() >= count + 1)
        queued_before_release = server.callbacks.qsize()
        release_at[0] = time.time()
        gate.set()
        wait_for(lambda: server.proc.cursor == server.proc.read_cursor)
        batches = [message for message in delivered
                   if message.get('method') == 'item/commandExecution/outputDelta']
        terminals = [message for message in delivered if message.get('method') == 'item/completed']
        self.assertEqual(len(batches), count)
        self.assertEqual(sum(size for size in commit_sizes), count + 1)
        self.assertLessEqual(len(commit_sizes), 4)
        self.assertEqual([m['_studioSupervisorSequence'] for m in batches],
                         sorted(m['_studioSupervisorSequence'] for m in batches))
        self.assertEqual(''.join(message['params']['delta'] for message in batches),
                         ''.join(f'{index},' for index in range(count)))
        self.assertEqual(len(terminals), 1)
        self.assertGreater(delivered.index(terminals[0]), delivered.index(batches[-1]))
        self.assertFalse(server.proc.ack_pending)
        with runtime.read_db() as db:
            row = db.execute('SELECT record FROM runtime_items WHERE id=?',
                             (agent['id'] + ':command-item',)).fetchone()
        item = json.loads(row['record'])
        self.assertEqual(json.loads(item['text'])['aggregatedOutput'],
                         ''.join(f'{index},' for index in range(count)))
        self.assertTrue(runtime._analytics_capture_idle.wait(5))
        with runtime.db() as db:
            total = db.execute('SELECT coalesce(sum(count),0) FROM analytics_notifications '
                               'WHERE agent=? AND method=?',
                               (agent['id'], 'item/commandExecution/outputDelta')).fetchone()[0]
        self.assertEqual(total, count * 2)
        percentile = lambda values, fraction: sorted(values)[min(len(values)-1, int((len(values)-1)*fraction))]
        print('command output load:', json.dumps({
            'events': count, 'beforeCallbacks': count + 1, 'afterCallbacks': len(batches) + len(terminals),
            'queuedBeforeRelease': queued_before_release,
            'beforeQueueDelayMs': {'p95': round(percentile(baseline_delays, .95), 2),
                                   'max': round(max(baseline_delays), 2)},
            'afterQueueDelayMs': {'p95': round(percentile(after_delays, .95), 2),
                                  'max': round(max(after_delays), 2)},
            'beforeCallbackDurationMs': {'p95': round(percentile(baseline_durations, .95), 2),
                                         'max': round(max(baseline_durations), 2),
                                         'total': round(sum(baseline_durations), 2)},
            'afterCallbackDurationMs': {'p95': round(percentile(after_durations, .95), 2),
                                        'max': round(max(after_durations), 2),
                                        'total': round(sum(after_durations), 2)},
            'afterCommitBatchSizes': commit_sizes,
            'afterCommitDurationMs': {'p95': round(percentile(commit_durations, .95), 2),
                                      'max': round(max(commit_durations), 2),
                                      'total': round(sum(commit_durations), 2)}}), flush=True)

        delivered.clear()
        gate, started = threading.Event(), threading.Event()
        server.enqueue(lambda _: (started.set(), gate.wait(30)), {})
        self.assertTrue(started.wait(3))
        submitted = server.submit('monitorBurst', {'count': count})
        server.wait(submitted, timeout=15)
        wait_for(lambda: server.callbacks.qsize() >= count)
        gate.set()
        wait_for(lambda: server.proc.cursor == server.proc.read_cursor)
        batches = [message for message in delivered
                   if message.get('method') == 'command/exec/outputDelta']
        self.assertLessEqual(len(batches), 3)
        expected = ''.join(f'{index},' for index in range(count)).encode()
        self.assertEqual((self.root/'monitor.log').read_bytes(), expected)
        with runtime.db() as db:
            row = json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?',
                                        ('monitor-item',)).fetchone()[0])
        self.assertEqual(row['bytes'], len(expected))
        self.assertEqual(row['tail'].encode(), expected)

    def test_delta_batch_does_not_repeat_a_durable_prefix(self):
        server = self.server()
        applied = {'sequence': 0}
        server.supervisor_commit = lambda message, sequence: applied.update(sequence=sequence)
        server.supervisor_event_applied = lambda sequence: sequence <= applied['sequence']
        server._supervisor_stream_batch_limit = 1
        gate, started = threading.Event(), threading.Event()
        server.enqueue(lambda _: (started.set(), gate.wait(3)), {})
        self.assertTrue(started.wait(3))
        submitted = server.submit('burst', {'itemId': 'partial'})
        server.wait(submitted, timeout=3)
        wait_for(lambda: server.callbacks.qsize() >= 7)
        with server.callbacks.mutex:
            deltas = [m for callback, m in server.callbacks.queue
                      if isinstance(m, dict) and m.get('method') == 'item/agentMessage/delta']
            applied['sequence'] = deltas[2]['_studioSupervisorSequence']
        gate.set()
        wait_for(lambda: server.proc.cursor == server.proc.read_cursor)
        self.assertEqual([m['params']['delta'] for m in self.delivered
                          if m.get('method') == 'item/agentMessage/delta'], list('def'))

    def test_failed_delta_batch_commit_keeps_every_journal_event(self):
        server = self.server()
        server.supervisor_event_applied = lambda sequence: False
        def fail_commit(message, sequence):
            raise RuntimeError('fixture durable commit failed')
        server.supervisor_commit = fail_commit
        gate, started = threading.Event(), threading.Event()
        server.enqueue(lambda _: (started.set(), gate.wait(3)), {})
        self.assertTrue(started.wait(3))
        native_pid = int(self.pid_file.read_text())
        submitted = server.submit('burst', {'itemId': 'failed-batch'})
        server.wait(submitted, timeout=3)
        wait_for(lambda: server.callbacks.qsize() >= 7)
        gate.set()
        wait_for(lambda: server.transport_error)
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            rows = db.execute('SELECT payload FROM events WHERE handle=? ORDER BY sequence',
                              (server.proc.handle,)).fetchall()
        fragments = [json.loads(row[0])['params']['delta'] for row in rows
                     if json.loads(row[0]).get('method') == 'item/agentMessage/delta']
        self.assertEqual(fragments, list('abcdef'))
        self.assertEqual(int(self.pid_file.read_text()), native_pid)

    def test_legacy_delta_gaps_require_durable_proof_and_preserve_requests(self):
        server = self.server()
        server._supervisor_stream_batch_limit = 1
        applied = {'sequence': 0}
        server.supervisor_commit = lambda message, sequence: applied.update(sequence=sequence)
        server.supervisor_event_applied = lambda sequence: sequence <= applied['sequence']
        with patch.object(server, 'release_slot', return_value=False):
            gate, started = threading.Event(), threading.Event()
            server.enqueue(lambda _: (started.set(), gate.wait(3)), {})
            self.assertTrue(started.wait(3))
            submitted = server.submit('burst', {'itemId': 'old-burst'})
            server.wait(submitted, timeout=3)
            wait_for(lambda: server.callbacks.qsize() >= 7)
            gate.set()
            wait_for(lambda: server.callbacks.unfinished_tasks == 0)
        self.assertLess(server.proc.cursor, server.proc.read_cursor)
        before = server.proc.cursor
        self.assertEqual(server.proc.ack_applied_deltas(lambda sequence: False), 0)
        self.assertEqual(server.proc.cursor, before)
        self.assertGreater(server.proc.ack_applied_deltas(server.supervisor_event_applied), 0)
        self.assertEqual(server.proc.cursor, server.proc.read_cursor)
        self.assertEqual(''.join(m['params']['delta'] for m in self.delivered
                                if m.get('method') == 'item/agentMessage/delta'), 'abcdef')
        # A missing tool request must not become permission to ACK its outcome.
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            seq = server.proc.cursor + 1
            payload = json.dumps({'id': 'unsettled', 'method': 'item/tool/call', 'params': {}})
            db.execute('INSERT INTO events(handle,sequence,kind,payload,size) VALUES (?,?,?,?,?)',
                       (server.proc.handle, seq, 'stdout', payload, len(payload)))
        server.proc.read_cursor = seq + 1
        server.proc.ack_pending.add(seq + 1)
        self.assertEqual(server.proc.ack_applied_deltas(lambda sequence: True), 0)
        self.assertEqual(server.proc.cursor, seq - 1)

    def test_legacy_launch_reattaches_without_restarting_or_resending(self):
        with patch.object(process_supervisor, 'native_launch_environment',
                          side_effect=lambda root, handle, command, env, cwd: dict(env)):
            first = self.server()
        native_pid = int(self.pid_file.read_text())
        first.close()
        os.environ['CODEX_AGENTS_BACKEND_ID'] = 'replacement-' + str(uuid.uuid4())
        second = self.server()
        self.assertEqual(second.call('model/list', {})['data'][0]['model'], 'fake')
        self.assertEqual(int(self.pid_file.read_text()), native_pid)
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 1)
        self.assertNotIn('turn/start', operations)

    def test_reattach_preserves_native_environment_after_backend_launcher_changes(self):
        first = self.server()
        native_pid = int(self.pid_file.read_text())
        first.close()
        with patch.dict(os.environ, {'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8',
                                    '__PYVENV_LAUNCHER__': '/different/backend/python'}):
            second = self.server()
        self.assertEqual(second.call('model/list', {})['data'][0]['model'], 'fake')
        self.assertEqual(int(self.pid_file.read_text()), native_pid)
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 1)
        self.assertNotIn('turn/start', operations)

    def test_backend_diagnostics_do_not_change_native_launch(self):
        first = self.server()
        native_pid = int(self.pid_file.read_text())
        first.close()
        with patch.dict(os.environ, {'CODEX_RUNTIME_LOCK_METRICS': '1',
                                    'CODEX_AGENTS_PROVIDER_CAPTURE': '1'}):
            second = self.server()
        self.assertEqual(second.call('model/list', {})['data'][0]['model'], 'fake')
        self.assertEqual(int(self.pid_file.read_text()), native_pid)
        second.close()
        # A child launched while a diagnostic was set reattaches after it is removed.
        self.assertEqual(self.server().call('model/list', {})['data'][0]['model'], 'fake')
        self.assertEqual(int(self.pid_file.read_text()), native_pid)

    def test_reattach_still_rejects_changed_credentials_options_and_command(self):
        first = self.server()
        native_pid = int(self.pid_file.read_text())
        first.close()
        for key in ('CODEX_HOME', 'HOME', 'OPENAI_API_KEY', 'STUDIO_CLAUDE_OPTIONS',
                    'CLAUDE_CONFIG_DIR'):
            with self.subTest(key=key), patch.dict(os.environ, {key: 'different-value'}):
                with self.assertRaisesRegex(RuntimeError, 'launch settings changed'):
                    self.server()
        with patch.dict(os.environ, {'PATH': '/usr/bin:/bin'}):
            with self.assertRaisesRegex(RuntimeError, 'launch settings changed'):
                process_supervisor.native_launch_environment(self.root, 'account:default',
                    [str(self.binary), 'different-command'], dict(os.environ), None)
        self.assertEqual(int(self.pid_file.read_text()), native_pid)

    def test_backend_lock_metrics_do_not_change_native_launch_or_reattach(self):
        with patch.dict(os.environ, {'CODEX_RUNTIME_LOCK_METRICS': '1'}):
            first = self.server()
        native_pid = int(self.pid_file.read_text())
        self.assertFalse('CODEX_RUNTIME_LOCK_METRICS' in
                         process_supervisor.process_launch_environment(native_pid))
        first.close()
        with patch.dict(os.environ, {'CODEX_RUNTIME_LOCK_METRICS': '0'}):
            second = self.server()
        self.assertTrue(second.proc.resumed)
        self.assertEqual(int(self.pid_file.read_text()), native_pid)
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 1)
        self.assertNotIn('turn/start', operations)

    def test_legacy_child_lock_metrics_preserve_exact_launch_on_reattach(self):
        with patch.dict(os.environ, {'CODEX_RUNTIME_LOCK_METRICS': '1'}), \
                patch.object(process_supervisor, 'native_launch_environment',
                             side_effect=lambda root, handle, command, env, cwd: dict(env)):
            first = self.server()
        native_pid = int(self.pid_file.read_text())
        first.close()
        with patch.dict(os.environ, {'CODEX_RUNTIME_LOCK_METRICS': '0'}):
            second = self.server()
        self.assertTrue(second.proc.resumed)
        self.assertEqual(int(self.pid_file.read_text()), native_pid)
        self.assertEqual(process_supervisor.process_launch_environment(native_pid)
                         ['CODEX_RUNTIME_LOCK_METRICS'], '1')
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 1)
        self.assertNotIn('turn/start', operations)

    def test_legacy_launch_rejects_account_changes_and_unverified_pid(self):
        with patch.object(process_supervisor, 'native_launch_environment',
                          side_effect=lambda root, handle, command, env, cwd: dict(env)):
            first = self.server()
        native_pid = int(self.pid_file.read_text())
        first.close()
        os.environ['CODEX_AGENTS_BACKEND_ID'] = 'replacement-' + str(uuid.uuid4())
        with patch.dict(os.environ, {'CODEX_HOME': str(self.root/'different-account')}):
            with self.assertRaisesRegex(RuntimeError, 'launch settings changed'):
                self.server()
        with patch.object(process_supervisor, 'process_start_time', return_value='different start'):
            with self.assertRaisesRegex(RuntimeError, 'Cannot verify'):
                self.server()
        os.environ['CODEX_AGENTS_BACKEND_ID'] = 'another-backend-' + str(uuid.uuid4())
        wait_for(lambda: status(self.root))
        second = self.server()
        self.assertEqual(second.call('model/list', {})['data'][0]['model'], 'fake')
        self.assertEqual(int(self.pid_file.read_text()), native_pid)

    def test_exited_legacy_launch_reaches_supervisor_without_replaying_receipts(self):
        with patch.object(process_supervisor, 'native_launch_environment',
                          side_effect=lambda root, handle, command, env, cwd: dict(env)):
            first = self.server()
        first.wait(first.submit('model/list', {}, operation_id='retained-operation'), timeout=2)
        old_pid = int(self.pid_file.read_text())
        first.close()
        os.kill(old_pid, signal.SIGKILL)
        wait_for(lambda: process_start_time(old_pid) is None)
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            self.assertIsNone(db.execute('SELECT closed_at FROM handles').fetchone()[0])
        os.environ['CODEX_AGENTS_BACKEND_ID'] = 'replacement-' + str(uuid.uuid4())
        with patch.object(process_supervisor, 'process_launch_environment',
                          side_effect=AssertionError('An exited child has no launch environment')):
            second = self.server()
        self.assertEqual(second.proc.generation, first.proc.generation + 1)
        self.assertFalse(second.proc.resumed)
        self.assertNotEqual(int(self.pid_file.read_text()), old_pid)
        self.assertEqual(second.call('model/list', {})['data'][0]['model'], 'fake')
        duplicate = second.proc.call('write', operationId='retained-operation', nativeId=50,
                                     message={'id': 50, 'method': 'model/list', 'params': {}})
        self.assertTrue(duplicate['duplicate'])
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 2)
        self.assertEqual(operations.count('model/list'), 2)
        self.assertNotIn('turn/start', operations)

    def test_dead_orphan_launch_still_requires_supervisor_ownership(self):
        with patch.object(process_supervisor, 'native_launch_environment',
                          side_effect=lambda root, handle, command, env, cwd: dict(env)):
            first = self.server()
        old_pid = int(self.pid_file.read_text())
        first.close()
        os.kill(old_pid, signal.SIGKILL)
        wait_for(lambda: process_start_time(old_pid) is None)
        command, env, cwd = [str(self.binary)], dict(os.environ), None
        clean = process_supervisor.native_launch_environment(
            self.root, 'account:default', command, env, cwd)
        isolated = process_supervisor.Supervisor(self.root)
        with self.assertRaisesRegex(RuntimeError, 'orphaned; native outcome remains unknown'):
            isolated.open_handle('account:default', command, clean, cwd)
        self.assertEqual(isolated.children, {})
        self.assertEqual(int(self.pid_file.read_text()), old_pid)

    def test_closed_legacy_launch_starts_new_generation_without_replaying_receipts(self):
        with patch.object(process_supervisor, 'native_launch_environment',
                          side_effect=lambda root, handle, command, env, cwd: dict(env)):
            first = self.server()
        first.wait(first.submit('model/list', {}, operation_id='retained-operation'), timeout=2)
        old_pid = int(self.pid_file.read_text())
        first.close()
        os.kill(old_pid, signal.SIGKILL)
        wait_for(lambda: process_start_time(old_pid) is None)
        self.restart_supervisor()
        os.environ['CODEX_AGENTS_BACKEND_ID'] = 'replacement-' + str(uuid.uuid4())
        with patch.object(process_supervisor, 'process_launch_environment',
                          side_effect=AssertionError('A closed child has no launch environment')):
            second = self.server()
        self.assertEqual(second.proc.generation, 2)
        self.assertNotEqual(int(self.pid_file.read_text()), old_pid)
        self.assertEqual(second.call('model/list', {})['data'][0]['model'], 'fake')
        duplicate = second.proc.call('write', operationId='retained-operation', nativeId=50,
                                     message={'id': 50, 'method': 'model/list', 'params': {}})
        self.assertTrue(duplicate['duplicate'])
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            self.assertEqual(db.execute("SELECT count(*) FROM operations WHERE operation_id='retained-operation'").fetchone()[0], 1)
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 2)
        self.assertEqual(operations.count('model/list'), 2)
        self.assertNotIn('turn/start', operations)

    def test_new_confirmed_attempt_keeps_exact_input_and_gets_new_native_write(self):
        server = self.server()
        self.release.touch()
        params = {'threadId': 'thread', 'clientUserMessageId': 'same-input'}
        first_id = 'turn:agent:same-input:attempt:rejected-attempt'
        second_id = 'turn:agent:same-input:attempt:accepted-attempt'
        server.wait(server.submit('turn/start', params, operation_id=first_id), timeout=3)
        duplicate = server.proc.call('write', operationId=first_id, nativeId=999,
                                    message={'id': 999, 'method': 'turn/start', 'params': params})
        self.assertTrue(duplicate['duplicate'])
        server.wait(server.submit('turn/start', params, operation_id=second_id), timeout=3)
        writes = [json.loads(line) for line in (self.root / 'native-ops.jsonl').read_text().splitlines()]
        turns = [row for row in writes if row['method'] == 'turn/start']
        self.assertEqual(len(turns), 2)
        self.assertEqual([row['params'] for row in turns], [params, params])

    def test_idle_claude_upgrade_uses_real_supervisor_transport_identity(self):
        from contextlib import contextmanager
        from codex_claude_controls import retire_idle_bridge
        server = self.server()
        server.initialize_result = {'capabilities': {'claudeVersion': 14}}
        server.provider_options = {}
        runtime = object.__new__(Runtime)
        runtime.root = self.root
        runtime.lock = threading.RLock()
        runtime.servers = {'default': server}
        runtime.server = server
        runtime.connection_ids = {'default': 'old-connection'}
        runtime.preparations = {}
        runtime.loaded = set()
        runtime.records = lambda db, table: []
        path = self.root / 'idle-runtime.sqlite3'
        @contextmanager
        def database():
            db = sqlite3.connect(path)
            try:
                db.execute('CREATE TABLE IF NOT EXISTS runtime_monitors(record TEXT)')
                db.execute('CREATE TABLE IF NOT EXISTS runtime_tasks(record TEXT)')
                yield db
            finally:
                db.close()
        runtime.db = database
        old_pid = server.proc.call('status')['pid']
        self.assertFalse(hasattr(server.proc, 'pid'))
        self.assertTrue(retire_idle_bridge(runtime, 'default', {}, server))
        self.assertNotIn('default', runtime.servers)
        wait_for(lambda: process_start_time(old_pid) is None)
        replacement = self.server()
        self.assertEqual(replacement.proc.generation, server.proc.generation + 1)
        self.assertNotEqual(replacement.proc.call('status')['pid'], old_pid)

    def test_closed_legacy_launch_without_recovery_proof_is_rejected(self):
        with patch.object(process_supervisor, 'native_launch_environment',
                          side_effect=lambda root, handle, command, env, cwd: dict(env)):
            first = self.server()
        old_pid = int(self.pid_file.read_text())
        first.close()
        os.kill(old_pid, signal.SIGKILL)
        wait_for(lambda: process_start_time(old_pid) is None)
        self.restart_supervisor()
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            db.execute('DELETE FROM degraded_handles')
        os.environ['CODEX_AGENTS_BACKEND_ID'] = 'replacement-' + str(uuid.uuid4())
        with self.assertRaisesRegex(RuntimeError, 'native outcome remains unknown'):
            self.server()
        self.assertEqual(int(self.pid_file.read_text()), old_pid)

    def test_closed_recovery_proof_cannot_replace_a_still_live_child(self):
        first = self.server()
        old_pid = int(self.pid_file.read_text())
        first.close()
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            started = db.execute('SELECT start_time FROM child_identities').fetchone()[0]
            db.execute('UPDATE handles SET closed_at=?', (time.time(),))
            db.execute('INSERT INTO degraded_handles VALUES (?,?,?,?)',
                       ('account:default', old_pid, started, time.time()))
        isolated = process_supervisor.Supervisor(self.root)
        with self.assertRaisesRegex(RuntimeError, 'native outcome remains unknown'):
            isolated.open_handle('account:default', [str(self.binary)], dict(os.environ), None)
        self.assertEqual(process_start_time(old_pid), started)
        self.assertEqual(isolated.children, {})

    def test_account_and_terminal_roots_share_the_state_supervisor(self):
        roots = [self.root/'account-servers'/'claude-local',
                 self.root/'account-servers'/('claude-profile-' + 'a'*64),
                 self.root/'terminal-server']
        for index, root in enumerate(roots):
            with self.subTest(root=root):
                root.mkdir(parents=True)
                handle = 'account:nested-' + str(index) if index < 2 else 'terminals'
                server = AppServer(root, lambda _: None, lambda _: None, lambda: None,
                                   executable=str(self.binary), supervisor_handle=handle,
                                   supervisor_root=self.root)
                self.servers.append(server)
                self.assertEqual(server.call('model/list', {})['data'][0]['model'], 'fake')
                self.assertEqual(server.proc.root, self.root)
                self.assertTrue((root/'app-server.log').exists())
        handles = status(self.root)['handles']
        self.assertEqual({row['id'] for row in handles},
                         {'account:nested-0', 'account:nested-1', 'terminals'})
        operations = [json.loads(line)['method'] for line in
                      (self.root/'native-ops.jsonl').read_text().splitlines()]
        self.assertEqual(operations.count('initialize'), 3)
        self.assertEqual(operations.count('model/list'), 3)
        self.assertNotIn('turn/start', operations)

    def test_sigkill_backend_mid_turn_terminal_and_monitor_output(self):
        cases=[('turn/start','item/agentMessage/delta','account:crash-turn'),
               ('process/spawn','process/outputDelta','terminals'),
               ('command/exec','command/exec/outputDelta','account:crash-monitor')]
        for trigger,notification,handle in cases:
            with self.subTest(trigger=trigger):
                os.environ['FAKE_TRIGGER_METHOD']=trigger
                os.environ['FAKE_TARGET_METHOD']=notification
                os.environ['FAKE_HANDLE']=handle
                marker=self.root/'callback-blocked'
                marker.unlink(missing_ok=True)
                backend=subprocess.Popen([sys.executable,'-B','-c',BACKEND_HARNESS,str(ROOT)],
                    env=os.environ.copy(),stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
                wait_for(lambda: marker.exists())
                native_pid=int(self.pid_file.read_text())
                backend.kill()
                backend.wait(timeout=5)
                self.release.touch()
                recovered=[]
                reattached=AppServer(self.root,lambda message:recovered.append(message),lambda _:None,lambda:None,
                    executable=str(self.binary),supervisor_handle=handle)
                self.servers.append(reattached)
                wait_for(lambda: any(message.get('method')==notification for message in recovered))
                self.assertEqual(int(self.pid_file.read_text()),native_pid)
                reattached.call('model/list',{})
                time.sleep(.1)
                with (self.root/'native-ops.jsonl').open() as stream:
                    methods=[json.loads(line)['method'] for line in stream]
                self.assertEqual(methods.count(trigger),1)
                reattached.close()
                self.release.unlink(missing_ok=True)

    def test_operation_receipt_and_cursor_errors_are_fail_closed(self):
        server=self.server()
        proxy=server.proc
        server.call('model/list',{})
        with self.assertRaisesRegex(RuntimeError,'stale or ahead'):
            proxy.call('replay',cursor=0)
        with self.assertRaisesRegex(RuntimeError,'stale or ahead'):
            proxy.call('replay',cursor=10**9)
        params={'clientUserMessageId':'exact-turn','threadId':'thread'}
        submitted=server.submit('turn/start',params,operation_id='turn:exact-turn')
        self.release.touch()
        result=server.wait(submitted)
        self.assertEqual(result['turn']['status'],'completed')
        # Stable client input identity maps to one durable supervisor receipt.
        with closing(sqlite3.connect(self.root/'supervisor.sqlite3')) as db:
            before=db.execute("SELECT count(*) FROM operations WHERE handle='account:default'").fetchone()[0]
        proxy.send_write({'id':999,'method':'turn/start','params':params},operation_id='turn:exact-turn')
        with closing(sqlite3.connect(self.root/'supervisor.sqlite3')) as db:
            after=db.execute("SELECT count(*) FROM operations WHERE handle='account:default'").fetchone()[0]
        self.assertEqual(after,before)
        with self.assertRaisesRegex(RuntimeError,'different content'):
            proxy.send_write({'id':1000,'method':'turn/start','params':{**params,'threadId':'other'}},
                             operation_id='turn:exact-turn')

    def test_competing_backend_attach_is_refused(self):
        first=self.server()
        os.environ['CODEX_AGENTS_BACKEND_ID']='second-backend-'+str(uuid.uuid4())
        with self.assertRaisesRegex(RuntimeError,'Another backend is attached'):
            self.server()
        first.close()

    def test_fsync_cost_for_durable_rpc_acceptance(self):
        server=self.server()
        samples=[]
        for i in range(80):
            result=server.call('model/list',{})
            self.assertTrue(result['data'])
            samples.append(server.proc.last_durable_ms)
        ordered=sorted(samples)
        p50=ordered[len(ordered)//2]
        p95=ordered[int((len(ordered)-1)*.95)]
        print(f'supervisor FULL-sync acceptance latency ms: p50={p50:.3f}, p95={p95:.3f}, n={len(samples)}')
        self.assertGreaterEqual(p50,0)
        self.assertGreaterEqual(p95,p50)

    def restart_supervisor_after_kill(self):
        self.supervisor.kill()
        self.supervisor.wait(timeout=5)
        self.supervisor=subprocess.Popen([sys.executable,'-B',str(ROOT/'scripts/codex_process_supervisor.py'),
            '--state',str(self.root)],stdout=subprocess.DEVNULL,stderr=self.supervisor_log_stream)
        if self.supervisor.poll() is not None:
            raise AssertionError(self.supervisor_log.read_text())
        deadline=time.monotonic()+8
        last_error=None
        while time.monotonic()<deadline:
            try:
                if status(self.root).get('recovery'):
                    return
            except Exception as error:
                last_error=repr(error)
            if self.supervisor.poll() is not None:
                raise AssertionError(self.supervisor_log.read_text())
            time.sleep(.05)
        raise AssertionError(f'supervisor recovery did not become ready: {last_error}; {self.supervisor_log.read_text()}')

    def test_supervisor_reboot_closes_dead_handles_and_preserves_receipts(self):
        server=self.server('account:dead-child')
        server.call('model/list',{})
        pid=int(self.pid_file.read_text())
        server.close()
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            receipts_before=db.execute("SELECT count(*) FROM operations WHERE handle='account:dead-child'").fetchone()[0]
        self.assertIsNotNone(process_start_time(pid))
        self.restart_supervisor_after_kill()
        wait_for(lambda: process_start_time(pid) is None)
        recovery=status(self.root)['recovery']
        self.assertFalse(recovery['degraded'])
        self.assertFalse(recovery['fallbackReady'])
        self.assertEqual(status(self.root)['handles'], [])
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            row=db.execute("SELECT outcome,detail FROM recovery_events WHERE handle='account:dead-child' ORDER BY id DESC LIMIT 1").fetchone()
            reason=db.execute("SELECT closed_reason FROM handles WHERE id='account:dead-child'").fetchone()[0]
            self.assertEqual(db.execute("SELECT count(*) FROM operations WHERE handle='account:dead-child'").fetchone()[0],receipts_before)
        self.assertEqual(row[0],'already-exited')
        self.assertIn('no longer running',row[1])
        self.assertIn('no longer running',reason)

    def test_launch_agent_waits_for_legacy_owner_then_recovers_its_live_child(self):
        os.environ['FAKE_STAY_ALIVE']='1'
        server=self.server('account:handoff-child')
        server.call('model/list',{})
        child_pid=int(self.pid_file.read_text())
        legacy_owner=self.supervisor
        legacy_pid=legacy_owner.pid
        waiter=subprocess.Popen([sys.executable,'-B',str(ROOT/'scripts/codex_process_supervisor.py'),
            '--state',str(self.root),'--wait-for-lease'],stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,text=True)
        self.extra_supervisors.append(waiter)
        deadline=time.monotonic()+5
        wait_message=''
        while time.monotonic()<deadline and f'supervisor waiting for owner {legacy_pid}' not in wait_message:
            if waiter.poll() is not None:
                self.fail(f'waiting owner exited early: {wait_message}{waiter.stderr.read()}')
            readable,_,_=select.select([waiter.stderr],[],[],.05)
            if readable:
                wait_message+=waiter.stderr.readline()
        self.assertIn(f'supervisor waiting for owner {legacy_pid}',wait_message)
        time.sleep(.25)
        self.assertIsNone(waiter.poll())
        readable,_,_=select.select([waiter.stderr],[],[],0)
        self.assertFalse(readable,'waiting owner logged more than once')
        self.assertEqual(status(self.root)['handles'][0]['pid'],child_pid)

        server.close()
        legacy_owner.kill()
        legacy_owner.wait(timeout=5)
        self.supervisor=waiter
        recovered=wait_for(lambda: status(self.root),timeout=8)
        self.assertTrue(recovered['recovery']['degraded'])
        self.assertTrue(recovered['recovery']['fallbackReady'])
        wait_for(lambda: process_start_time(child_pid) is None)
        self.assertIsNone(waiter.poll())
        os.environ.pop('FAKE_STAY_ALIVE',None)

    def test_supervisor_sigkill_terminates_verified_live_child_group(self):
        os.environ['FAKE_STAY_ALIVE']='1'
        server=self.server('account:live-child')
        server.call('model/list',{})
        pid=int(self.pid_file.read_text())
        server.close()
        self.restart_supervisor_after_kill()
        wait_for(lambda: process_start_time(pid) is None)
        recovery=status(self.root)['recovery']
        self.assertTrue(recovery['fallbackReady'])
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            outcome=db.execute("SELECT outcome FROM recovery_events WHERE handle='account:live-child' ORDER BY id DESC LIMIT 1").fetchone()[0]
        self.assertIn(outcome,{'terminated','killed'})
        finish_fallback(self.root)
        self.assertFalse(status(self.root)['recovery']['degraded'])
        resumed=self.server('account:live-child')
        self.assertEqual(resumed.call('model/list',{})['data'][0]['model'],'fake')
        os.environ.pop('FAKE_STAY_ALIVE',None)

    def test_reused_child_pid_closes_old_handle_without_signalling_new_process(self):
        os.environ['FAKE_STAY_ALIVE']='1'
        server=self.server('account:reused-pid')
        server.call('model/list',{})
        pid=int(self.pid_file.read_text())
        server.close()
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            db.execute("UPDATE child_identities SET start_time='different-process-start' WHERE handle='account:reused-pid'")
        self.restart_supervisor_after_kill()
        recovery=status(self.root)['recovery']
        self.assertFalse(recovery['fallbackReady'])
        self.assertIsNone(recovery['blocked'])
        self.assertEqual(status(self.root)['handles'], [])
        self.assertIsNotNone(process_start_time(pid))
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            self.assertIn('different process start time', db.execute(
                "SELECT closed_reason FROM handles WHERE id='account:reused-pid'").fetchone()[0])
        os.kill(pid,signal.SIGKILL)
        os.environ.pop('FAKE_STAY_ALIVE',None)

    def test_fallback_is_one_backend_generation_and_next_start_uses_supervisor(self):
        config={'supervisorEnabled':True,'environment':{'CODEX_AGENTS_SUPERVISOR_MODE':'1'},
                'unsetEnvironment':[],'codex':'fake'}
        with patch.dict(os.environ, {'CODEX_AGENTS_SUPERVISOR_FALLBACK':'stale'}):
            fallback=recover_backend.launch_environment(config,self.root,supervisor_fallback=True)
            next_generation=recover_backend.launch_environment(config,self.root)
        self.assertNotIn('CODEX_AGENTS_SUPERVISOR_MODE',fallback)
        self.assertEqual(fallback['CODEX_AGENTS_SUPERVISOR_FALLBACK'],'1')
        self.assertEqual(next_generation['CODEX_AGENTS_SUPERVISOR_MODE'],'1')
        self.assertNotIn('CODEX_AGENTS_SUPERVISOR_FALLBACK',next_generation)


if __name__=='__main__': unittest.main(verbosity=2)

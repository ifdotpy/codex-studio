#!/usr/bin/env python3
"""Private-state process-supervisor contracts with a deterministic fake model."""
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import uuid
from unittest.mock import patch

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
    elif method == 'command/exec':
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


class ProcessSupervisorContract(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(prefix='o6-',dir='/tmp')
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
        })
        self.env_patch.start()
        self.supervisor_log=self.root/'supervisor.stderr'
        self.supervisor_log_stream=self.supervisor_log.open('w')
        self.supervisor=subprocess.Popen([sys.executable,'-B',str(ROOT/'scripts/codex_process_supervisor.py'),
            '--state',str(self.root)],stdout=subprocess.DEVNULL,stderr=self.supervisor_log_stream)
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
        self.addCleanup(runtime.close)
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
        self.assertEqual(fragments, ['abcdef'])
        batch = next(m for m in self.delivered if m.get('method') == 'item/agentMessage/delta')
        self.assertEqual(len(batch['_studioNotificationSamples']), 6)
        self.assertFalse(server.proc.ack_pending)
        item = next(i for i in runtime.transcript(agent['id'])['items'] if i['id'].endswith(':burst-item'))
        self.assertEqual(item['text'], 'abcdef')
        self.assertTrue(runtime.supervisor_event_applied(server.proc.handle, batch['_studioSupervisorSequence']))
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
        release_at = [None]
        def notification(message):
            started_at = time.time()
            runtime.notification(message, 'default', None)
            runtime.commit_supervisor_event(server.proc.handle, message,
                                            message['_studioSupervisorSequence'], 'default', None)
            after_delays.append((message.get('_studioDispatchedAt', started_at)
                                 - (release_at[0] or started_at)) * 1000)
            after_durations.append((time.time() - started_at) * 1000)
            delivered.append(message)
        server.notification = notification
        server.supervisor_commit = lambda message, sequence: None
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
        self.assertLessEqual(len(batches), 3)
        self.assertEqual(sum(len(message['_studioNotificationSamples']) for message in batches), count)
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
                                        'total': round(sum(after_durations), 2)}}), flush=True)

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
                          if m.get('method') == 'item/agentMessage/delta'], ['def'])

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
                                   executable=str(self.binary), supervisor_handle=handle)
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
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            before=db.execute("SELECT count(*) FROM operations WHERE handle='account:default'").fetchone()[0]
        proxy.send_write({'id':999,'method':'turn/start','params':params},operation_id='turn:exact-turn')
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
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

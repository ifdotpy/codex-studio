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
from codex_runtime import AppServer, ResponseTimeout
from codex_process_supervisor import finish_fallback, process_start_time, status
import codex_process_supervisor as process_supervisor
import recover_backend

FAKE_NATIVE = r'''#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
Path(os.environ['FAKE_NATIVE_PID']).write_text(str(os.getpid()))
for line in sys.stdin:
    request=json.loads(line)
    if 'id' not in request: continue
    method=request.get('method')
    if not method: continue
    with open(os.environ['FAKE_NATIVE_OPS'],'a') as log: log.write(json.dumps({'method':method,'params':request.get('params')})+'\n')
    if method == 'initialize':
        result={'userAgent':'fake-model/1.0.0'}
    elif method == 'model/list':
        result={'data':[{'model':'fake'}]}
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
        self.binary.write_text(FAKE_NATIVE)
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

    def test_supervisor_sigkill_recovers_children_that_exit_on_pipe_close(self):
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
        self.assertTrue(recovery['degraded'])
        self.assertTrue(recovery['fallbackReady'])
        with sqlite3.connect(self.root/'supervisor.sqlite3') as db:
            outcome=db.execute("SELECT outcome FROM recovery_events WHERE handle='account:dead-child' ORDER BY id DESC LIMIT 1").fetchone()[0]
            self.assertEqual(db.execute("SELECT count(*) FROM operations WHERE handle='account:dead-child'").fetchone()[0],receipts_before)
        self.assertEqual(outcome,'already-exited')

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

    def test_reused_child_pid_fails_closed_without_signalling(self):
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
        self.assertIn('Cannot prove ownership',recovery['blocked'])
        self.assertIsNotNone(process_start_time(pid))
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

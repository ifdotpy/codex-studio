#!/usr/bin/env python3
"""Studio spawn and native provider contracts with a fake guest transport."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import concurrent.futures
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('linux_runtime_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_process_supervisor import Supervisor
from codex_runtime import PreparationPending

FAKE_NATIVE = r'''import json,sys
for line in sys.stdin:
    message=json.loads(line)
    if 'id' not in message: continue
    method=message.get('method')
    if method=='initialize': result={'userAgent':'linux-fixture/1'}
    elif method in ('thread/start','thread/resume','thread/read'):
        result={'thread':{'id':'linux-thread','turns':[],'status':{'type':'idle'}}}
    else: result={'ok':True}
    print(json.dumps({'id':message['id'],'result':result}),flush=True)
'''


class NativeGuest:
    def __init__(self, root):
        self.supervisor = Supervisor(root / 'guest-native')
        self.script = root / 'fake-native.py'
        self.script.write_text(FAKE_NATIVE)
        self.calls = []
        self.fail = False
        self.starts = []

    def request(self, method, params=None, **options):
        self.calls.append((method, copy.deepcopy(params), options))
        if self.fail:
            raise ConnectionError('The VM transport is unavailable')
        if method == 'health': return {'protocol':1}
        if method == 'provider.start':
            self.starts.append(copy.deepcopy(params))
            return self.supervisor.handle({'action':'open','handle':params['handle'],
                'command':[sys.executable,str(self.script)],'env':dict(os.environ),'cwd':str(self.script.parent)})
        if method == 'provider.list':
            return {'providers':[{'handle':key,'state':'running' if child.process.poll() is None else 'exited'}
                                 for key,child in self.supervisor.children.items()]}
        if method == 'provider.stop':
            row = next(row for row in self.supervisor.handle({'action':'health'})['handles'] if row['id']==params['handle'])
            child = self.supervisor.children[params['handle']]
            result = self.supervisor.handle({'action':'adminCloseHandle','handle':params['handle'],
                'expectedPid':row['pid'],'expectedStartTime':row['startTime'],'expectedSignature':row['signature']})
            self.close_streams(child)
            return result
        if method in ('workspace.archive','workspace.remove'):
            return {'state':'archived' if method.endswith('archive') else 'removed','freedBytes':0}
        if method == 'provider.rpc':
            return self.supervisor.handle(params)
        raise AssertionError(method)

    @staticmethod
    def close_streams(child):
        child.reader.join(timeout=5)
        child.stderr.join(timeout=5)
        for stream in (child.process.stdin,child.process.stdout,child.process.stderr):
            stream.close()

    def close(self):
        for child in self.supervisor.children.values():
            child.process.terminate()
            child.process.wait(timeout=5)
            self.close_streams(child)
        if self.supervisor.journal:
            self.supervisor.journal.close()


class LinuxRuntime(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='linux-runtime-')
        self.root = Path(self.temp.name).resolve()
        self.project = self.root / 'project'
        self.project.mkdir()
        self.runtime = f.ControlledRuntime(self.root / 'state', f.f.FakeServer)
        self.runtime.catalog = lambda account='default': copy.deepcopy(f.CATALOG)
        self.lead = self.runtime.new_lead({'cwd':str(self.project)})
        self.guest = NativeGuest(self.root)
        self.runtime._linux_vm_client = self.guest
        self.addCleanup(patch.stopall)
        patch('codex_linux_workspaces.ensure', return_value={'path':str(self.project),'mount':str(self.project)}).start()
        patch('codex_linux_vm_credentials.sync_credentials', return_value='fixture-hash').start()
        patch('codex_linux_vm_auth.bootstrap_codex').start()

    def tearDown(self):
        self.runtime.close()
        self.guest.close()
        self.temp.cleanup()

    def spawn(self):
        self.future = concurrent.futures.Future()
        self.runtime.start_image_base = lambda repo, agent_id=None, **options: self.future
        value = self.runtime.spawn_agents(self.runtime.agent(self.lead['id']), {'agents':[
            {'name':'Linux worker','prompt':'Complete the assigned task','environment':'linux'}]}, 'linux-spawn-fixture')
        self.worker_id = value['agents'][0]['id']
        return value['agents'][0]

    def ready(self):
        with self.runtime.lock, self.runtime.db() as db:
            worker = self.runtime.agent(self.worker_id,db)
            worker.update(imageWorkspaceReady=True,imageWorkspacePhase='ready',cwd=str(self.project))
            self.runtime.put(db,'agents',worker)
        return self.runtime.agent(self.worker_id)

    def test_linux_external_token_request_uses_the_host_refresh_path(self):
        self.spawn()
        worker = self.ready()
        server = self.runtime.connect_agent(worker)
        connection = self.runtime.agent_connection(worker)
        self.runtime.reply = unittest.mock.Mock()
        with patch('codex_linux_vm_auth.codex_access',return_value={
                'type':'chatgptAuthTokens','accessToken':'host-fresh-access',
                'chatgptAccountId':self.runtime.accounts.get('default')['accountId']}):
            self.runtime.request({'id':99,'method':'account/chatgptAuthTokens/refresh',
                'params':{'previousAccountId':self.runtime.accounts.get('default')['accountId']}},
                'default',connection)
        result, key, actual_connection = self.runtime.reply.call_args.args
        self.assertEqual(key,'default')
        self.assertEqual(actual_connection,connection)
        self.assertEqual(result['result']['accessToken'],'host-fresh-access')
        self.assertNotIn('refreshToken',json.dumps(result))

    def test_spawn_waits_for_base_without_host_input(self):
        result = self.spawn()
        self.assertEqual(result['environment'],'linux')
        self.assertEqual(result['workspace'],'linux')
        before = dict(self.runtime.servers)
        with self.assertRaises(PreparationPending):
            self.runtime.prepare_locked(self.runtime.agent(self.worker_id))
        self.assertEqual(self.runtime.servers,before)
        self.assertEqual(self.guest.starts,[])
        self.assertIsNone(self.runtime.agent(self.worker_id)['threadId'])

    def test_native_provider_uses_guest_while_host_account_identity_stays(self):
        self.spawn()
        worker = self.ready()
        host = self.runtime.connect('default')
        prepared = self.runtime.prepare_locked(worker)
        if isinstance(prepared,concurrent.futures.Future): prepared.result(timeout=10)
        current = self.runtime.agent(self.worker_id)
        self.assertEqual(current['threadId'],'linux-thread')
        self.assertEqual(current['accountKey'],'default')
        server = self.runtime.connect_agent(current)
        self.assertIs(self.runtime.server_for('default',self.runtime.agent_connection(current)),server)
        self.assertIs(self.runtime.servers['default'],host)
        self.assertTrue(server.supervisor_mode)
        self.assertEqual(self.guest.starts[0]['handle'],'linux-worker:'+self.worker_id)
        self.assertEqual(self.guest.starts[0]['transport'],'native')
        self.assertEqual(self.guest.starts[0]['agentId'],self.worker_id)
        self.assertEqual(server.call('fixture/read',{},timeout=5),{'ok':True})
        child = self.guest.supervisor.children['linux-worker:'+self.worker_id]
        pid = child.process.pid
        server.close()
        self.runtime.offline_linux_agents.add(self.worker_id)
        replacement = self.runtime.connect_agent(current)
        self.assertTrue(replacement.supervisor_resumed)
        self.assertEqual(child.process.pid,pid)
        self.assertEqual(replacement.call('fixture/read',{},timeout=5),{'ok':True})

    def test_base_failure_records_host_fallback_before_provider_input(self):
        self.spawn()
        self.runtime.image_base_completed(self.worker_id,{'state':'failed','error':'The VM could not start'})
        worker = self.runtime.agent(self.worker_id)
        self.assertEqual(worker['environment'],'host')
        self.assertEqual(worker['imageWorkspacePhase'],'fallback')
        self.assertIn('VM could not start',worker['imageWorkspaceError'])
        self.assertEqual(self.guest.starts,[])

    def test_archive_closes_guest_provider_and_restore_keeps_snapshot(self):
        from codex_agent_management import manage_agent
        self.spawn()
        worker = self.ready()
        server = self.runtime.connect_agent(worker)
        with self.runtime.lock,self.runtime.db() as db:
            worker = self.runtime.agent(self.worker_id,db)
            worker.update(status='completed',autoWake=True)
            self.runtime.put(db,'agents',worker)
            db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=?",(self.worker_id,))
        result = manage_agent(self.runtime,self.lead['id'],{'action':'archive','agent_id':self.worker_id,'reason':'Result checked'})
        self.assertEqual(result['status'],'archived')
        methods = [method for method,_,_ in self.guest.calls]
        self.assertLess(methods.index('provider.stop'),methods.index('workspace.archive'))
        self.assertNotIn('linux-worker:'+self.worker_id,self.guest.supervisor.children)
        with patch.object(self.runtime,'workspace_exec_prefix',return_value=[]):
            restored = manage_agent(self.runtime,self.lead['id'],{'action':'restore','agent_id':self.worker_id})
        self.assertEqual(restored['status'],'restored')
        self.assertEqual(self.runtime.agent(self.worker_id)['status'],'paused')
        self.assertTrue(self.runtime.agent(self.worker_id)['imageWorkspaceReady'])

    def test_linux_disconnect_does_not_interrupt_host_agents(self):
        self.spawn()
        worker = self.ready()
        server = self.runtime.connect_agent(worker)
        with self.runtime.lock,self.runtime.db() as db:
            worker = self.runtime.agent(self.worker_id,db)
            worker.update(status='running',inFlight=True,threadId='linux-thread',turnId='linux-turn',autoWake=True)
            self.runtime.put(db,'agents',worker)
        host_before = self.runtime.agent(self.lead['id'])
        self.runtime.disconnected('default',self.runtime.agent_connection(worker))
        after = self.runtime.agent(self.worker_id)
        self.assertEqual(after['status'],'interrupted')
        self.assertIn('Linux VM provider',after['error'])
        self.assertEqual(self.runtime.agent(self.lead['id']),host_before)
        self.assertNotIn('default',self.runtime.offline_accounts)
        self.assertFalse(self.runtime.connection_current('default',self.runtime.agent_connection(worker)))
        self.guest.fail=True
        with self.assertRaises(ConnectionError): self.runtime.connect_agent(after)
        self.guest.fail=False


if __name__=='__main__': unittest.main(verbosity=2)

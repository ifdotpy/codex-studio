#!/usr/bin/env python3
"""Claude account selection, identity and delivery boundaries. No model calls."""
import base64
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('account_fixture',Path(__file__).with_name('runtime-accounts-contract.py'))
f=importlib.util.module_from_spec(spec);spec.loader.exec_module(f)
import codex_claude
from codex_accounts import AccountStore

AUTH={'status':'ready','accountId':'claude:test@example.test','email':'test@example.test','plan':'max','_credentialIdentity':'claude:test@example.test'}

class MonitorServer(f.AccountServer):
    def call(self, method, params, timeout=60):
        if method == 'model/list':
            result=super().call(method, params, timeout)
            result['data'].extend([{**result['data'][0],'model':name} for name in ('default','sonnet')])
            return result
        if method in {'command/exec/write', 'command/exec/resize'}:
            self.calls.append((method, params))
            return {}
        return super().call(method, params, timeout)

class ClaudeProvider(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        self.env=patch.dict(os.environ,{'CODEX_HOME':str(self.root/'codex')});self.env.start();self.addCleanup(self.env.stop)
        self.auth=patch('codex_claude.auth_metadata',return_value=AUTH.copy());self.auth.start();self.addCleanup(self.auth.stop)
        self.installed=patch('codex_claude.installed',return_value='/fake/claude');self.installed.start();self.addCleanup(self.installed.stop)
        self.runtime=f.ControlledRuntime(self.root/'state',MonitorServer);self.addCleanup(self.runtime.close)
        self.runtime.accounts.discover()

    def test_subscription_identity_and_provider_selection(self):
        account=self.runtime.accounts.get('claude-local')
        self.assertEqual(account['provider'],'claude');self.assertNotIn('_credentialIdentity',account)
        a=self.runtime.new_lead({'cwd':str(self.root)})
        a=self.runtime.set_account(a['id'],'claude-local')
        self.assertEqual(a['model'],'default');self.assertEqual(a['provider'],'claude')
        self.assertEqual(self.runtime.worker_defaults(a)['model'],'gpt-6-luna')
        a=self.runtime.set_account(a['id'],'default')
        self.assertEqual(a['model'],'gpt-6-astra');self.assertEqual(a['provider'],'codex')
        with patch('codex_claude.auth_metadata',return_value={**AUTH,'accountId':'claude:changed'}):
            self.assertEqual(self.runtime.accounts.get('claude-local')['status'],'changed')
            with self.assertRaisesRegex(ValueError,'changed'):self.runtime.connect('claude-local')

    def test_claude_defaults_steer_and_native_limits(self):
        a=self.runtime.new_lead({'cwd':str(self.root),'account_key':'claude-local'})
        self.assertEqual(a['model'],'default')
        names={t['name'] for t in self.runtime.tool_definitions(a)}
        self.assertIn('orchestration_spawn',names)
        for name in ('orchestration_monitor','orchestration_cancel_monitor','orchestration_monitor_input'):
            self.assertIn(name,names)
        self.assertNotIn('orchestration_speak',names)
        with self.runtime.lock, self.runtime.db() as db:
            a.update(inFlight=True,turnId='active-turn',status='running')
            self.runtime.put(db,'agents',a)
        result=self.runtime.send(a['id'],'hello',delivery='after_tool')
        self.assertEqual(result['status'],'delivered')
        self.assertEqual(self.runtime.send(a['id'],'now',delivery='steer')['status'],'delivered')
        self.runtime.limits('claude-local')
        self.assertIn('claude-local',self.runtime.servers)

    def test_monitor_approval_sandbox_input_and_cancel_use_claude_connection(self):
        a=self.runtime.new_lead({'cwd':str(self.root),'account_key':'claude-local'})
        with self.runtime.lock, self.runtime.db() as db:
            a.update(autoWake=True,yoloMode=False)
            self.runtime.put(db,'agents',a)
        monitor=self.runtime.monitor(a['id'],{'command':'fixture','interactive':True})
        self.assertEqual(monitor['status'],'approval')
        self.assertFalse(any(method=='command/exec' for server in self.runtime.servers.values() for method,_ in server.calls))
        with self.runtime.db() as db:
            request=next(r for r in self.runtime.records(db,'requests') if r.get('params',{}).get('monitorId')==monitor['id'])
        self.runtime.answer(request['id'],{'decision':'accept'})
        server=self.runtime.connect('claude-local')
        f.f.eventually(lambda:any(method=='command/exec' for method,_ in server.calls))
        params=next(p for method,p in server.calls if method=='command/exec')
        self.assertEqual(params['sandboxPolicy']['type'],'workspaceWrite')
        self.assertIn(str(self.root.resolve()),params['sandboxPolicy']['writableRoots'])
        self.assertFalse(params['sandboxPolicy']['networkAccess'])
        self.assertTrue(params['tty']);self.assertTrue(params['streamStdin'])
        self.runtime.monitor_input(monitor['id'],{'text':'hello\n'},owner=a['id'])
        self.runtime.monitor_input(monitor['id'],{'rows':30,'cols':100},owner=a['id'])
        write=next(p for method,p in server.calls if method=='command/exec/write')
        self.assertEqual(base64.b64decode(write['deltaBase64']),b'hello\n')
        self.assertEqual(write['processId'],monitor['id'])
        size=next(p for method,p in server.calls if method=='command/exec/resize')
        self.assertEqual(size['size'],{'rows':30,'cols':100})
        self.runtime.cancel_monitor(monitor['id'],owner=a['id'])
        self.assertTrue(any(method=='command/exec/terminate' and p['processId']==monitor['id'] for method,p in server.calls))
        with self.assertRaisesRegex(ValueError,'not active'):
            self.runtime.monitor_input(monitor['id'],{'text':'late'},owner=a['id'])
        self.assertNotIn('default',self.runtime.servers)

    def test_claude_resume_refreshes_studio_tools(self):
        a=self.runtime.new_lead({'cwd':str(self.root),'account_key':'claude-local'})
        a=self.runtime.prepare(a)
        self.runtime.loaded.discard(a['id'])
        self.runtime.prepare(a)
        server=self.runtime.connect('claude-local')
        resumed=next(p for method,p in reversed(server.calls) if method=='thread/resume')
        names={tool['name'] for tool in resumed['dynamicTools']}
        self.assertIn('orchestration_monitor',names)
        self.assertNotIn('orchestration_speak',names)

    def test_native_command_has_its_own_queued_batch(self):
        a=self.runtime.new_lead({'cwd':str(self.root),'account_key':'claude-local'})
        with self.runtime.lock, self.runtime.db() as db:
            a=self.runtime.agent(a['id'],db)
            a.update(autoWake=True,status='queued',inFlight=False)
            self.runtime.put(db,'agents',a)
            self.runtime.enqueue(db,a,'user','/compact','command-message')
            self.runtime.enqueue(db,a,'user','Continue after compact','next-message')
        with self.runtime.lock, patch.object(self.runtime.pool,'submit') as submit:
            self.runtime.dispatch()
            starts=[call for call in submit.call_args_list if call.args[0]==self.runtime.start]
            self.assertEqual(len(starts),1)
            self.assertEqual([row['id'] for row in starts[0].args[2]],['command-message'])
            with self.runtime.db() as db:
                self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='next-message'").fetchone()[0],'pending')

    def test_explicit_claude_worker_keeps_selected_provider(self):
        a=self.runtime.new_lead({'cwd':str(self.root),'account_key':'claude-local'})
        catalog={'data':[{'model':'sonnet','supportedReasoningEfforts':[{'reasoningEffort':'max'}],
                         'defaultReasoningEffort':'medium','serviceTiers':[]}]}
        child=self.runtime.create({'name':'Worker','prompt':'Read the project','model':'sonnet'},parent=a['id'],defer=True,_catalog=('claude-local',catalog))
        self.assertEqual(child['accountKey'],'claude-local')
        self.assertEqual(child['provider'],'claude')
        self.assertEqual(child['model'],'sonnet')

    def test_environment_cannot_select_api_billing(self):
        with patch.dict(os.environ,{'ANTHROPIC_API_KEY':'not-a-real-key','ANTHROPIC_BASE_URL':'https://invalid.test','CLAUDE_CODE_USE_BEDROCK':'1'}):
            env=codex_claude.subscription_env()
            self.assertNotIn('ANTHROPIC_API_KEY',env)
            self.assertNotIn('ANTHROPIC_BASE_URL',env)
            self.assertNotIn('CLAUDE_CODE_USE_BEDROCK',env)

if __name__=='__main__':unittest.main()

#!/usr/bin/env python3
"""Claude account selection, identity and delivery boundaries. No model calls."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import base64
import importlib.util
import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('account_fixture',Path(__file__).with_name('runtime-accounts-contract.py'))
f=importlib.util.module_from_spec(spec);spec.loader.exec_module(f)
import codex_claude
from codex_accounts import AccountStore
from codex_native_errors import NativeRpcError
from codex_runtime import PreparationPending
from codex_connection_recovery import recover, preparation_eligible

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
        if method == 'thread/compact/start':
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

    def test_pinned_keychain_denial_uses_native_account_proof_before_input(self):
        denied = {'status': 'error', 'accountId': None, 'email': None, 'plan': None,
                  '_authErrorKind': 'keychain', 'error': 'Keychain interaction is unavailable'}
        self.auth.stop()
        denied_probe = patch('codex_claude.auth_metadata', return_value=denied)
        native = denied_probe.start()
        self.addCleanup(denied_probe.stop)
        account = self.runtime.accounts.get('claude-local')
        self.assertEqual(account['status'], 'error')
        self.assertTrue(account['canAttemptNativeProof'])
        self.assertNotIn('claude-local', self.runtime.servers)
        proof = {'account': {'type': 'claude', 'email': AUTH['email'], 'planType': 'max'}}
        original = MonitorServer.call
        def call(server, method, params, timeout=60):
            if method == 'account/read':
                self.assertGreaterEqual(timeout, 60 if not params.get('passive') else 10)
                server.calls.append((method, params))
                return proof
            return original(server, method, params, timeout)
        with patch.object(MonitorServer, 'call', call):
            server = self.runtime.connect('claude-local')
            self.assertEqual(self.runtime.accounts.get('claude-local')['status'], 'error')
            server.initialize_result = {'capabilities': {'passiveAccountRead': True}}
            self.assertEqual(self.runtime.accounts.get('claude-local')['status'], 'ready')
            agent = self.runtime.new_lead({'cwd': str(self.root), 'account_key': 'claude-local'})
            self.runtime.prepare(agent)
            self.assertEqual(server.calls[0][0], 'account/read')
            self.assertTrue(any(method == 'thread/start' for method, _ in server.calls))
            self.assertFalse(any(method == 'turn/start' for method, _ in server.calls))
            proof['account']['email'] = 'foreign@example.test'
            self.assertEqual(self.runtime.accounts.get('claude-local')['status'], 'error')
            with self.assertRaisesRegex(ValueError, 'original Claude subscription'):
                self.runtime.connect('claude-local')
            proof['account']['email'] = AUTH['email']
            generation = self.runtime.connection_ids['claude-local']
            def changed(server, method, params, timeout=60):
                if method == 'account/read':
                    self.runtime.connection_ids['claude-local'] += '-next'
                    return proof
                return original(server, method, params, timeout)
            with patch.object(MonitorServer, 'call', changed):
                with self.assertRaisesRegex(ValueError, 'connection changed'):
                    self.runtime.connect('claude-local')
            self.runtime.connection_ids['claude-local'] = generation
            for kind in ('parser', 'spawn', 'timeout'):
                native.return_value = {**denied, '_authErrorKind': kind}
                self.assertNotIn('canAttemptNativeProof', self.runtime.accounts.get('claude-local'))
                with self.assertRaisesRegex(ValueError, 'Keychain interaction'):
                    self.runtime.connect('claude-local')
            native.return_value = denied
            with self.runtime.accounts.lock:
                self.runtime.accounts.data['accounts']['claude-local'].pop('_credentialIdentity')
            self.assertNotIn('canAttemptNativeProof', self.runtime.accounts.get('claude-local'))
            with self.assertRaisesRegex(ValueError, 'Keychain interaction'):
                self.runtime.connect('claude-local')

    def busy_resume(self, *, message='Claude is still working', code=-32000,
                    read_thread=None, read_gate=None, stale=False, read_error=None):
        agent = self.runtime.new_lead({'cwd': str(self.root), 'account_key': 'claude-local'})
        agent = self.runtime.prepare(agent)
        self.runtime.loaded.discard(agent['id'])
        server = self.runtime.servers['claude-local']
        original = server.call

        def call(method, params, timeout=60):
            if method == 'thread/resume':
                server.calls.append((method, params))
                if stale:
                    with self.runtime.lock, self.runtime.db() as db:
                        current = self.runtime.agent(agent['id'], db)
                        current['epoch'] += 1
                        self.runtime.put(db, 'agents', current)
                raise NativeRpcError({'code': code, 'message': message})
            if method == 'thread/read':
                server.calls.append((method, params))
                if read_gate is not None:
                    read_gate.wait(5)
                if read_error:
                    raise read_error
                return {'thread': {'id': read_thread or agent['threadId']},
                        'model': 'default', 'approvalPolicy': 'on-request'}
            return original(method, params, timeout)

        server.call = call
        return agent, server

    def test_busy_resume_reattaches_and_delivers_the_original_input_once(self):
        agent, server = self.busy_resume()
        self.runtime.send(agent['id'], 'Continue the saved task', 'busy-resume-input')
        self.runtime.dispatch()
        f.f.eventually(lambda: self.runtime.delivery_receipt('busy-resume-input')['status'] == 'delivered')
        self.runtime.dispatch()
        calls = [(m, p) for m, p in server.calls if m in {'thread/resume', 'thread/read', 'turn/start'}]
        self.assertEqual([m for m, _ in calls], ['thread/resume', 'thread/read', 'turn/start'])
        self.assertEqual(calls[1][1], {'threadId': agent['threadId'], 'includeTurns': False})
        self.assertEqual(calls[2][1]['clientUserMessageId'], 'busy-resume-input')
        self.assertEqual(self.runtime.agent(agent['id'])['threadId'], agent['threadId'])
        self.assertFalse(any(m in {'turn/interrupt', 'thread/unsubscribe'} for m, _ in server.calls))

    def test_other_resume_rejections_do_not_use_a_read(self):
        for message, code in [('Claude is still working', -32600), ('Different failure', -32000)]:
            with self.subTest(message=message, code=code):
                agent, server = self.busy_resume(message=message, code=code)
                with self.assertRaises(NativeRpcError):
                    self.runtime.prepare(agent)
                self.assertFalse(any(m == 'thread/read' for m, _ in server.calls))
                self.assertNotIn(agent['id'], self.runtime.loaded)

    def test_busy_resume_from_a_stale_epoch_cannot_reattach(self):
        agent, server = self.busy_resume(stale=True)
        with self.assertRaises(NativeRpcError):
            self.runtime.prepare(agent)
        self.assertFalse(any(m == 'thread/read' for m, _ in server.calls))
        self.assertNotIn(agent['id'], self.runtime.loaded)

    def test_busy_resume_read_requires_the_exact_thread_identity(self):
        agent, server = self.busy_resume(read_thread='another-thread')
        with self.assertRaisesRegex(RuntimeError, 'different thread identity; outcome unknown'):
            self.runtime.prepare(agent)
        self.assertEqual(sum(m == 'thread/read' for m, _ in server.calls), 1)
        self.assertNotIn(agent['id'], self.runtime.loaded)
        self.assertFalse(any(m == 'turn/start' for m, _ in server.calls))

    def test_busy_resume_waits_for_the_read_receipt_without_resubmission(self):
        gate = threading.Event()
        self.addCleanup(gate.set)
        agent, server = self.busy_resume(read_gate=gate)
        self.runtime.preparation_wait_seconds = .02
        with self.assertRaises(PreparationPending) as pending:
            self.runtime.prepare(agent)
        self.assertNotIn(agent['id'], self.runtime.loaded)
        with self.assertRaises(PreparationPending):
            self.runtime.prepare(agent)
        gate.set()
        self.assertEqual(pending.exception.future.result(3)['threadId'], agent['threadId'])
        self.assertEqual(sum(m == 'thread/read' for m, _ in server.calls), 1)
        self.assertEqual(sum(m == 'thread/resume' for m, _ in server.calls), 1)

    def test_busy_resume_read_failure_remains_visible(self):
        agent, server = self.busy_resume(read_error=NativeRpcError({'code': -32000,
                                                                  'message': 'Claude is still working'}))
        with self.assertRaises(NativeRpcError):
            self.runtime.prepare(agent)
        self.assertEqual(sum(m == 'thread/read' for m, _ in server.calls), 1)
        self.assertNotIn(agent['id'], self.runtime.loaded)

    def test_reattach_preserves_context_for_both_bridge_versions(self):
        for current_bridge in (False, True):
            with self.subTest(current_bridge=current_bridge):
                agent, server = self.busy_resume()
                if current_bridge:
                    old_call = server.call
                    def call(method, params, timeout=60):
                        if method == 'thread/resume':
                            server.calls.append((method, params))
                            return {'thread': {'id': agent['threadId']}, 'model': 'default', 'reattached': True}
                        return old_call(method, params, timeout)
                    server.call = call
                previous = agent['preparedContext']
                guidance = '[Studio role skill: fixture]\nSource: fixture\nNEW ROLE POLICY\n[End Studio role skill]'
                with patch.object(self.runtime, 'role_guidance', return_value=guidance):
                    self.runtime.prepare(agent)
                    with self.runtime.lock, self.runtime.db() as db:
                        current = self.runtime.agent(agent['id'], db)
                        self.assertEqual(current['preparedContext'], previous)
                        self.assertIn('NEW ROLE POLICY', self.runtime.model_turn_context(db, current, 'context-probe'))

    def test_saved_busy_preparation_restores_only_the_unsent_input(self):
        agent, server = self.busy_resume()
        self.runtime.send(agent['id'], 'The original task', 'saved-busy-input')
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(agent['id'], db)
            attempt = {'id': 'saved-attempt', 'submitted': False, 'epoch': agent['epoch'],
                       'accountKey': agent['accountKey'], 'activeAtReservation': False,
                       'events': ['saved-busy-input']}
            agent.update(status='failed', inFlight=False, turnId=None, startAttempt=attempt,
                         error='{"code": -32000, "message": "Claude is still working"}')
            self.runtime.put(db, 'agents', agent)
        self.assertTrue(preparation_eligible(agent))
        for field, value in [('provider', 'codex'), ('threadId', None), ('autoWake', False),
                             ('inFlight', True), ('nativeFailureHold', {'reason': 'review'})]:
            self.assertFalse(preparation_eligible({**agent, field: value}), field)
        self.assertFalse(preparation_eligible({**agent, 'startAttempt': {**attempt, 'submitted': True}}))
        self.assertEqual(recover(self.runtime, agent['id'])['status'], 'input_restored')
        self.assertEqual(recover(self.runtime, agent['id'])['status'], 'superseded')
        self.runtime.dispatch()
        f.f.eventually(lambda: self.runtime.delivery_receipt('saved-busy-input')['status'] == 'delivered')
        starts = [p for m, p in server.calls if m == 'turn/start']
        self.assertEqual([p['clientUserMessageId'] for p in starts], ['saved-busy-input'])

    def test_saved_busy_preparation_keeps_an_uncertain_input_reserved(self):
        agent, server = self.busy_resume()
        self.runtime.send(agent['id'], 'An uncertain task', 'uncertain-busy-input')
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(agent['id'], db)
            agent.update(status='failed', inFlight=False, turnId=None,
                error='{"code": -32000, "message": "Claude is still working"}',
                startAttempt={'id': 'saved-attempt', 'submitted': False, 'epoch': agent['epoch'],
                    'accountKey': agent['accountKey'], 'events': ['uncertain-busy-input']})
            self.runtime.put(db, 'agents', agent)
            db.execute("UPDATE runtime_events SET status='uncertain' WHERE id='uncertain-busy-input'")
        self.assertEqual(recover(self.runtime, agent['id'])['status'], 'unconfirmed')
        self.assertEqual(self.runtime.delivery_receipt('uncertain-busy-input')['status'], 'uncertain')
        self.assertFalse(any(m == 'turn/start' for m, _ in server.calls))

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
            a.update(inFlight=True,turnId='active-turn',threadId='claude-thread',status='running')
            self.runtime.put(db,'agents',a)
        self.runtime.loaded.add(a['id'])
        result=self.runtime.send(a['id'],'hello',delivery='after_tool')
        self.assertEqual(result['status'],'queued')
        self.runtime.dispatch()
        f.f.eventually(lambda:self.runtime.delivery_receipt(result['id'])['status']=='delivered')
        second=self.runtime.send(a['id'],'now',delivery='steer')
        self.runtime.dispatch()
        f.f.eventually(lambda:self.runtime.delivery_receipt(second['id'])['status']=='delivered')
        self.runtime.limits('claude-local')
        self.assertIn('claude-local',self.runtime.servers)

    def test_queued_child_result_steers_through_selected_claude_account(self):
        a=self.runtime.new_lead({'cwd':str(self.root),'account_key':'claude-local'})
        with self.runtime.lock, self.runtime.db() as db:
            current=self.runtime.agent(a['id'],db)
            current.update(inFlight=True,turnId='active-turn',threadId='claude-thread',status='running')
            self.runtime.put(db,'agents',current)
            self.runtime.enqueue(db,current,'child_result','Claude child result','claude-live-child')
        self.runtime.loaded.add(a['id'])
        self.runtime.dispatch()
        f.f.eventually(lambda:self.runtime.delivery_receipt('claude-live-child')['status']=='delivered')
        server=self.runtime.servers['claude-local']
        calls=[params for method,params in server.calls if method=='turn/start']
        self.assertEqual(len(calls),1)
        self.assertEqual(calls[0]['clientUserMessageId'],'claude-live-child')

    def test_claude_bridge_steer_rejection_waits_for_idle_turn(self):
        a=self.runtime.new_lead({'cwd':str(self.root),'account_key':'claude-local'})
        with self.runtime.lock,self.runtime.db() as db:
            a.update(inFlight=True,turnId='claude-active',threadId='claude-thread',status='running')
            self.runtime.put(db,'agents',a)
        self.runtime.loaded.add(a['id'])
        server=self.runtime.connect('claude-local')
        original=server.call
        def reject(method,params,timeout=60):
            if method=='turn/start' and params.get('clientUserMessageId')=='claude-rejected':
                server.calls.append((method,params))
                raise RuntimeError('The steer belongs to a different Claude turn')
            return original(method,params,timeout)
        with patch.object(server,'call',side_effect=reject):
            self.runtime.send(a['id'],'Keep Claude input','claude-rejected')
            self.runtime.dispatch()
            f.f.eventually(lambda:self.runtime.agent(a['id']).get('steerRejectedTurnId')=='claude-active')
        self.assertEqual(self.runtime.delivery_receipt('claude-rejected')['status'],'pending')
        self.assertIsNone(self.runtime.agent(a['id'])['error'])
        self.runtime.dispatch()
        self.assertEqual(sum(p.get('clientUserMessageId')=='claude-rejected'
            for method,p in server.calls if method=='turn/start'),1)
        server.complete('claude-thread','claude-active')
        self.runtime.dispatch()
        f.f.eventually(lambda:self.runtime.delivery_receipt('claude-rejected')['status']=='delivered')
        self.assertEqual(sum(p.get('clientUserMessageId')=='claude-rejected'
            for method,p in server.calls if method=='turn/start'),2)

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

    def test_native_actions_follow_claude_rules(self):
        a=self.runtime.new_lead({'cwd':str(self.root),'account_key':'claude-local'})
        a=self.runtime.prepare(a)
        with self.assertRaisesRegex(ValueError,'only for Codex agents'):
            self.runtime.native_action(a['id'],'review')
        server=self.runtime.servers['claude-local']
        self.runtime.native_action(a['id'],'compact')
        methods=[m for m,_ in server.calls]
        self.assertIn('thread/compact/start',methods)
        # Claude applies settings with each turn and has no thread settings update.
        self.assertNotIn('thread/settings/update',methods)

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
        with self.runtime.lock:
            with self.runtime.db() as db:
                a=self.runtime.agent(a['id'],db)
                a.update(autoWake=True,status='queued',inFlight=False)
                self.runtime.put(db,'agents',a)
                self.runtime.enqueue(db,a,'user','/compact','command-message')
                self.runtime.enqueue(db,a,'user','Continue after compact','next-message')
            with patch.object(self.runtime.delivery_executor(),'submit') as submit:
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

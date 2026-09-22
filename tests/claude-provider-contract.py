#!/usr/bin/env python3
"""Claude account selection, identity and delivery boundaries. No model calls."""
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

class ClaudeProvider(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        self.env=patch.dict(os.environ,{'CODEX_HOME':str(self.root/'codex')});self.env.start();self.addCleanup(self.env.stop)
        self.auth=patch('codex_claude.auth_metadata',return_value=AUTH.copy());self.auth.start();self.addCleanup(self.auth.stop)
        self.installed=patch('codex_claude.installed',return_value='/fake/claude');self.installed.start();self.addCleanup(self.installed.stop)
        self.runtime=f.ControlledRuntime(self.root/'state',f.AccountServer);self.addCleanup(self.runtime.close)
        self.runtime.accounts.discover()

    def test_subscription_identity_and_provider_selection(self):
        account=self.runtime.accounts.get('claude-local')
        self.assertEqual(account['provider'],'claude');self.assertNotIn('_credentialIdentity',account)
        a=self.runtime.new_lead({'cwd':str(self.root)})
        a=self.runtime.set_account(a['id'],'claude-local')
        self.assertEqual(a['model'],'default');self.assertEqual(a['provider'],'claude')
        self.assertEqual(self.runtime.worker_defaults(a)['model'],'sonnet')
        a=self.runtime.set_account(a['id'],'default')
        self.assertEqual(a['model'],'gpt-6-astra');self.assertEqual(a['provider'],'codex')
        with patch('codex_claude.auth_metadata',return_value={**AUTH,'accountId':'claude:changed'}):
            self.assertEqual(self.runtime.accounts.get('claude-local')['status'],'changed')
            with self.assertRaisesRegex(ValueError,'changed'):self.runtime.connect('claude-local')

    def test_claude_defaults_queue_and_unavailable_capabilities(self):
        a=self.runtime.new_lead({'cwd':str(self.root),'account_key':'claude-local'})
        self.assertEqual(a['model'],'default')
        names={t['name'] for t in self.runtime.tool_definitions(a)}
        self.assertIn('orchestration_spawn',names);self.assertNotIn('orchestration_monitor',names)
        with self.runtime.lock, self.runtime.db() as db:
            a.update(inFlight=True,turnId='active-turn',status='running')
            self.runtime.put(db,'agents',a)
        result=self.runtime.send(a['id'],'hello',delivery='after_tool')
        self.assertEqual(result['status'],'queued')
        with self.assertRaisesRegex(ValueError,'queue'):self.runtime.send(a['id'],'now',delivery='steer')
        self.assertIn('does not expose',self.runtime.limits('claude-local')['error'])
        self.assertNotIn('claude-local',self.runtime.servers)

    def test_workers_keep_claude_account_and_model_defaults(self):
        a=self.runtime.new_lead({'cwd':str(self.root),'account_key':'claude-local'})
        catalog={'data':[{'model':'sonnet','supportedReasoningEfforts':[{'reasoningEffort':'max'}],
                         'defaultReasoningEffort':'medium','serviceTiers':[]}]}
        child=self.runtime.create({'name':'Worker','prompt':'Read the project'},parent=a['id'],defer=True,_catalog=catalog)
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

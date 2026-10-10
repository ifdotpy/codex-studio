#!/usr/bin/env python3
"""Validate fresh native account metadata before admitting a Claude input."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import time
import unittest

spec = importlib.util.spec_from_file_location('fresh_account_fixture', Path(__file__).with_name('claude-pre-admission-receipt-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
NORMAL = {'email': 'test@example.test', 'subscriptionType': 'Claude Max', 'apiProvider': 'firstParty', 'apiKeySource': 'none'}
SDK = f.SDK.replace('let abort=new AbortController();', '''let abort=new AbortController();
 const cachedAccount=fs.existsSync(options.cwd+'/.cached-account.json')
  ?JSON.parse(fs.readFileSync(options.cwd+'/.cached-account.json','utf8'))
  :{email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty',apiKeySource:'none'};''')
SDK = SDK.replace("export function query({prompt,options}){", r"""export function query({prompt,options}){
 fs.appendFileSync(options.cwd+'/.all-queries','query\n');""")
SDK = SDK.replace("return {email:fs.existsSync(options.cwd+'/.wrong-account')?'different@example.test':'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'};", 'return cachedAccount;')
SDK = SDK.replace('initializationResult:async()=>', r'''reinitialize:async()=>{
  fs.appendFileSync(options.cwd+'/.account-refreshes','fresh\n');
  if(fs.existsSync(options.cwd+'/.refresh-hang'))return new Promise(()=>{});
  if(fs.existsSync(options.cwd+'/.refresh-gate')){
   fs.writeFileSync(options.cwd+'/.refresh-entered','yes');
   while(!fs.existsSync(options.cwd+'/.refresh-release'))await new Promise(resolve=>setTimeout(resolve,5));
  }
  if(fs.existsSync(options.cwd+'/.refresh-throw'))throw new Error('The fixture account read failed');
  return {models:[{value:'haiku',displayName:'Haiku',resolvedModel:'claude-haiku-fixture'}],account:fs.existsSync(options.cwd+'/.fresh-account.json')?JSON.parse(fs.readFileSync(options.cwd+'/.fresh-account.json','utf8')):cachedAccount};
 },initializationResult:async()=>''')


class FreshAccount(f.PreAdmission):
    def setUp(self):
        if self._testMethodName == 'test_passive_read_and_background_refresh_never_spawn_a_query':
            self.catalog_refresh_ms = 40
        if self._testMethodName.startswith('test_expired_passive_'):
            self.catalog_ttl_ms = 100
        if self._testMethodName == 'test_expired_passive_refresh_timeout_preserves_the_live_turn':
            self.passive_timeout_ms = 40
        before = f.SDK
        f.SDK = (SDK.replace('reinitialize:async()=>', 'unavailableReinitialize:async()=>')
                 if self._testMethodName == 'test_sdk_without_fresh_control_keeps_not_applied' else SDK)
        try:
            super().setUp()
        finally:
            f.SDK = before

    def test_passive_read_and_background_refresh_never_spawn_a_query(self):
        with self.assertRaisesRegex(ValueError, 'Fresh Claude account proof'):
            self.call('account/read', {'passive': True})
        self.assertFalse((self.root / 'state' / '.all-queries').exists())
        self.call('model/list', {})
        self.assertEqual(self.call('account/read', {'passive': True})['account']['email'], NORMAL['email'])
        self.accounts({key: value for key, value in NORMAL.items() if key != 'email'}, NORMAL)
        self.turn('steer', 'held-input')
        self.wait_file(self.root / '.admitted-inputs')
        self.wait_file(self.root / '.account-refreshes')
        deadline = time.monotonic() + 2
        while len(self.refreshes()) < 2 and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertGreaterEqual(len(self.refreshes()), 2)
        self.assertEqual(len((self.root / '.all-queries').read_text().splitlines()), 1)
        self.assertEqual(len((self.root / 'state' / '.all-queries').read_text().splitlines()), 1)
        self.assertEqual(self.call('account/read', {'passive': True})['account']['email'], NORMAL['email'])
        self.assertEqual(self.call('model/list', {})['data'][0]['resolvedModel'], 'claude-haiku-fixture')
        self.assertEqual(len(self.admissions()), 1)
        (self.root / '.fresh-account.json').write_text(json.dumps({**NORMAL, 'email': 'foreign@example.test'}))
        self.call('account/read', {'passive': True})
        deadline = time.monotonic() + 2
        while len(self.refreshes()) < 3 and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertGreaterEqual(len(self.refreshes()), 3)
        with self.assertRaisesRegex(ValueError, 'Fresh Claude account proof'):
            self.call('account/read', {'passive': True})
        self.assertEqual(len((self.root / '.all-queries').read_text().splitlines()), 1)
        self.assertEqual(len(self.admissions()), 1)

    def accounts(self, cached, fresh):
        (self.root / '.cached-account.json').write_text(json.dumps(cached))
        (self.root / '.fresh-account.json').write_text(json.dumps(fresh))

    def test_expired_passive_read_refreshes_the_live_query_without_new_input(self):
        self.call('model/list', {})
        self.accounts(NORMAL, NORMAL)
        self.turn('steer', 'held-input')
        self.wait_file(self.root / '.admitted-inputs')
        time.sleep(.15)
        queries = (self.root / '.all-queries').read_text()
        probes = (self.root / 'state' / '.all-queries').read_text()
        refreshes = len(self.refreshes())
        proof = self.call('account/read', {'passive': True})
        self.assertEqual(proof['account']['email'], NORMAL['email'])
        self.assertGreater(len(self.refreshes()), refreshes)
        self.assertEqual((self.root / '.all-queries').read_text(), queries)
        self.assertEqual((self.root / 'state' / '.all-queries').read_text(), probes)
        self.assertEqual(len(self.admissions()), 1)

    def test_expired_passive_read_without_a_live_query_cannot_start_a_cli(self):
        self.call('model/list', {})
        probes = (self.root / 'state' / '.all-queries').read_text()
        time.sleep(.15)
        with self.assertRaisesRegex(ValueError, 'No live Claude query'):
            self.call('account/read', {'passive': True})
        self.assertEqual((self.root / 'state' / '.all-queries').read_text(), probes)
        self.assertFalse((self.root / '.all-queries').exists())
        self.assertEqual(self.admissions(), [])

    def test_expired_passive_refresh_timeout_preserves_the_live_turn(self):
        self.call('model/list', {})
        self.accounts(NORMAL, NORMAL)
        self.turn('steer', 'held-input')
        self.wait_file(self.root / '.admitted-inputs')
        time.sleep(.15)
        (self.root / '.refresh-hang').touch()
        probes = (self.root / 'state' / '.all-queries').read_text()
        self.sequence += 1
        request = self.sequence
        self.write({'id': request, 'method': 'account/read', 'params': {'passive': True}})
        while True:
            response = self.read()
            if response.get('id') == request:
                break
            self.notifications.append(response)
        error = response['error']['data']
        self.assertEqual(error['claudePreparationPhase'], 'catalog_account_reinitialize')
        self.assertGreaterEqual(error['claudePreparationElapsedMs'], self.passive_timeout_ms - 5)
        self.assertEqual(len((self.root / '.all-queries').read_text().splitlines()), 1)
        self.assertEqual((self.root / 'state' / '.all-queries').read_text(), probes)
        self.assertEqual(len(self.admissions()), 1)
        self.assertTrue(self.call('claude/state', {'threadId': self.thread})['turns'])

    def refreshes(self):
        p = self.root / '.account-refreshes'
        return p.read_text().splitlines() if p.exists() else []

    def test_missing_cached_email_uses_fresh_exact_identity_before_one_input(self):
        self.accounts({key: value for key, value in NORMAL.items() if key != 'email'}, NORMAL)
        turn = self.turn('fixture input', 'original-account-input')['turn']['id']
        completed = self.completed()
        self.assertEqual(completed['status'], 'completed')
        self.assertEqual(self.refreshes(), ['fresh'])
        self.assertEqual(len(self.admissions()), 1)
        self.assertEqual(self.turn('fixture input', 'original-account-input')['turn']['id'], turn)
        self.assertEqual(len(self.admissions()), 1)
        self.assertEqual(len((self.root / '.queries').read_text().splitlines()), 1)

    def test_ready_cached_metadata_does_not_reinitialize(self):
        self.accounts(NORMAL, {**NORMAL, 'email': 'a-different-account@example.test'})
        self.turn('fixture input', 'ready-input')
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertEqual(self.refreshes(), [])
        self.assertEqual(len(self.admissions()), 1)

    def test_fresh_foreign_missing_and_api_key_proofs_reject_all_original_inputs(self):
        cached = {key: value for key, value in NORMAL.items() if key != 'email'}
        for index, fresh in enumerate((None, cached, {**NORMAL, 'email': 'a-different-account@example.test'},
                                       {**NORMAL, 'apiProvider': 'bedrock'}, {**NORMAL, 'subscriptionType': ''},
                                       {**NORMAL, 'subscriptionType': 'Claude Pro'},
                                       {key: value for key, value in NORMAL.items() if key != 'apiKeySource'},
                                       {**NORMAL, 'apiKeySource': 'environment'})):
            with self.subTest(index=index):
                self.accounts(cached, fresh)
                self.turn('fixture input', 'rejected-fresh-' + str(index))
                completed = self.completed()
                self.assertEqual(completed['status'], 'failed')
                self.assertEqual(completed['startOutcome'], 'not_applied')
                self.assertEqual(completed['error']['data']['turnStartOutcome'], 'not_applied')
                self.assertEqual(completed['error']['data']['claudePreparationFailure'], 'account_validation')
                self.assertEqual(self.admissions(), [])
        self.assertEqual(len(self.refreshes()), 8)

    def test_known_foreign_identity_does_not_reinitialize(self):
        for index, cached in enumerate(({**NORMAL, 'email': 'a-different-account@example.test'},
                                        {'email': 'a-different-account@example.test'})):
            with self.subTest(index=index):
                self.accounts(cached, NORMAL)
                self.turn('fixture input', 'known-foreign-' + str(index))
                completed = self.completed()
                self.assertEqual(completed['status'], 'failed')
                self.assertEqual(completed['error']['data']['claudeAccountFailure'], 'identity_mismatch')
                self.assertEqual(self.refreshes(), [])
                self.assertEqual(self.admissions(), [])

    def test_sdk_without_fresh_control_keeps_not_applied(self):
        self.accounts({}, NORMAL)
        self.turn('fixture input', 'unsupported-fresh-control')
        completed = self.completed()
        self.assertEqual(completed['startOutcome'], 'not_applied')
        self.assertEqual(completed['error']['data']['claudePreparationFailure'], 'account_validation')
        self.assertEqual(self.refreshes(), [])
        self.assertEqual(self.admissions(), [])

    def test_known_foreign_provider_or_api_key_cannot_use_fresh_metadata(self):
        cached = {key: value for key, value in NORMAL.items() if key != 'email'}
        for index, account in enumerate(({**cached, 'apiProvider': 'bedrock'},
                                         {**cached, 'apiKeySource': 'environment'})):
            with self.subTest(index=index):
                self.accounts(account, NORMAL)
                self.turn('fixture input', 'known-foreign-provider-' + str(index))
                self.assertEqual(self.completed()['startOutcome'], 'not_applied')
                self.assertEqual(self.refreshes(), [])
                self.assertEqual(self.admissions(), [])

    def test_catalog_returns_verified_fresh_metadata_without_a_prompt(self):
        state = self.root / 'state'
        (state / '.cached-account.json').write_text(json.dumps({}))
        (state / '.fresh-account.json').write_text(json.dumps(NORMAL))
        account = self.call('account/read', {})['account']
        self.assertEqual(account['email'], NORMAL['email'])
        self.assertEqual(account['planType'], NORMAL['subscriptionType'])
        self.assertTrue(self.call('model/list', {})['data'])
        self.assertEqual((state / '.account-refreshes').read_text().splitlines(), ['fresh'])
        self.assertEqual(self.admissions(), [])

    def test_fresh_read_error_keeps_structural_unsubmitted_proof(self):
        self.accounts({}, NORMAL)
        (self.root / '.refresh-throw').touch()
        self.turn('fixture input', 'failed-fresh-read')
        completed = self.completed()
        self.assertEqual(completed['startOutcome'], 'not_applied')
        self.assertEqual(completed['error']['data']['claudePreparationFailure'], 'account_validation')
        self.assertEqual(self.admissions(), [])
        self.assertEqual(self.refreshes(), ['fresh'])

    def test_fresh_read_timeout_uses_the_same_deadline(self):
        self.accounts({}, NORMAL)
        (self.root / '.refresh-hang').touch()
        began = time.monotonic()
        self.turn('fixture input', 'fresh-read-timeout')
        self.assert_rejected(self.completed())
        self.assertLess(time.monotonic() - began, 1.8)
        self.assertEqual(self.refreshes(), ['fresh'])

    def test_stop_during_fresh_read_never_admits_late_valid_proof(self):
        self.accounts({}, NORMAL)
        (self.root / '.refresh-gate').touch()
        turn = self.turn('fixture input', 'stopped-fresh-read')['turn']['id']
        self.wait_file(self.root / '.refresh-entered')
        self.call('turn/interrupt', {'threadId': self.thread, 'turnId': turn})
        (self.root / '.refresh-release').touch()
        self.assertEqual(self.completed()['status'], 'interrupted')
        self.assertEqual(self.admissions(), [])
        self.assertEqual(self.refreshes(), ['fresh'])

    def test_resume_and_settings_during_fresh_read_keep_exact_query_settings(self):
        self.accounts({}, NORMAL)
        (self.root / '.refresh-gate').touch()
        self.turn('fixture input', 'exact-query-source')
        self.wait_file(self.root / '.refresh-entered')
        with self.assertRaisesRegex(ValueError, 'Pause Claude'):
            self.call('claude/settings', {'threadId': self.thread, 'settings': {'thinking': False}})
        resumed = self.call('thread/resume', {'threadId': self.thread, 'excludeTurns': True, 'model': 'sonnet'})
        self.assertTrue(resumed['reattached'])
        self.assertEqual(resumed['model'], 'default')
        (self.root / '.refresh-release').touch()
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertEqual(len(self.admissions()), 1)
        self.assertEqual(self.refreshes(), ['fresh'])


if __name__ == '__main__':
    suite = unittest.TestSuite(FreshAccount(name) for name in FreshAccount.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())

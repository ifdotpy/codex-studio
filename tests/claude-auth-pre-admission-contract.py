#!/usr/bin/env python3
"""Keep Claude account rejection separate from accepted input."""
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    'claude_admission_fixture', Path(__file__).with_name('claude-pre-admission-receipt-contract.py'))
admission = importlib.util.module_from_spec(spec)
spec.loader.exec_module(admission)
fixture = admission.fixture

SDK = admission.SDK.replace(
    "return {email:fs.existsSync(options.cwd+'/.wrong-account')?'different@example.test':'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'};",
    """if(fs.existsSync(options.cwd+'/.auth-account.json'))return JSON.parse(fs.readFileSync(options.cwd+'/.auth-account.json','utf8'));
     return {email:fs.existsSync(options.cwd+'/.wrong-account')?'different@example.test':'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'};""")
SDK = SDK.replace(
    "let text=input.value.message.content[0].text;",
    """let text=input.value.message.content[0].text;
     if(text==='post-admission-auth-error')throw new Error('Claude Code account or subscription changed. Restore the original login.');
     if(text==='post-admission-structured-account-error')throw Object.assign(new Error('Account validation after input'),{claudeAccountValidationFailed:true,data:{turnStartOutcome:'not_applied',claudePreparationFailure:'account_validation',claudeAccountFailure:'identity_mismatch'}});""")


class AccountAdmission(admission.PreAdmission):
    def setUp(self):
        self.preparation_timeout_ms = 1000
        original = admission.SDK
        admission.SDK = SDK
        try:
            super().setUp()
        finally:
            admission.SDK = original

    def set_account(self, account):
        (self.root / '.auth-account.json').write_text(json.dumps(account))

    def account_rejection(self, key, reason, message):
        turn = self.turn('fixture input', key)['turn']['id']
        completed = self.completed()
        self.assertEqual(completed['status'], 'failed')
        self.assertEqual(completed['error'].get('data'), {
            'turnStartOutcome': 'not_applied',
            'claudePreparationFailure': 'account_validation',
            'claudeAccountFailure': reason,
        })
        self.assertIn(message, completed['error']['message'])
        self.assertEqual(completed.get('startOutcome'), 'not_applied')
        saved = self.history()[0]
        self.assertEqual(saved['id'], turn)
        self.assertEqual(saved['clientUserMessageId'], key)
        self.assertEqual(saved['startOutcome'], 'not_applied')
        self.assertEqual(saved['error'], completed['error'])
        self.assertEqual(self.admissions(), [])
        self.assertFalse(any(row.get('method') == 'item/agentMessage/delta'
                             for row in self.notifications))
        return turn

    def test_foreign_identity_rejects_before_admission_then_exact_retry_once(self):
        gate = self.root / '.wrong-account'
        gate.touch()
        old = self.account_rejection('same-input', 'identity_mismatch',
                                     'expected test@example.test, got different@example.test')
        self.assertEqual(len((self.root / '.queries').read_text().splitlines()), 1)
        self.restart_bridge()
        self.assertEqual(self.history()[0]['startOutcome'], 'not_applied')
        self.assertEqual(self.admissions(), [])
        gate.unlink()
        new = self.turn('fixture input', 'same-input')['turn']['id']
        self.assertNotEqual(new, old)
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertEqual(len(self.admissions()), 1)
        self.assertEqual(self.turn('fixture input', 'same-input')['turn']['id'], new)
        self.assertEqual(len(self.admissions()), 1)

    def test_missing_metadata_does_not_claim_the_identity_changed(self):
        normal = {'email': 'test@example.test', 'apiProvider': 'firstParty',
                  'subscriptionType': 'Claude Max'}
        cases = [(None, 'account_metadata', 'account metadata is missing'),
                 ({k: v for k, v in normal.items() if k != 'email'},
                  'email_metadata', 'email metadata is missing'),
                 ({k: v for k, v in normal.items() if k != 'apiProvider'},
                  'provider_metadata', 'API provider metadata is missing'),
                 ({k: v for k, v in normal.items() if k != 'subscriptionType'},
                  'subscription_metadata', 'subscription metadata is missing'),
                 ({**normal, 'subscriptionType': False},
                  'subscription_metadata', 'subscription metadata is missing')]
        for index, (account, reason, message) in enumerate(cases):
            with self.subTest(reason=reason, index=index):
                self.set_account(account)
                self.account_rejection('metadata-' + str(index), reason, message)
                self.assertNotIn('account or subscription changed', self.history()[0]['error']['message'])

    def test_foreign_provider_and_api_key_stay_blocked(self):
        normal = {'email': 'test@example.test', 'apiProvider': 'firstParty',
                  'subscriptionType': 'Claude Max'}
        for index, (account, reason, message) in enumerate([
                ({**normal, 'apiProvider': 'bedrock'}, 'provider_mismatch', 'API provider is bedrock'),
                ({**normal, 'apiKeySource': 'environment'}, 'api_key_source', 'uses an API key')]):
            with self.subTest(reason=reason):
                self.set_account(account)
                self.account_rejection('provider-' + str(index), reason, message)

    def test_matching_subscription_identity_accepts_once(self):
        self.set_account({'email': 'test@example.test', 'apiProvider': 'firstParty',
                          'subscriptionType': 'Claude Max', 'apiKeySource': 'none'})
        turn = self.turn('fixture input', 'allowed')['turn']['id']
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertEqual(self.history()[0]['startOutcome'], 'accepted')
        self.assertEqual(len(self.admissions()), 1)
        self.assertEqual(self.turn('fixture input', 'allowed')['turn']['id'], turn)
        self.assertEqual(len(self.admissions()), 1)

    def test_account_rejection_survives_accepted_receipt_persist_overlap(self):
        (self.root / '.wrong-account').touch()
        (self.root / 'state' / 'gate-accepted').touch()
        self.sequence += 1
        request = self.sequence
        self.write({'id': request, 'method': 'turn/start', 'params': {
            'threadId': self.thread, 'clientUserMessageId': 'account-overlap',
            'input': [{'type': 'text', 'text': 'fixture input'}]}})
        self.wait_file(self.root / 'state' / 'held-accepted')
        self.assertEqual(self.admissions(), [])
        (self.root / 'state' / 'release-accepted').touch()
        reply = self.persist_reply(request)
        self.assertIn('result', reply)
        completed = self.completed()
        self.assertEqual(completed.get('startOutcome'), 'not_applied')
        self.assertEqual(completed['error']['data']['claudePreparationFailure'],
                         'account_validation')
        self.restart_bridge()
        self.assertEqual(self.history()[0]['startOutcome'], 'not_applied')
        self.assertEqual(self.admissions(), [])
        self.assertEqual(len((self.root / '.queries').read_text().splitlines()), 1)

    def test_post_admission_raw_and_structured_auth_errors_preserve_acceptance(self):
        for text in ('post-admission-auth-error', 'post-admission-structured-account-error'):
            with self.subTest(text=text):
                turn = self.turn(text, text)['turn']['id']
                completed = self.completed()
                self.assertEqual(completed['status'], 'failed')
                self.assertNotIn('data', completed['error'])
                self.assertEqual(self.history()[0]['startOutcome'], 'accepted')
                count = len(self.admissions())
                self.assertEqual(self.turn(text, text)['turn']['id'], turn)
                self.assertEqual(len(self.admissions()), count)

    def test_bridge_version_identifies_the_account_receipt_guard(self):
        self.assertEqual(self.call('initialize', {})['capabilities']['claudeVersion'], 16)


def load_tests(loader, tests, pattern):
    return unittest.TestSuite(AccountAdmission(name) for name in AccountAdmission.__dict__
                              if name.startswith('test_'))


if __name__ == '__main__':
    unittest.main()

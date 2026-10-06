#!/usr/bin/env python3
"""Prove fresh Claude input rejection before SDK prompt admission."""
import importlib.util
from pathlib import Path
import time
import unittest

spec = importlib.util.spec_from_file_location(
    'claude_bridge_fixture', Path(__file__).with_name('claude-bridge-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

SDK = fixture.SDK.replace(
    'if(input.done)return;',
    "if(input.done)return;fs.appendFileSync(options.cwd+'/.admitted-inputs',input.value.uuid+'\\n');")
SDK = SDK.replace(
    "accountInfo:async()=>{if(fs.existsSync(options.cwd+'/.hang-account'))",
    """accountInfo:async()=>{
     if(fs.existsSync(options.cwd+'/.account-timeout-text'))throw new Error('Claude preparation timed out before input was submitted');
     if(options.systemPrompt&&fs.existsSync(options.cwd+'/.slow-account'))await new Promise(resolve=>setTimeout(resolve,1200));
     if(fs.existsSync(options.cwd+'/.late-account')){
      fs.writeFileSync(options.cwd+'/.account-entered','yes');
      await new Promise(resolve=>setTimeout(resolve,1800));
      fs.writeFileSync(options.cwd+'/.account-finished','yes');
     }
     if(fs.existsSync(options.cwd+'/.hang-account'))""")
SDK = SDK.replace(
    "let text=input.value.message.content[0].text;",
    """let text=input.value.message.content[0].text;
     if(text==='post-admission-timeout')throw Object.assign(new Error('Claude preparation timed out before input was submitted'),{preparationTimedOut:true,data:{turnStartOutcome:'not_applied'}});""")
SDK = SDK.replace(
    'const value=await appendOriginal(file,data,...args);',
    """const value=await appendOriginal(file,data,...args);
     if(fs.existsSync(root+'/gate-accepted')&&String(data).includes('accepted')&&!fs.existsSync(root+'/held-accepted')){
      fs.writeFileSync(root+'/held-accepted','yes');
      while(!fs.existsSync(root+'/release-accepted'))await new Promise(resolve=>setTimeout(resolve,5));
     }""")


class PreAdmission(fixture.Bridge):
    def setUp(self):
        self.preparation_timeout_ms = 1000
        if self._testMethodName == 'test_cold_initialization_can_exceed_control_budget_and_admit_once':
            self.initialization_timeout_ms = 2000
        original = fixture.SDK
        fixture.SDK = SDK
        try:
            super().setUp()
        finally:
            fixture.SDK = original

    def history(self):
        return self.call('thread/turns/list', {'threadId': self.thread})['data']

    def admissions(self):
        path = self.root / '.admitted-inputs'
        return path.read_text().splitlines() if path.exists() else []

    def wait_file(self, path, timeout=3):
        deadline = time.monotonic() + timeout
        while not path.exists() and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertTrue(path.exists(), path.name)

    def assert_rejected(self, completed, status='failed'):
        self.assertEqual(completed['status'], status)
        self.assertEqual(completed['error'].get('data'), {'turnStartOutcome': 'not_applied'})
        self.assertEqual(completed.get('startOutcome'), 'not_applied')
        saved = self.history()[0]
        self.assertEqual(saved['id'], completed['id'])
        self.assertEqual(saved['startOutcome'], 'not_applied')
        self.assertEqual(saved['error']['data'], {'turnStartOutcome': 'not_applied'})
        self.assertEqual(self.admissions(), [])
        return saved

    def test_preparation_fresh_timeout_saves_rejection_and_exact_retry_once(self):
        gate = self.root / '.hang-account'
        gate.touch()
        original = self.turn('fixture input', 'fresh-timeout')['turn']['id']
        saved = self.assert_rejected(self.completed())
        self.assertEqual(saved['id'], original)
        self.assertEqual(saved['clientUserMessageId'], 'fresh-timeout')
        self.assertEqual(len((self.root / '.queries').read_text().splitlines()), 1)
        gate.unlink()
        self.restart_bridge()
        self.assertEqual(self.history()[0]['startOutcome'], 'not_applied')
        new = self.turn('fixture input', 'fresh-timeout')['turn']['id']
        self.assertNotEqual(new, original)
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertEqual(len(self.admissions()), 1)
        self.assertEqual(self.turn('fixture input', 'fresh-timeout')['turn']['id'], new)
        self.assertEqual(len(self.admissions()), 1)
        self.assertEqual(len(self.history()), 2)

    def test_cold_initialization_can_exceed_control_budget_and_admit_once(self):
        (self.root / '.slow-account').touch()
        started = time.monotonic()
        turn = self.turn('fixture input', 'slow-cold-start')['turn']['id']
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertGreater(time.monotonic() - started, 1)
        self.assertEqual(self.history()[0]['startOutcome'], 'accepted')
        self.assertEqual(len(self.admissions()), 1)
        self.assertEqual(self.turn('fixture input', 'slow-cold-start')['turn']['id'], turn)
        self.assertEqual(len(self.admissions()), 1)
        (self.root / '.slow-account').unlink()
        (self.root / '.hang-preparation').touch()
        started = time.monotonic()
        with self.assertRaisesRegex(ValueError, 'before input was submitted'):
            self.turn('second input', 'blocked-control')
        self.assertLess(time.monotonic() - started, 1.8)
        self.assertEqual(len(self.admissions()), 1)

    def test_preparation_late_account_result_cannot_admit_rejected_input(self):
        (self.root / '.late-account').touch()
        self.turn('fixture input', 'late-account')
        self.wait_file(self.root / '.account-entered')
        self.assert_rejected(self.completed())
        self.wait_file(self.root / '.account-finished')
        self.assertEqual(self.admissions(), [])
        self.assertEqual(self.history()[0]['startOutcome'], 'not_applied')
        self.assertEqual(len((self.root / '.queries').read_text().splitlines()), 1)

    def test_preparation_stop_does_not_admit_or_restart_input(self):
        (self.root / '.hang-account').touch()
        turn = self.turn('fixture input', 'stop-before-admission')['turn']['id']
        self.call('turn/interrupt', {'threadId': self.thread, 'turnId': turn})
        self.assert_rejected(self.completed(), status='interrupted')
        self.assertEqual(len((self.root / '.queries').read_text().splitlines()), 1)
        self.assertEqual(self.call('thread/read', {'threadId': self.thread})
                         ['thread']['status']['type'], 'idle')

    def test_preparation_post_admission_structured_error_preserves_acceptance(self):
        turn = self.turn('post-admission-timeout', 'already-admitted')['turn']['id']
        completed = self.completed()
        self.assertEqual(completed['status'], 'failed')
        self.assertNotIn('data', completed['error'])
        self.assertEqual(self.history()[0]['startOutcome'], 'accepted')
        self.assertEqual(len(self.admissions()), 1)
        self.restart_bridge()
        self.assertEqual(self.turn('post-admission-timeout', 'already-admitted')['turn']['id'], turn)
        self.assertEqual(len(self.admissions()), 1)

    def test_preparation_timeout_text_cannot_prove_rejection(self):
        (self.root / '.account-timeout-text').touch()
        turn = self.turn('fixture input', 'text-only')['turn']['id']
        completed = self.completed()
        self.assertEqual(completed['status'], 'failed')
        self.assertNotIn('data', completed['error'])
        self.assertEqual(self.history()[0]['startOutcome'], 'accepted')
        self.assertEqual(self.admissions(), [])
        self.assertEqual(self.turn('fixture input', 'text-only')['turn']['id'], turn)
        self.assertEqual(len((self.root / '.queries').read_text().splitlines()), 1)

    def test_preparation_rejection_survives_accepted_persist_overlap(self):
        (self.root / '.hang-account').touch()
        (self.root / 'state' / 'gate-accepted').touch()
        self.sequence += 1
        request = self.sequence
        self.write({'id': request, 'method': 'turn/start', 'params': {
            'threadId': self.thread, 'clientUserMessageId': 'overlap',
            'input': [{'type': 'text', 'text': 'fixture input'}]}})
        self.wait_file(self.root / 'state' / 'held-accepted')
        self.wait_file(self.root / '.query-closes')
        self.assertEqual(self.admissions(), [])
        (self.root / 'state' / 'release-accepted').touch()
        reply = self.persist_reply(request)
        self.assertIn('result', reply)
        self.assert_rejected(self.completed())
        self.restart_bridge()
        self.assertEqual(self.history()[0]['startOutcome'], 'not_applied')
        self.assertEqual(self.admissions(), [])


def load_tests(loader, tests, pattern):
    return unittest.TestSuite(PreAdmission(name) for name in PreAdmission.__dict__
                              if name.startswith('test_'))


if __name__ == '__main__':
    unittest.main()

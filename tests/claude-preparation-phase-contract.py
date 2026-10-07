#!/usr/bin/env python3
"""Name the blocked Claude preparation phase without admitting input."""
import importlib.util
import json
from pathlib import Path
import time
import unittest

spec = importlib.util.spec_from_file_location(
    'claude_phase_fixture', Path(__file__).with_name('claude-bridge-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

SDK = fixture.SDK.replace(
    ' let abort=new AbortController();',
    " if(!options.systemPrompt)fs.appendFileSync(process.argv[2]+'/.catalog-queries','query\\n');\n let abort=new AbortController();")
SDK = SDK.replace(
    'accountInfo:async()=>{',
    """accountInfo:async()=>{
   if(options.systemPrompt&&fs.existsSync(options.cwd+'/.hang-session-account'))await new Promise(()=>{});
   if(options.systemPrompt&&fs.existsSync(options.cwd+'/.missing-session-email'))return {subscriptionType:'Claude Max',apiProvider:'firstParty'};
   if(!options.systemPrompt&&fs.existsSync(process.argv[2]+'/.hang-catalog'))await new Promise(()=>{});
   if(!options.systemPrompt&&fs.existsSync(process.argv[2]+'/.slow-catalog'))await new Promise(resolve=>setTimeout(resolve,250));""")
SDK = SDK.replace(
    '  initializationResult:',
    """  reinitialize:async()=>{await new Promise(()=>{});},
  initializationResult:""")
SDK = SDK.replace(
    'applyFlagSettings:async settings=>{',
    """applyFlagSettings:async settings=>{
   if(fs.existsSync(options.cwd+'/.hang-flag-settings'))await new Promise(()=>{});""")
SDK = SDK.replace('if(input.done)return;',
                  "if(input.done)return;fs.appendFileSync(options.cwd+'/.admitted-inputs',input.value.uuid+'\\n');")


class PreparationPhase(fixture.Bridge):
    def setUp(self):
        self.preparation_timeout_ms = 300
        self.initialization_timeout_ms = 700
        self.catalog_refresh_ms = 500
        original = fixture.SDK
        fixture.SDK = SDK
        try:
            super().setUp()
        finally:
            fixture.SDK = original

    def admissions(self):
        path = self.root / '.admitted-inputs'
        return path.read_text().splitlines() if path.exists() else []

    def assert_timeout(self, data, phase, minimum):
        self.assertEqual(data['turnStartOutcome'], 'not_applied')
        if phase == 'catalog_flags':
            self.assertIn(data['claudePreparationPhase'], {'catalog_flags', 'catalog_account_initialize'})
        else:
            self.assertEqual(data['claudePreparationPhase'], phase)
        self.assertGreaterEqual(data['claudePreparationElapsedMs'], minimum)
        self.assertLess(data['claudePreparationElapsedMs'], 2000)

    def cold_timeout(self, marker, phase):
        ((self.root / 'state' if marker == '.hang-catalog' else self.root) / marker).touch()
        turn = self.turn('fixture input', phase)['turn']['id']
        completed = self.completed()
        self.assertEqual(completed['id'], turn)
        self.assertEqual(completed['status'], 'failed')
        self.assertEqual(completed['clientUserMessageId'], phase)
        self.assert_timeout(completed['error']['data'], phase, 600)
        saved = self.call('thread/turns/list', {'threadId': self.thread})['data'][0]
        self.assertEqual(saved['error']['data'], completed['error']['data'])
        self.assertEqual(self.admissions(), [])

    def test_cold_catalog_timeout_prevents_query_and_input(self):
        self.cold_timeout('.hang-catalog', 'catalog_flags')
        self.assertFalse((self.root / '.queries').exists())

    def test_session_initialize_timeout_keeps_constructor_thinking_flags(self):
        self.call('claude/settings', {'threadId': self.thread, 'settings': {'thinking': False}})
        self.cold_timeout('.hang-session-account', 'account_initialize')
        flags = [json.loads(line) for line in (self.root / '.thinking-flags').read_text().splitlines()]
        self.assertEqual(len(flags), 1)
        self.assertEqual(flags[0]['phase'], 'initial')
        self.assertTrue(flags[0]['settings']['alwaysThinkingEnabled'])

    def test_account_refresh_timeout_has_its_own_phase(self):
        self.cold_timeout('.missing-session-email', 'account_reinitialize')

    def test_catalog_and_session_share_the_original_deadline(self):
        (self.root / 'state' / '.slow-catalog').touch()
        started = time.monotonic()
        self.cold_timeout('.hang-session-account', 'account_initialize')
        self.assertLess(time.monotonic() - started, .95)

    def retained_timeout(self, marker, phase):
        self.turn('hello', 'original')
        self.assertEqual(self.completed()['status'], 'completed')
        (self.root / marker).touch()
        self.sequence += 1
        request = self.sequence
        self.write({'id': request, 'method': 'turn/start', 'params': {
            'threadId': self.thread, 'clientUserMessageId': 'not-admitted',
            'input': [{'type': 'text', 'text': 'second'}]}})
        while True:
            row = self.read()
            if row.get('id') == request:
                break
            self.notifications.append(row)
        self.assert_timeout(row['error']['data'], phase, 250)
        saved = self.call('thread/turns/list', {'threadId': self.thread})['data']
        self.assertEqual([turn['clientUserMessageId'] for turn in saved], ['original'])
        self.assertEqual(len(self.admissions()), 1)

    def test_retained_model_timeout_has_its_own_phase(self):
        self.retained_timeout('.hang-preparation', 'model_set')

    def test_retained_flags_timeout_has_its_own_phase(self):
        self.retained_timeout('.hang-flag-settings', 'flag_settings')

    def test_metadata_account_timeout_does_not_hide_native_failure(self):
        (self.root / '.hang-account').touch()
        self.sequence += 1
        request = self.sequence
        self.write({'id': request, 'method': 'account/rateLimits/read', 'params': {'cwd': str(self.root)}})
        while True:
            row = self.read()
            if row.get('id') == request:
                break
            self.notifications.append(row)
        self.assert_timeout(row['error']['data'], 'metadata_account_initialize', 600)
        self.assertEqual(self.admissions(), [])

    def test_active_turn_refreshes_original_catalog_without_another_turn(self):
        self.turn('wait', 'active-native-work')
        path = self.root / 'state' / '.catalog-queries'
        deadline = time.monotonic() + 2
        while (not path.exists() or len(path.read_text().splitlines()) < 2) and time.monotonic() < deadline:
            time.sleep(.005)
        self.assertEqual(len(path.read_text().splitlines()), 2)
        self.assertTrue(self.call('model/list', {})['data'])
        self.assertEqual(len((self.root / '.queries').read_text().splitlines()), 1)
        flags = [json.loads(line) for line in (self.root / '.thinking-flags').read_text().splitlines()]
        self.assertEqual([row['phase'] for row in flags], ['initial'])

    def test_idle_retained_query_does_not_refresh_catalog(self):
        self.turn('hello', 'idle-native-query')
        self.assertEqual(self.completed()['status'], 'completed')
        time.sleep(.7)
        path = self.root / 'state' / '.catalog-queries'
        self.assertEqual(len(path.read_text().splitlines()), 1)


for name in tuple(vars(fixture.Bridge)):
    if name.startswith('test_') and name not in vars(PreparationPhase):
        setattr(PreparationPhase, name, None)

if __name__ == '__main__':
    unittest.main()

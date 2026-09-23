#!/usr/bin/env python3
"""Daybreak settings, native parameters and account boundaries without model calls."""
import copy
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))


def fixture(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


f = fixture('runtime-contract')
transfer = fixture('account-transfer-contract')
safety = fixture('native-safety-contract')


def access(catalog, programs=('standard', 'daybreakBlue')):
    result = copy.deepcopy(catalog)
    for row in result['data']:
        row['availableAccessPrograms'] = {'cyber': list(programs)}
    return result


class DaybreakServer(f.FakeServer):
    def call(self, method, params, timeout=60):
        result = super().call(method, params, timeout)
        return access(result) if method == 'model/list' else result


class ControlledRuntime(f.Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.05)
            self.changed.clear()


class DaybreakResolution(unittest.TestCase):
    def resolve(self, programs, enabled=True, provider='codex'):
        from codex_daybreak import resolve_program
        row = {'model': 'model'}
        if programs is not None:
            row['availableAccessPrograms'] = {'cyber': programs}
        return resolve_program({'data': [row]}, 'model', enabled, provider=provider)

    def test_blue_and_red_are_model_access_programs(self):
        self.assertEqual(self.resolve(['standard', 'daybreakBlue']), 'daybreakBlue')
        self.assertEqual(self.resolve(['standard', 'daybreakRed']), 'daybreakRed')
        self.assertEqual(self.resolve(['standard', 'daybreakBlue'], False), 'standard')

    def test_enabled_requires_explicit_supported_metadata(self):
        for programs in [None, [], ['standard'], ['futureProgram']]:
            with self.subTest(programs=programs), self.assertRaises(ValueError):
                self.resolve(programs)

    def test_malformed_program_metadata_cannot_grant_daybreak(self):
        for programs in ['daybreakBlue', 'standard daybreakBlue', {'daybreakBlue': True},
                         ['standard', 'daybreakBlue', 1], 1]:
            with self.subTest(programs=programs), self.assertRaises(ValueError):
                self.resolve(programs)

    def test_disabled_accepts_missing_metadata_but_rejects_explicit_no_standard(self):
        self.assertEqual(self.resolve(None, False), 'standard')
        for programs in [[], ['daybreakBlue'], ['daybreakRed']]:
            with self.subTest(programs=programs), self.assertRaises(ValueError):
                self.resolve(programs, False)

    def test_enabled_rejects_claude_and_non_boolean_values(self):
        with self.assertRaises(ValueError):
            self.resolve(['standard', 'daybreakBlue'], True, 'claude')
        for enabled in [None, 0, 1, 'true', [], {}]:
            with self.subTest(enabled=enabled), self.assertRaises(ValueError):
                self.resolve(['standard', 'daybreakBlue'], enabled)

    def test_claude_native_params_never_include_codex_access_program(self):
        from codex_daybreak import turn_params
        self.assertEqual(turn_params({'provider': 'claude', 'daybreakEnabled': False}), {})
        self.assertEqual(turn_params({'provider': 'codex', 'daybreakEnabled': False}),
                         {'cyberAccessProgram': 'standard'})


class DaybreakRuntime(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='daybreak-contract-')
        self.root = Path(self.tmp.name)
        self.runtime = ControlledRuntime(self.root, DaybreakServer)
        self.server = self.runtime.connect()
        self.a = self.runtime.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': ''}, draft=True)
        self.key = self.a['id']

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def agent(self):
        return self.runtime.agent(self.key)

    def settings(self, **changes):
        return self.runtime.conversation_settings(self.key, changes)

    def starts(self):
        return [params for method, params in self.server.calls if method == 'turn/start']

    def start(self, text='Task', identity='message-one'):
        self.runtime.send(self.key, text, identity)
        self.runtime.dispatch()
        f.eventually(lambda: self.agent().get('turnId'))
        return self.agent()

    def mutate(self, **values):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.agent()
            a.update(values)
            self.runtime.put(db, 'agents', a)

    def fail_capacity(self):
        a = self.agent()
        self.server.notify({'method': 'turn/completed', 'params': {'threadId': a['threadId'],
            'turn': {'id': a['turnId'], 'status': 'failed',
                     'error': {'message': 'At capacity', 'codexErrorInfo': 'serverOverloaded'}}}})
        return self.agent()['capacityRetry']

    def test_normal_turn_explicitly_emits_standard_then_daybreak(self):
        a = self.start()
        self.assertEqual(self.starts()[0]['cyberAccessProgram'], 'standard')
        self.server.complete(a['threadId'], a['turnId'])
        result = self.settings(daybreak_enabled=True)
        self.assertTrue(result['daybreakEnabled'])
        self.assertEqual(result['cyberAccessProgram'], 'daybreakBlue')
        self.start('Next task', 'message-two')
        self.assertEqual(self.starts()[-1]['cyberAccessProgram'], 'daybreakBlue')
        self.assertEqual(self.starts()[0]['model'], self.starts()[-1]['model'])

    def test_queued_mode_does_not_change_active_turn(self):
        a = self.start()
        first = copy.deepcopy(self.starts()[0])
        queued = self.settings(daybreak_enabled=True, next_turn=True, request_id='enable-next')
        self.assertFalse(queued.get('daybreakEnabled', False))
        self.assertTrue(queued['pendingSettings']['daybreakEnabled'])
        self.assertEqual(queued['pendingSettings']['cyberAccessProgram'], 'daybreakBlue')
        self.assertEqual(queued['turnId'], a['turnId'])
        self.assertEqual(self.starts(), [first])
        self.server.complete(a['threadId'], a['turnId'])
        self.start('Next task', 'message-two')
        self.assertEqual(self.starts()[-1]['cyberAccessProgram'], 'daybreakBlue')
        self.assertNotIn('pendingSettings', self.agent())

    def test_queue_receipt_is_exact_and_duplicate_keeps_newer_choice(self):
        request = dict(daybreak_enabled=True, next_turn=True, request_id='same-request')
        self.settings(**request)
        self.settings(daybreak_enabled=False, next_turn=True, request_id='newer-request')
        self.settings(**request)
        self.assertFalse(self.agent()['pendingSettings']['daybreakEnabled'])
        with self.assertRaisesRegex(ValueError, 'different'):
            self.settings(**{**request, 'daybreak_enabled': False})

    def test_stale_account_rejects_mode_before_catalog_and_before_old_receipt(self):
        request = dict(daybreak_enabled=True, next_turn=True, request_id='same-request', expected_account_key='default')
        self.settings(**request)
        self.mutate(accountKey='destination')
        before = self.agent()
        with patch.object(self.runtime, 'catalog', side_effect=AssertionError('Stale account read catalog')):
            for values in [request, dict(daybreak_enabled=True, expected_account_key='default')]:
                with self.subTest(values=values), self.assertRaisesRegex(ValueError, 'account changed'):
                    self.settings(**values)
        self.assertEqual(self.agent(), before)

    def test_bad_boolean_or_unavailable_mode_never_mutates_settings(self):
        before = self.agent()
        for value in [None, 0, 1, 'true']:
            with self.subTest(value=value), self.assertRaises(ValueError):
                self.settings(daybreak_enabled=value)
        catalog = access(self.runtime.catalog(), ['standard'])
        with patch.object(self.runtime, 'catalog', return_value=catalog):
            with self.assertRaises(ValueError):
                self.settings(daybreak_enabled=True)
            with self.assertRaises(ValueError):
                self.settings(daybreak_enabled=True, next_turn=True, request_id='unavailable')
        self.assertEqual(self.agent(), before)

    def test_enabled_turn_rechecks_current_model_access(self):
        self.settings(daybreak_enabled=True)
        catalog = access(self.runtime.catalog(), ['standard'])
        with patch.object(self.runtime, 'catalog', return_value=catalog):
            self.runtime.send(self.key, 'Task', 'revoked-access')
            self.runtime.dispatch()
            f.eventually(lambda: self.agent()['status'] == 'failed')
        self.assertEqual(self.starts(), [])
        self.assertTrue(self.agent()['daybreakEnabled'])

    def test_model_change_requires_matching_mode_and_preserves_original_on_failure(self):
        self.settings(daybreak_enabled=True)
        catalog = self.runtime.catalog()
        for row in catalog['data']:
            if row['model'] == 'gpt-5.6-sol':
                row['availableAccessPrograms'] = {'cyber': ['standard']}
        before = self.agent()
        with patch.object(self.runtime, 'catalog', return_value=catalog):
            with self.assertRaises(ValueError):
                self.settings(model='gpt-5.6-sol')
            self.assertEqual(self.agent(), before)
            updated = self.settings(model='gpt-5.6-sol', daybreak_enabled=False)
        self.assertEqual(updated['model'], 'gpt-5.6-sol')
        self.assertFalse(updated['daybreakEnabled'])

    def test_native_review_rejects_queued_daybreak_without_consuming_settings(self):
        a = self.start()
        self.server.complete(a['threadId'], a['turnId'])
        self.settings(daybreak_enabled=True, next_turn=True, request_id='review-mode')
        before = self.agent()
        with self.assertRaisesRegex(ValueError, 'review cannot select Daybreak'):
            self.runtime.native_action(self.key, 'review', 'native-review')
        self.assertEqual(self.agent(), before)
        self.assertFalse(any(method == 'review/start' for method, _ in self.server.calls))

    def test_agent_review_rejects_daybreak_without_creating_worker(self):
        from codex_agent_review import request
        self.settings(daybreak_enabled=True)
        a = self.start()
        with self.runtime.db() as db:
            before = {record['id'] for record in self.runtime.records(db, 'agents')}
        with self.assertRaisesRegex(ValueError, 'review cannot select Daybreak'):
            request(self.runtime, a, {}, a['threadId'] + ':review-daybreak')
        with self.runtime.db() as db:
            self.assertEqual({record['id'] for record in self.runtime.records(db, 'agents')}, before)
        current = self.agent()
        for field in ('threadId', 'turnId', 'model', 'daybreakEnabled', 'cyberAccessProgram'):
            self.assertEqual(current[field], a[field], field)

    def test_rollout_usage_keeps_exact_live_turn_program_after_model_change(self):
        a = {**self.agent(), 'threadId': 'analytics-thread', 'turnId': 'original-turn',
             'daybreakEnabled': True, 'cyberAccessProgram': 'daybreakBlue'}
        params = {'threadId': a['threadId'], 'turnId': a['turnId']}
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.analytics_event(db, a, 'turn/started', params, at=100)
            changed = {**a, 'model': 'gpt-5.6-sol', 'daybreakEnabled': False,
                       'cyberAccessProgram': 'standard', 'turnId': 'new-turn'}
            self.runtime.analytics_event(db, changed, 'thread/tokenUsage/updated', {
                **params, 'responseId': 'old-response', 'rawTokenUsageRecord': {'response_id': 'old-response'},
                'tokenUsage': {'total': {'totalTokens': 100}, 'last': {'inputTokens': 80, 'outputTokens': 20}}},
                at=101, source='rollout')
            row = db.execute('SELECT record FROM analytics_usage WHERE turn=?', ('original-turn',)).fetchone()
        record = json.loads(row[0])
        self.assertTrue(record['daybreakEnabled'])
        self.assertEqual(record['cyberAccessProgram'], 'daybreakBlue')

    def test_unknown_rollout_turn_does_not_inherit_current_daybreak_mode(self):
        a = {**self.agent(), 'threadId': 'analytics-thread', 'turnId': 'live-turn',
             'daybreakEnabled': True, 'cyberAccessProgram': 'daybreakBlue'}
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.analytics_event(db, a, 'turn/started', {'turnId': a['turnId']}, at=100)
            self.runtime.analytics_event(db, a, 'thread/tokenUsage/updated', {
                'threadId': a['threadId'], 'turnId': 'unknown-old-turn', 'responseId': 'old-response',
                'rawTokenUsageRecord': {'response_id': 'old-response'},
                'tokenUsage': {'total': {'totalTokens': 100}, 'last': {'inputTokens': 80, 'outputTokens': 20}}},
                at=99, source='rollout')
            row = db.execute('SELECT record FROM analytics_usage WHERE turn=?', ('unknown-old-turn',)).fetchone()
        record = json.loads(row[0])
        self.assertIsNone(record['daybreakEnabled'])
        self.assertIsNone(record['cyberAccessProgram'])

    def test_native_reviewer_does_not_inherit_worker_daybreak_default(self):
        from codex_agent_review import request
        self.settings(worker_defaults={'model': 'gpt-5.6-luna', 'effort': 'high',
                                      'fast_mode': False, 'daybreak_enabled': True})
        a = self.start()
        result = request(self.runtime, a, {}, a['threadId'] + ':review-standard')
        child = self.runtime.agent(result['agentId'])
        self.assertFalse(child['daybreakEnabled'])
        self.assertEqual(child['cyberAccessProgram'], 'standard')

    def test_agent_review_rechecks_mode_after_catalog_read(self):
        from codex_agent_review import request
        a = self.start()
        original = self.runtime.catalog
        def catalog(account='default'):
            result = original(account)
            self.mutate(daybreakEnabled=True, cyberAccessProgram='daybreakBlue')
            return result
        with patch.object(self.runtime, 'catalog', side_effect=catalog):
            with self.assertRaisesRegex(ValueError, 'Daybreak'):
                request(self.runtime, a, {}, a['threadId'] + ':review-race')
        with self.runtime.db() as db:
            self.assertEqual(len(self.runtime.records(db, 'agents')), 1)

    def test_capacity_continuation_rechecks_revoked_access(self):
        self.settings(daybreak_enabled=True)
        self.start()
        retry = self.fail_capacity()
        catalog = access(self.runtime.catalog(), ['standard'])
        with patch.object(self.runtime, 'catalog', return_value=catalog):
            self.runtime.capacity_retry(self.key, retry['id'], 'retry')
            f.eventually(lambda: self.agent()['capacityRetry']['status'] == 'failed')
        self.assertEqual(len(self.starts()), 1)

    def test_worker_mode_comes_from_defaults_and_explicit_override(self):
        self.settings(worker_defaults={'model': 'gpt-5.6-luna', 'effort': 'high',
                                      'fast_mode': False, 'daybreak_enabled': True})
        self.assertTrue(self.agent()['workerDefaults']['daybreakEnabled'])
        child = self.runtime.create({'name': 'Worker', 'prompt': 'Review'}, parent=self.key, defer=True)
        self.assertTrue(child['daybreakEnabled'])
        self.assertEqual(child['cyberAccessProgram'], 'daybreakBlue')
        other = self.runtime.create({'name': 'Other', 'prompt': 'Review', 'daybreak_enabled': False}, parent=self.key, defer=True)
        self.assertFalse(other['daybreakEnabled'])
        self.assertEqual(other['cyberAccessProgram'], 'standard')
        self.assertFalse(self.agent().get('daybreakEnabled', False))

    def test_worker_defaults_omission_means_disabled_and_rejects_invalid_values(self):
        defaults = {'model': None, 'effort': 'high', 'fast_mode': False}
        result = self.settings(worker_defaults=defaults)
        self.assertFalse(result['workerDefaults']['daybreakEnabled'])
        before = self.agent()
        for value in [None, 1, 'true']:
            with self.subTest(value=value), self.assertRaises(ValueError):
                self.settings(worker_defaults={**defaults, 'daybreak_enabled': value})
        self.assertEqual(self.agent(), before)

    def test_creation_receipt_cannot_change_mode(self):
        import uuid
        data = {'id': str(uuid.uuid4()), 'name': 'Draft', 'prompt': '', 'cwd': str(self.root), 'daybreak_enabled': True}
        first = self.runtime.create(data, draft=True)
        self.assertTrue(first['daybreakEnabled'])
        self.assertEqual(self.runtime.create(data, draft=True)['id'], first['id'])
        with self.assertRaisesRegex(ValueError, 'different'):
            self.runtime.create({**data, 'daybreak_enabled': False}, draft=True)

    def test_capacity_continuation_consumes_mode_once_with_empty_input(self):
        self.start()
        self.settings(daybreak_enabled=True, next_turn=True, request_id='enable-next')
        retry = self.fail_capacity()
        self.runtime.capacity_retry(self.key, retry['id'], 'retry')
        f.eventually(lambda: len(self.starts()) == 2 and self.agent().get('turnId'))
        params = self.starts()[-1]
        self.assertEqual(params['cyberAccessProgram'], 'daybreakBlue')
        self.assertEqual(params['input'], [])
        self.assertNotIn('clientUserMessageId', params)
        self.settings(daybreak_enabled=False, next_turn=True, request_id='disable-later')
        self.runtime.capacity_retry(self.key, retry['id'], 'retry')
        self.assertEqual(len(self.starts()), 2)
        self.assertFalse(self.agent()['pendingSettings']['daybreakEnabled'])

    def test_capacity_wrong_account_keeps_pending_mode_unconsumed(self):
        self.start()
        self.settings(daybreak_enabled=True, next_turn=True, request_id='enable-next')
        retry = self.fail_capacity()
        self.mutate(pendingSettingsAccountKey='other-account')
        before = self.agent()
        with self.assertRaisesRegex(ValueError, 'account changed'):
            self.runtime.capacity_retry(self.key, retry['id'], 'retry')
        self.assertEqual(self.agent(), before)
        self.assertEqual(len(self.starts()), 1)


class DaybreakSafetyRetry(unittest.TestCase):
    def setUp(self):
        self.t = safety.Safety('test_retry_exact_native_fork_and_input_preserves_team_and_permissions')
        self.t.setUp()
        self.runtime = self.t.runtime
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.t.key, db)
            a.update(daybreakEnabled=True, cyberAccessProgram='daybreakBlue')
            self.runtime.put(db, 'agents', a)

    def tearDown(self):
        self.t.tearDown()

    def test_unsupported_faster_model_preserves_source_turn_before_native_mutation(self):
        catalog = access(self.runtime.catalog())
        for row in catalog['data']:
            if row['model'] == 'gpt-5.6-sol':
                row['availableAccessPrograms'] = {'cyber': ['standard']}
        self.t.buffering()
        before = self.runtime.agent(self.t.key)
        calls_before = len(self.t.server.calls)
        with patch.object(self.runtime, 'catalog', return_value=catalog):
            self.t.action()
            receipt = self.t.settled()
        self.assertEqual(receipt['stage'], 'failed')
        self.assertIn('Daybreak is not available', receipt['error'])
        after = self.runtime.agent(self.t.key)
        for field in ('threadId', 'turnId', 'model', 'status', 'inFlight', 'daybreakEnabled', 'cyberAccessProgram'):
            self.assertEqual(after[field], before[field], field)
        mutations = {'turn/interrupt', 'thread/fork', 'turn/start'}
        self.assertFalse(any(method in mutations for method, _ in self.t.server.calls[calls_before:]))


class DaybreakTransfer(unittest.TestCase):
    def setUp(self):
        self.t = transfer.TransferContract('test_transfer_keeps_identity_history_queue_and_settings')
        self.t.setUp()
        self.runtime = self.t.runtime
        self.key = self.t.lead_agent['id']
        self.catalog = self.runtime.catalog
        self.programs = ['standard', 'daybreakBlue']
        self.runtime.catalog = lambda account='default': access(
            self.catalog(account), self.programs if account == self.t.other_key else ['standard', 'daybreakBlue'])
        self.runtime.conversation_settings(self.key, {'daybreak_enabled': True})

    def tearDown(self):
        self.t.tearDown()

    def member(self, operation):
        return self.t.receipt(operation['id'])['members'][self.key]

    def test_transfer_preserves_mode_and_revalidates_destination_program(self):
        self.programs = ['standard', 'daybreakRed']
        operation = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.t.complete_fork()
        a = self.runtime.agent(self.key)
        self.assertTrue(a['daybreakEnabled'])
        self.assertEqual(a['cyberAccessProgram'], 'daybreakRed')
        self.assertEqual(a['accountKey'], self.t.other_key)
        self.assertEqual(self.member(operation)['phase'], 'completed')

    def test_unsupported_destination_blocks_before_native_fork(self):
        self.programs = ['standard']
        operation = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: self.member(operation)['phase'] == 'blocked')
        self.assertEqual(self.t.pending, [])
        self.assertEqual(self.runtime.agent(self.key)['accountKey'], 'default')
        self.assertTrue(self.runtime.agent(self.key)['daybreakEnabled'])
        self.assertFalse(any(method == 'thread/unsubscribe' for method, _ in self.t.native_calls))

    def test_transfer_validates_queued_mode_even_when_active_mode_is_off(self):
        self.runtime.conversation_settings(self.key, {'daybreak_enabled': False})
        self.runtime.conversation_settings(self.key, {'daybreak_enabled': True, 'next_turn': True, 'request_id': 'queued'})
        self.programs = ['standard']
        operation = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: self.member(operation)['phase'] == 'blocked')
        self.assertEqual(self.t.pending, [])
        self.assertTrue(self.runtime.agent(self.key)['pendingSettings']['daybreakEnabled'])

    def test_transfer_preserves_queued_mode_and_next_turn_uses_destination_grant(self):
        self.runtime.conversation_settings(self.key, {'daybreak_enabled': False})
        self.runtime.conversation_settings(self.key, {'daybreak_enabled': True, 'next_turn': True, 'request_id': 'queued'})
        self.programs = ['standard', 'daybreakRed']
        operation = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.t.complete_fork()
        a = self.runtime.agent(self.key)
        self.assertFalse(a['daybreakEnabled'])
        self.assertTrue(a['pendingSettings']['daybreakEnabled'])
        self.assertEqual(a['pendingSettings']['cyberAccessProgram'], 'daybreakRed')
        self.assertEqual(a['pendingSettingsAccountKey'], self.t.other_key)
        self.runtime.send(self.key, 'Continue on destination', 'destination-message')
        self.runtime.dispatch()
        self.t.until(lambda: self.runtime.agent(self.key).get('turnId'))
        starts = [params for method, params in self.t.target_server.calls if method == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['cyberAccessProgram'], 'daybreakRed')
        self.assertNotIn('pendingSettings', self.runtime.agent(self.key))
        self.assertEqual(self.member(operation)['phase'], 'completed')

    def test_provider_change_clears_lead_mode_and_preserves_worker_defaults(self):
        self.runtime.conversation_settings(self.key, {'worker_defaults': {
            'model': 'gpt-5.6-luna', 'effort': 'high', 'fast_mode': False, 'daybreak_enabled': True}})
        self.runtime.conversation_settings(self.key, {'daybreak_enabled': True, 'next_turn': True, 'request_id': 'queued'})
        original = self.runtime.accounts.get
        def get(account):
            result = copy.deepcopy(original(account))
            if account == self.t.other_key:
                result['provider'] = 'claude'
            return result
        catalog = access(self.catalog(self.t.other_key), ['standard'])
        catalog['data'][0]['isDefault'] = True
        with patch.object(self.runtime.accounts, 'get', side_effect=get):
            result = self.t.store.destination_settings(self.runtime.agent(self.key), self.t.other_key, catalog)
        self.assertFalse(result['daybreakEnabled'])
        self.assertTrue(result['workerDefaults']['daybreakEnabled'])
        self.assertEqual(result['workerDefaults']['model'], 'gpt-5.6-luna')
        self.assertEqual(result['workerDefaults']['cyberAccessProgram'], 'daybreakBlue')
        self.assertEqual(result.get('cyberAccessProgram'), 'standard')
        self.assertIsNone(result['pendingSettings'])


if __name__ == '__main__':
    unittest.main()

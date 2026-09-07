#!/usr/bin/env python3
"""Native errors reach Studio without repeating commands or releasing active turns."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
# Public CodexErrorInfo variants at rust-v0.153.4 (042fb41b7c81).
CODES = ['contextWindowExceeded', 'sessionBudgetExceeded', 'usageLimitExceeded',
    'rateLimitExceeded', 'serverOverloaded', 'cyberPolicy', 'misalignmentPolicyViolation',
    {'httpConnectionFailed': {'httpStatusCode': 503}},
    {'responseStreamConnectionFailed': {'httpStatusCode': 502}},
    'internalServerError', 'unauthorized', 'badRequest', 'threadRollbackFailed', 'sandboxError',
    {'responseStreamDisconnected': {'httpStatusCode': None}},
    {'responseTooManyFailedAttempts': {'httpStatusCode': 429}},
    {'activeTurnNotSteerable': {'turnKind': 'compact'}}, 'other']

class NativeErrorContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        self.server = self.runtime.connect()
        self.a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Work'})
        self.key = self.a['id']
        fixture.eventually(lambda: bool(self.runtime.agent(self.key).get('turnId')))
        self.a = self.runtime.agent(self.key)
        self.turn = self.a['turnId']
        self.connection = self.runtime.connection_ids['default']

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def send(self, method, **params):
        self.runtime.notification({'method': method, 'params': {
            'threadId': self.a['threadId'], 'turnId': self.turn, **params}}, 'default', self.connection)

    def messages(self):
        with self.runtime.db() as db:
            return [json.loads(r[0]) for r in db.execute('SELECT record FROM runtime_items WHERE agent=?', (self.key,))]

    def test_every_error_variant_preserves_details_without_resubmission(self):
        calls = len(self.server.calls)
        for info in CODES + ['futureError']:
            with self.subTest(info=info):
                error = {'message': 'Native explanation', 'codexErrorInfo': info,
                         'additionalDetails': 'Details', 'misalignment': {'errorType': 'new-category'}}
                self.send('error', error=error, willRetry=False)
                self.assertTrue(self.runtime.agent(self.key)['inFlight'])
                self.assertTrue(any(m.get('nativeError') == error for m in self.messages()))
        self.assertEqual(len(self.server.calls), calls)

    def test_retry_is_transient_and_progress_clears_it(self):
        error = {'message': 'Reconnecting 1/5', 'codexErrorInfo': 'other'}
        self.send('error', error=error, willRetry=True)
        a = self.runtime.agent(self.key)
        self.assertTrue(a['inFlight'])
        self.assertFalse(a.get('error'))
        self.assertEqual(a['activity']['phase'], 'retrying')
        self.assertFalse(any(m.get('nativeNotice') for m in self.messages()))
        self.send('thread/tokenUsage/updated', tokenUsage={})
        self.assertIn('nativeStatus', self.runtime.agent(self.key))
        self.send('item/agentMessage/delta', itemId='answer', delta='Recovered')
        self.assertNotIn('nativeStatus', self.runtime.agent(self.key))
        self.assertEqual(self.runtime.agent(self.key)['activity']['phase'], 'writing')

    def test_stale_turn_account_and_connection_cannot_change_error(self):
        self.send('error', turnId='old', error={'message': 'old'}, willRetry=False)
        message = {'method': 'error', 'params': {'threadId': self.a['threadId'], 'turnId': self.turn,
                   'error': {'message': 'misrouted'}, 'willRetry': False}}
        self.runtime.notification(message, 'other', None)
        self.runtime.notification(message, 'default', 'old-connection')
        self.assertFalse(self.runtime.agent(self.key).get('error'))
        self.assertFalse(any(m.get('nativeNotice') for m in self.messages()))

    def test_terminal_event_keeps_error_and_history_without_duplicate(self):
        error = {'message': 'Sign-in expired', 'codexErrorInfo': 'unauthorized'}
        self.send('error', error=error, willRetry=False)
        self.send('turn/completed', turn={'id': self.turn, 'status': 'failed', 'error': None})
        a = self.runtime.agent(self.key)
        self.assertFalse(a['inFlight'])
        self.assertEqual(a['status'], 'failed')
        self.assertEqual(a['error'], error)
        self.assertEqual(len([m for m in self.messages() if m.get('nativeNotice') == 'error']), 1)
        self.send('error', error={'message': 'late'}, willRetry=True)
        self.assertNotIn('nativeStatus', self.runtime.agent(self.key))

    def test_terminal_error_without_prior_notification_is_recorded(self):
        error = {'message': 'Quota reached', 'codexErrorInfo': 'usageLimitExceeded'}
        self.send('turn/completed', turn={'id': self.turn, 'status': 'failed', 'error': error})
        self.assertEqual(self.runtime.agent(self.key)['error'], error)
        self.assertTrue(any(m.get('nativeError') == error for m in self.messages()))

    def test_auth_recovery_preserves_turn_and_updates_one_notice(self):
        for suffix in ['Started', 'Completed']:
            self.send('modelProvider/authRecovery' + suffix, message='Auth ' + suffix, provider='provider')
            self.assertTrue(self.runtime.agent(self.key)['inFlight'])
        self.assertNotIn('nativeStatus', self.runtime.agent(self.key))
        self.assertEqual(len([m for m in self.messages() if m.get('nativeNotice') == 'info']), 1)

    def test_warning_dedup_mcp_failures_and_approval_noise(self):
        self.send('warning', message='Check configuration')
        self.send('warning', message='Check configuration')
        self.send('guardianWarning', message='Automatic approval review approved (command)')
        self.send('mcpServer/startupStatus/updated', name='Search', status='failed', error='Missing command')
        self.send('mcpServer/oauthLogin/completed', name='Search', success=False, error='Expired')
        self.send('autoApprovalReview/strictReviewRequired', startedAtMs=1)
        self.assertEqual(len([m for m in self.messages() if m.get('nativeNotice')]), 4)
        self.assertFalse(self.runtime.agent(self.key).get('error'))

    def test_resolved_request_clears_approval_without_grant_or_reply(self):
        request = {'id': 17, 'method': 'item/tool/requestUserInput',
                   'params': {'threadId': self.a['threadId'], 'turnId': self.turn, 'questions': []}}
        self.runtime.request(request, 'default', self.connection)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'approval')
        replies = len(self.server.responses)
        self.send('serverRequest/resolved', requestId=18)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'approval')
        self.send('serverRequest/resolved', requestId=17)
        with self.runtime.db() as db:
            records = self.runtime.records(db, 'requests')
        self.assertEqual(records[0]['status'], 'resolved')
        self.assertEqual(self.runtime.agent(self.key)['status'], 'running')
        self.assertEqual(len(self.server.responses), replies)
        with self.assertRaisesRegex(ValueError, 'no longer pending'):
            self.runtime.answer(records[0]['id'], {'answers': {}})

    def test_failed_native_turn_holds_pending_events_until_explicit_resume(self):
        queued = self.runtime.send(self.key, 'Next task', manual=False)
        error = {'message': 'Usage limit reached', 'codexErrorInfo': 'usageLimitExceeded'}
        calls = sum(method == 'turn/start' for method, _ in self.server.calls)
        self.send('turn/completed', turn={'id': self.turn, 'status': 'failed', 'error': error})
        self.runtime.send(self.key, 'Worker result', manual=False)
        self.runtime.dispatch()
        self.assertEqual(self.runtime.agent(self.key)['status'], 'failed')
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), calls)
        self.assertEqual(self.runtime.delivery_receipt(queued['id'])['status'], 'pending')
        self.runtime.send(self.key, 'Continue after fixing the account', manual=True)
        fixture.eventually(lambda: self.runtime.agent(self.key).get('inFlight'))
        self.assertFalse(self.runtime.agent(self.key).get('nativeFailureHold'))

    def test_account_warning_before_thread_is_durable_and_scoped(self):
        self.runtime.notification({'method': 'configWarning', 'params': {'summary': 'Invalid setting', 'details': 'Check config.toml'}}, 'default', self.connection)
        notices = self.runtime.snapshot()['nativeNotices']
        self.assertEqual(notices[0]['accountKey'], 'default')
        self.assertEqual(notices[0]['message'], 'Invalid setting')
        self.assertFalse(any(m.get('nativeNotice') for m in self.messages()))
        self.runtime.notification({'method': 'mcpServer/startupStatus/updated', 'params': {'name': 'Search', 'status': 'failed', 'error': 'Missing command'}}, 'default', self.connection)
        self.assertEqual(len(self.runtime.snapshot()['nativeNotices']), 2)
        self.runtime.notification({'method': 'mcpServer/startupStatus/updated', 'params': {'name': 'Search', 'status': 'ready'}}, 'default', self.connection)
        self.assertEqual(len(self.runtime.snapshot()['nativeNotices']), 1)
        self.runtime.connection_ids['default'] = 'replacement'
        self.assertEqual(self.runtime.snapshot()['nativeNotices'], [])

    def test_steer_rejection_does_not_poison_active_turn(self):
        self.send('error', error={'message': 'Cannot steer compact', 'codexErrorInfo': {'activeTurnNotSteerable': {'turnKind': 'compact'}}}, willRetry=False)
        a = self.runtime.agent(self.key)
        self.assertFalse(a.get('error'))
        self.assertNotIn('nativeTurnError', a)
        self.assertTrue(a['inFlight'])
        self.assertEqual(a['status'], 'running')

    def test_unknown_native_request_returns_protocol_error_without_approval(self):
        self.runtime.request({'id': 19, 'method': 'future/request', 'params': {'threadId': self.a['threadId']}},
                             'default', self.connection)
        self.assertEqual(self.server.responses[-1]['error']['code'], -32601)
        self.assertEqual(self.server.responses[-1]['id'], 19)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'running')

if __name__ == '__main__':
    unittest.main()

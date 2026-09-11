#!/usr/bin/env python3
"""Native errors reach Studio without repeating commands or releasing active turns."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
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
        calls = [call for call in self.server.calls if call[0] != 'account/rateLimits/read']
        for info in CODES + ['futureError']:
            with self.subTest(info=info):
                error = {'message': 'Native explanation', 'codexErrorInfo': info,
                         'additionalDetails': 'Details', 'misalignment': {'errorType': 'new-category'}}
                self.send('error', error=error, willRetry=False)
                self.assertTrue(self.runtime.agent(self.key)['inFlight'])
                self.assertTrue(any(m.get('nativeError') == error for m in self.messages()))
        self.assertEqual([call for call in self.server.calls if call[0] != 'account/rateLimits/read'], calls)

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

    def test_limit_failure_forces_one_read_and_completion_does_not_repeat_it(self):
        self.runtime.limits()
        calls = list(self.server.calls)
        error = {'message': 'Usage exhausted', 'codexErrorInfo': 'usageLimitExceeded'}
        self.send('error', error=error, willRetry=False)
        episode = self.runtime.agent(self.key)['nativeLimitErrorAt']
        self.assertGreater(episode, 0)
        for _ in range(2):
            self.send('error', error=error, willRetry=False)
        fixture.eventually(lambda: len(self.server.calls) > len(calls))
        self.send('turn/completed', turn={'id': self.turn, 'status': 'failed', 'error': error})
        for _ in range(3):
            self.runtime.snapshot()
            self.runtime.limits()
        self.assertEqual(self.server.calls[len(calls):], [('account/rateLimits/read', {})])
        self.assertEqual(self.runtime.agent(self.key)['nativeLimitErrorAt'], episode)
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_native_limit_refreshes').fetchone()[0], 1)

    def test_terminal_only_rate_limit_failure_refreshes_without_model_retry(self):
        self.runtime.limits()
        calls = list(self.server.calls)
        error = {'message': 'Rate exceeded', 'codexErrorInfo': 'rateLimitExceeded'}
        self.send('turn/completed', turn={'id': self.turn, 'status': 'failed', 'error': error})
        fixture.eventually(lambda: len(self.server.calls) > len(calls))
        self.send('turn/completed', turn={'id': self.turn, 'status': 'failed', 'error': error})
        self.assertEqual(self.server.calls[len(calls):], [('account/rateLimits/read', {})])
        self.assertEqual(self.runtime.agent(self.key)['status'], 'failed')

    def test_limit_recovery_skips_replaced_connection_before_read(self):
        done = threading.Event()
        original = self.runtime.limits
        def tracked(*args, **kwargs):
            try:
                return original(*args, **kwargs)
            finally:
                done.set()
        self.runtime.limits = tracked
        calls = list(self.server.calls)
        with self.runtime.lock:
            self.send('error', error={'message': 'Rate exceeded', 'codexErrorInfo': 'rateLimitExceeded'}, willRetry=False)
            self.runtime.connection_ids['default'] = 'replacement'
        self.assertTrue(done.wait(2))
        self.assertEqual(self.server.calls, calls)

    def test_limit_recovery_discards_read_from_replaced_connection(self):
        entered, release, done = threading.Event(), threading.Event(), threading.Event()
        original_call, original_limits = self.server.call, self.runtime.limits
        def call(method, params, **kwargs):
            if method == 'account/rateLimits/read':
                entered.set()
                if not release.wait(2):
                    raise RuntimeError('Test did not release the read')
            return original_call(method, params, **kwargs)
        def tracked(*args, **kwargs):
            try:
                return original_limits(*args, **kwargs)
            finally:
                done.set()
        self.server.call, self.runtime.limits = call, tracked
        try:
            self.send('error', error={'message': 'Usage exhausted', 'codexErrorInfo': 'usageLimitExceeded'}, willRetry=False)
            self.assertTrue(entered.wait(2))
            with self.runtime.lock:
                self.runtime.connection_ids['default'] = 'replacement'
                self.runtime.set_rate_limits('default', {'data': {'replacement': True}, 'at': 1, 'error': None})
            release.set()
            self.assertTrue(done.wait(2))
            self.assertEqual(self.runtime.rate_limits_for()['data'], {'replacement': True})
        finally:
            release.set()

    def hook_run(self, status='running', entries=None):
        return {'id': 'native-hook-1', 'eventName': 'preToolUse', 'handlerType': 'command',
                'executionMode': 'sync', 'scope': 'turn', 'sourcePath': '/tmp/hooks.json',
                'source': 'project', 'displayOrder': 0, 'status': status,
                'statusMessage': 'Check command', 'startedAt': 1, 'completedAt': None,
                'durationMs': None, 'entries': entries or []}

    def test_hook_failure_updates_one_notice_without_changing_turn(self):
        calls = len(self.server.calls)
        self.send('hook/started', run=self.hook_run())
        entries = [{'kind': 'warning', 'text': 'Warning text'}, {'kind': 'error', 'text': 'Exit failed'},
                   {'kind': 'stop', 'text': 'Stop reason'}, {'kind': 'context', 'text': 'Hidden model context'},
                   {'kind': 'warning', 'text': 'Second warning hidden by native TUI'}]
        self.send('hook/completed', run=self.hook_run('failed', entries))
        self.send('hook/started', run=self.hook_run())
        hooks = [m for m in self.messages() if m.get('nativeHook')]
        self.assertEqual(len(hooks), 1)
        self.assertEqual(hooks[0]['nativeHook']['status'], 'failed')
        self.assertIn('Exit failed', hooks[0]['details'])
        self.assertIn('Stop reason', hooks[0]['details'])
        self.assertNotIn('Hidden model context', json.dumps(hooks))
        self.assertNotIn('Second warning', json.dumps(hooks))
        self.assertTrue(self.runtime.agent(self.key)['inFlight'])
        self.assertFalse(self.runtime.agent(self.key).get('error'))
        self.assertEqual(len(self.server.calls), calls)

    def test_hook_quiet_success_has_no_durable_notice(self):
        self.send('hook/started', run=self.hook_run())
        self.send('hook/completed', run=self.hook_run('completed', [{'kind': 'context', 'text': 'Hidden'}]))
        hooks = [m for m in self.messages() if m.get('nativeHook')]
        self.assertEqual(len(hooks), 1)
        self.assertTrue(hooks[0]['nativeHookQuiet'])
        self.assertEqual(hooks[0]['nativeHook']['status'], 'completed')

    def test_hook_late_and_thread_scope_outcomes_preserve_identity(self):
        self.send('hook/completed', threadId='different', run=self.hook_run('failed'))
        self.runtime.notification({'method': 'hook/completed', 'params': {
            'threadId': self.a['threadId'], 'run': self.hook_run('failed')}}, 'default', 'old-connection')
        self.assertFalse(any(m.get('nativeHook') for m in self.messages()))
        self.send('turn/completed', turn={'id': self.turn, 'status': 'completed'})
        self.send('hook/completed', run=self.hook_run('blocked'))
        self.assertTrue(any(m.get('nativeHook', {}).get('status') == 'blocked' for m in self.messages()))
        self.send('hook/completed', turnId=None, run={**self.hook_run('stopped'), 'id': 'thread-hook', 'scope': 'thread'})
        self.assertEqual(sum(bool(m.get('nativeHook')) for m in self.messages()), 2)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'completed')

    def policy_error(self):
        return {'message': 'Native precaution', 'codexErrorInfo': 'misalignmentPolicyViolation',
                'misalignment': {'errorType': 'untrusted', 'details': 'Exact native details'}}

    def test_policy_block_preserves_queue_and_exact_receipts(self):
        queued = self.runtime.send(self.key, 'Pending task', message_id='before-policy')
        error = self.policy_error()
        self.send('error', error=error, willRetry=False)
        self.send('turn/completed', turn={'id': self.turn, 'status': 'failed'})
        for options in ({'manual': True}, {'manual': False}, {'resume': True}, {'delivery': 'steer'}):
            with self.subTest(options=options), self.assertRaisesRegex(ValueError, 'another chat'):
                self.runtime.send(self.key, 'Continue', message_id='after-policy', **options)
        self.assertEqual(self.runtime.send(self.key, 'Pending task', message_id='before-policy'),
                         {'id': queued['id'], 'status': 'pending', 'error': None})
        self.assertIsNone(self.runtime.delivery_receipt('after-policy'))
        for action in ('compact', 'review'):
            with self.assertRaisesRegex(ValueError, 'another chat'):
                self.runtime.native_action(self.key, action)
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            a.update(status='queued', autoWake=True, nativeFailureHold=False, error=None)
            self.runtime.put(db, 'agents', a)
            calls = len(self.server.calls)
            self.runtime.dispatch()
            self.assertEqual(len(self.server.calls), calls)
        visible = next(a for a in self.runtime.snapshot()['agents'] if a['id'] == self.key)
        self.assertFalse(visible['canSend'])
        self.assertEqual(visible['nativeThreadBlock'], {'threadId': self.a['threadId'], 'error': error})
        self.assertEqual(self.runtime.delivery_receipt(queued['id'])['status'], 'pending')

    def test_policy_completed_only_and_restart_keep_original_error(self):
        error = self.policy_error()
        self.send('turn/completed', turn={'id': self.turn, 'status': 'failed', 'error': error})
        self.runtime.close()
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        a = self.runtime.agent(self.key)
        self.assertEqual(a['nativeThreadBlock']['error'], error)
        with self.assertRaisesRegex(ValueError, 'another chat'):
            self.runtime.send(self.key, 'Resume', resume=True)

    def test_policy_before_completion_survives_disconnect_and_restart(self):
        error = self.policy_error()
        self.send('error', error=error, willRetry=False)
        self.runtime.disconnected('default', self.connection)
        self.assertNotEqual(self.runtime.agent(self.key)['error'], error)
        self.runtime.close()
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        self.assertEqual(self.runtime.agent(self.key)['nativeThreadBlock']['error'], error)
        with self.assertRaisesRegex(ValueError, 'another chat'):
            self.runtime.send(self.key, 'Resume')

    def test_policy_requires_exact_current_turn_and_thread(self):
        error = self.policy_error()
        for params in ({'turnId': 'old'}, {'turnId': None}, {'threadId': 'other'}):
            self.send('error', error=error, willRetry=False, **params)
        self.assertNotIn('nativeThreadBlock', self.runtime.agent(self.key))
        self.send('error', error=error, willRetry=True)
        self.assertNotIn('nativeThreadBlock', self.runtime.agent(self.key))
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            a['nativeThreadBlock'] = {'threadId': 'another-thread', 'error': error}
            self.runtime.put(db, 'agents', a)
        self.runtime.send(self.key, 'Current thread still accepts input')
        self.assertTrue(next(a for a in self.runtime.snapshot()['agents'] if a['id'] == self.key)['canSend'])

    def test_policy_blocks_pending_and_later_native_requests_without_response(self):
        request = {'id': 'policy-approval', 'method': 'item/commandExecution/requestApproval',
                   'params': {'threadId': self.a['threadId'], 'turnId': self.turn}}
        self.runtime.request(request, 'default', self.connection)
        with self.runtime.db() as db:
            pending = next(r for r in self.runtime.records(db, 'requests') if r['rpcId'] == request['id'])
        self.send('error', error=self.policy_error(), willRetry=False)
        replies = len(self.server.responses)
        with self.assertRaisesRegex(ValueError, 'another chat'):
            self.runtime.answer(pending['id'], {'decision': 'accept'})
        self.runtime.request({**request, 'id': 'later-approval'}, 'default', self.connection)
        with self.runtime.db() as db:
            records = self.runtime.records(db, 'requests')
        self.assertEqual(next(r for r in records if r['id'] == pending['id'])['status'], 'pending')
        self.assertEqual(next(r for r in records if r['rpcId'] == 'later-approval')['status'], 'blocked')
        self.assertEqual(len(self.server.responses), replies)

    def test_legacy_approval_identity_and_unblocked_response(self):
        for method in ('execCommandApproval', 'applyPatchApproval'):
            request = {'id': method, 'method': method, 'params': {
                'conversationId': self.a['threadId'], 'threadId': 'ignored-wrong-field', 'callId': method}}
            self.runtime.request(request, 'default', self.connection)
            with self.runtime.db() as db:
                record = next(r for r in self.runtime.records(db, 'requests') if r['rpcId'] == method)
            self.assertEqual(record['agent'], self.key)
            self.assertEqual(self.runtime.answer(record['id'], {'decision': 'accept'})['status'], 'answered')
            self.assertEqual(self.server.responses[-1], {'id': method, 'result': {'decision': 'approved'}})

    def test_legacy_approvals_before_and_after_policy_block_do_not_reply(self):
        records = []
        for method in ('execCommandApproval', 'applyPatchApproval'):
            self.runtime.request({'id': method, 'method': method, 'params': {
                'conversationId': self.a['threadId'], 'callId': method}}, 'default', self.connection)
            with self.runtime.db() as db:
                record = next(r for r in self.runtime.records(db, 'requests') if r['rpcId'] == method)
                self.assertEqual(record['agent'], self.key)
                if method == 'applyPatchApproval':
                    # An existing record can predate correct legacy identity binding.
                    record['agent'] = None
                    self.runtime.put(db, 'requests', record)
                records.append(record)
        self.send('error', error=self.policy_error(), willRetry=False)
        responses = list(self.server.responses)
        for record in records:
            with self.assertRaisesRegex(ValueError, 'another chat'):
                self.runtime.answer(record['id'], {'decision': 'accept'})
            self.runtime.request({'id': 'later-' + record['rpcId'], 'method': record['method'],
                                  'params': record['params']}, 'default', self.connection)
        with self.runtime.db() as db:
            late = [r for r in self.runtime.records(db, 'requests') if str(r['rpcId']).startswith('later-')]
        self.assertEqual(len(late), 2)
        self.assertTrue(all(r['status'] == 'blocked' and r['agent'] == self.key for r in late))
        self.assertEqual(self.server.responses, responses)

    def test_native_resolution_uses_legacy_conversation_identity(self):
        for method in ('execCommandApproval', 'applyPatchApproval'):
            self.runtime.request({'id': method, 'method': method, 'params': {
                'conversationId': self.a['threadId'], 'callId': method}}, 'default', self.connection)
            self.send('serverRequest/resolved', threadId='unrelated', requestId=method)
            with self.runtime.db() as db:
                record = next(r for r in self.runtime.records(db, 'requests') if r['rpcId'] == method)
                self.assertEqual(record['status'], 'pending')
                if method == 'applyPatchApproval':
                    record['agent'] = None
                    self.runtime.put(db, 'requests', record)
            responses = list(self.server.responses)
            self.send('serverRequest/resolved', requestId=method)
            with self.runtime.db() as db:
                record = next(r for r in self.runtime.records(db, 'requests') if r['rpcId'] == method)
            self.assertEqual(record['status'], 'resolved')
            self.assertEqual(self.server.responses, responses)

    def test_conversation_id_is_not_a_fallback_for_other_requests(self):
        self.runtime.request({'id': 'v2-wrong-field', 'method': 'item/fileChange/requestApproval',
                              'params': {'conversationId': self.a['threadId']}}, 'default', self.connection)
        with self.runtime.db() as db:
            record = next(r for r in self.runtime.records(db, 'requests') if r['rpcId'] == 'v2-wrong-field')
        self.assertIsNone(record['agent'])

    def test_policy_duplicate_claim_does_not_replace_running_receipt(self):
        request = {'id': 'duplicate-policy', 'method': 'item/tool/call', 'params': {
            'threadId': self.a['threadId'], 'turnId': self.turn, 'callId': 'duplicate-policy',
            'tool': 'orchestration_title', 'arguments': {'title': 'Must not execute twice'}}}
        reserve = self.runtime.reserve_tool_request
        observed = {}
        def reserve_then_duplicate_claim(*args, **kwargs):
            stale = reserve(*args, **kwargs)
            self.assertEqual(stale['stage'], 'queued')
            self.assertTrue(self.runtime.begin_tool_request(stale['id']))
            observed['receipt'] = self.runtime.tool_request(stale['id'])
            self.send('error', error=self.policy_error(), willRetry=False)
            return stale
        self.runtime.reserve_tool_request = reserve_then_duplicate_claim
        name = self.runtime.agent(self.key)['name']
        self.runtime.dynamic(request, 'default', self.connection)
        self.assertEqual(self.runtime.tool_request(observed['receipt']['id']), observed['receipt'])
        self.assertEqual(self.runtime.agent(self.key)['name'], name)
        result = self.server.responses[-1]['result']
        self.assertTrue(result['success'])
        self.assertIn('running', json.dumps(result))
        self.assertNotIn('not_applied', json.dumps(result))

    def test_policy_dynamic_guard_preserves_completed_receipt(self):
        def message(call, title):
            return {'id': call, 'method': 'item/tool/call', 'params': {
                'threadId': self.a['threadId'], 'turnId': self.turn, 'callId': call,
                'tool': 'orchestration_title', 'arguments': {'title': title}}}
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            a.update(isLead=True, manualName=False)
            self.runtime.put(db, 'agents', a)
        before = message('title-before', 'Accepted title')
        self.runtime.dynamic(before, 'default', self.connection)
        original = self.server.responses[-1]['result']
        self.assertTrue(original['success'])
        self.send('error', error=self.policy_error(), willRetry=False)
        self.runtime.dynamic(message('title-after', 'Rejected title'), 'default', self.connection)
        self.assertFalse(self.server.responses[-1]['result']['success'])
        self.assertEqual(self.runtime.agent(self.key)['name'], 'Accepted title')
        self.runtime.dynamic(before, 'default', self.connection)
        self.assertEqual(self.server.responses[-1]['result'], original)

    def test_policy_blocks_turn_start_after_preparation(self):
        from codex_native_errors import preserve_thread_block
        self.send('turn/completed', turn={'id': self.turn, 'status': 'completed'})
        original = self.runtime.prepare
        def block_after_prepare(a):
            result = original(a)
            with self.runtime.lock, self.runtime.db() as db:
                current = self.runtime.agent(self.key, db)
                preserve_thread_block(current, self.policy_error())
                self.runtime.put(db, 'agents', current)
            return result
        self.runtime.prepare = block_after_prepare
        calls = len(self.server.calls)
        self.runtime.send(self.key, 'Submit after preparation')
        fixture.eventually(lambda: bool(self.runtime.agent(self.key).get('nativeThreadBlock')))
        fixture.eventually(lambda: self.runtime.agent(self.key)['status'] == 'failed')
        self.assertFalse(any(method == 'turn/start' for method, _ in self.server.calls[calls:]))

    def test_policy_final_submission_guard_after_preparation(self):
        from codex_native_errors import preserve_thread_block
        self.send('turn/completed', turn={'id': self.turn, 'status': 'completed'})
        original = self.runtime.prepare
        def block_after_prepare(a):
            result = original(a)
            with self.runtime.lock, self.runtime.db() as db:
                current = self.runtime.agent(self.key, db)
                preserve_thread_block(current, self.policy_error())
                self.runtime.put(db, 'agents', current)
            return result
        self.runtime.prepare = block_after_prepare
        calls = len(self.server.calls)
        with self.assertRaisesRegex(ValueError, 'another chat'):
            self.runtime.native_action(self.key, 'compact')
        self.assertFalse(any(method in {'thread/compact/start', 'review/start', 'turn/start'}
                             for method, _ in self.server.calls[calls:]))

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

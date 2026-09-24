#!/usr/bin/env python3
"""Native review through Studio tool receipts and dispatch. No model calls."""
import concurrent.futures
import copy
import importlib.util
import json
import os
import subprocess
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('account_fixture', Path(__file__).with_name('runtime-accounts-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_runtime import ResponseTimeout

FINDINGS = '[P1] Preserve the receipt before command submission. File: server.py:42.'


class ReviewServer(f.AccountServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.mode = 'running'
        self.reviews = []

    def call(self, method, params, timeout=60):
        if method == 'turn/interrupt' and params.get('turnId') == '':
            self.calls.append((method, copy.deepcopy(params)))
            return {}
        return super().call(method, params, timeout)

    def submit(self, method, params):
        if method != 'review/start':
            return super().submit(method, params)
        self.calls.append((method, copy.deepcopy(params)))
        entry = {'params': copy.deepcopy(params), 'future': concurrent.futures.Future(),
                 'turn': {'id': 'review-' + str(len(self.reviews)), 'status': 'inProgress'},
                 'handled': threading.Event(), 'registered': threading.Event()}
        self.reviews.append(entry)
        if self.mode == 'early':
            self.finish(entry)
        if self.mode != 'lost':
            entry['future'].set_result({'turn': entry['turn'], 'reviewThreadId': params['threadId']})
        return entry['future']

    def wait(self, future, timeout=60):
        if any(e['future'] is future for e in self.reviews) and not future.done():
            raise ResponseTimeout('review/start response timed out; outcome unknown')
        return super().wait(future, timeout)

    def on_result(self, future, callback):
        entry = next((e for e in self.reviews if e['future'] is future), None)
        if entry is None:
            return super().on_result(future, callback)
        def done(result):
            work = callback(result)
            if work:
                work.add_done_callback(lambda _: entry['handled'].set())
            else:
                entry['handled'].set()
        future.add_done_callback(done)
        entry['registered'].set()

    def finish(self, entry, status='completed'):
        params = {'threadId': entry['params']['threadId'], 'turnId': entry['turn']['id']}
        self.notify({'method': 'item/completed', 'params': {**params,
            'item': {'id': 'findings', 'type': 'exitedReviewMode', 'review': FINDINGS}}})
        self.notify({'method': 'turn/completed', 'params': {**params,
            'turn': {'id': entry['turn']['id'], 'status': status}}})


class ReviewRuntimeContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-review-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        # Native review reads git history; the reviewer folder must be a repository.
        subprocess.run(['git', 'init', '-q', str(self.root)], check=True)
        self.env = patch.dict(os.environ, {'CODEX_HOME': str(self.root / 'home')})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.rt = f.ControlledRuntime(self.root / 'state', ReviewServer)
        self.addCleanup(self.rt.close)
        self.server = self.rt.connect()
        self.parent = self.rt.new_lead({'cwd': str(self.root), 'model': 'test-model'})
        self.parent = self.rt.prepare(self.parent)
        self.update(self.parent['id'], autoWake=True, inFlight=True, status='running', turnId='parent-turn')
        self.parent = self.rt.agent(self.parent['id'])
        self.seq = 100

    def update(self, key, **values):
        with self.rt.lock, self.rt.db() as db:
            a = self.rt.agent(key, db)
            a.update(values)
            self.rt.put(db, 'agents', a)
        return a

    def invoke(self, args=None, *, actor=None):
        self.seq += 1
        actor = actor or self.parent
        name = 'orchestration_review'
        args = copy.deepcopy(args if args is not None else {'request_id': 'review-fixture'})
        self.rt.dynamic({'id': self.seq, 'params': {'threadId': actor['threadId'],
            'turnId': actor['turnId'], 'callId': str(self.seq), 'tool': name, 'arguments': args}},
            actor.get('accountKey', 'default'))
        response = next(r['result'] for r in self.server.responses if r['id'] == self.seq)
        return response

    def request(self, **options):
        result = self.invoke(**options)
        self.assertTrue(result['success'], result)
        return json.loads(result['contentItems'][0]['text'])

    def dispatch_review(self, value):
        self.rt.dispatch()
        f.f.eventually(lambda: bool(self.server.reviews))
        f.f.eventually(lambda: self.rt.agent(value['agentId'])['status'] in {'running', 'completed', 'starting'})
        entry = self.server.reviews[-1]
        if self.server.mode == 'lost':
            self.assertTrue(entry['registered'].wait(3))
        else:
            f.f.eventually(lambda: self.rt.agent(value['agentId']).get('startAttempt', {}).get('turnId') == entry['turn']['id'])
        return entry

    def parent_results(self):
        with self.rt.db() as db:
            return db.execute("SELECT id,text FROM runtime_events WHERE agent=? AND kind='child_result'",
                              (self.parent['id'],)).fetchall()

    def test_retries_share_one_durable_receipt(self):
        first = self.request()
        self.assertEqual(self.request(), first)
        self.assertEqual(self.request(), first)
        with self.rt.db() as db:
            children = [a for a in self.rt.records(db, 'agents') if a.get('parentId') == self.parent['id']]
            receipts = [r for r in self.rt.records(db, 'tool_requests') if r['id'] == first['requestId']]
        self.assertEqual([a['id'] for a in children], [first['agentId']])
        self.assertEqual(len(receipts), 1)
        self.assertEqual(receipts[0]['outcome'], 'applied')
        conflict = self.invoke({'request_id': 'review-fixture', 'target': {'type': 'baseBranch', 'branch': 'other'}})
        self.assertFalse(conflict['success'])
        self.assertFalse(self.server.reviews)

    def test_receipt_recovery_reports_current_child(self):
        request_id = 'recovery-direct'
        first = self.request(args={'request_id': request_id})
        self.assertEqual(self.request(args={'request_id': request_id}), first)
        receipt = self.rt.request_action(self.parent['id'], {
            'action': 'get', 'request_id': request_id})
        self.assertEqual(receipt['agentIds'], [first['agentId']])
        self.assertEqual(receipt['tool'], 'orchestration_review')
        self.assertEqual([(a['id'], a['status']) for a in receipt['agents']],
                         [(first['agentId'], 'queued')])
        self.update(first['agentId'], status='completed', inFlight=False)
        recovered = self.rt.request_action(self.parent['id'], {
            'action': 'get', 'request_id': first['requestId']})
        self.assertEqual(recovered['agentIds'], [first['agentId']])
        self.assertEqual([(a['id'], a['status']) for a in recovered['agents']],
                         [(first['agentId'], 'completed')])
        self.assertEqual(recovered['result'], receipt['result'])
        self.assertEqual(recovered['outcome'], 'applied')


    def test_dispatch_uses_child_native_review_with_inherited_identity_and_readonly_policy(self):
        target = {'type': 'baseBranch', 'branch': 'main'}
        value = self.request(args={'target': target, 'request_id': 'base-main'})
        child = self.rt.agent(value['agentId'])
        for field in ('cwd', 'model', 'effort', 'accountKey', 'provider'):
            self.assertEqual(child[field], self.parent[field], field)
        self.assertEqual(child['role'], 'reviewer')
        self.assertFalse(child['worktree'])
        self.assertEqual(self.rt.tool_definitions(child), [])
        entry = self.dispatch_review(value)
        child = self.rt.agent(child['id'])
        self.assertEqual(entry['params'], {'threadId': child['threadId'], 'target': target, 'delivery': 'inline'})
        self.assertNotEqual(child['threadId'], self.parent['threadId'])
        self.assertFalse(any(m == 'turn/start' for m, _ in self.server.calls))
        start = next(p for m, p in self.server.calls if m == 'thread/start' and p.get('dynamicTools') == [])
        self.assertEqual(start['sandbox'], 'read-only')
        settings = next(p for m, p in self.server.calls if m == 'thread/settings/update' and p['threadId'] == child['threadId'])
        self.assertEqual(settings['sandboxPolicy'], {'type': 'readOnly'})
        self.assertEqual(settings['model'], self.parent['model'])
        self.rt.dispatch()
        self.assertEqual(len(self.server.reviews), 1)

    def test_team_concurrency_blocks_review_until_parent_yields(self):
        self.update(self.parent['id'], concurrency=1)
        value = self.request()
        self.rt.dispatch()
        self.assertEqual(self.rt.agent(value['agentId'])['status'], 'queued')
        self.assertFalse(self.server.reviews)
        self.update(self.parent['id'], inFlight=False, status='waiting', turnId=None)
        self.dispatch_review(value)
        self.assertEqual(len(self.server.reviews), 1)

    def test_findings_before_rpc_response_without_started_event_reach_parent_once(self):
        self.server.mode = 'early'
        value = self.request()
        entry = self.dispatch_review(value)
        child = self.rt.agent(value['agentId'])
        self.assertEqual(child['status'], 'completed')
        self.assertFalse(child['inFlight'])
        self.assertEqual(child['lastAnswer'], FINDINGS)
        with self.rt.db() as db:
            records = [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_items WHERE agent=?', (child['id'],))]
        self.assertTrue(any(FINDINGS in item['text'] for item in records))
        self.assertEqual(len(self.parent_results()), 1)
        self.assertEqual(json.loads(self.parent_results()[0]['text'])['result'], FINDINGS)
        self.server.finish(entry)
        self.rt.dispatch()
        self.assertEqual(len(self.parent_results()), 1)
        self.assertEqual(len(self.server.reviews), 1)
        self.assertEqual(self.rt.agent(child['id'])['status'], 'completed')

    def test_lost_response_never_repeats_and_late_result_preserves_completion(self):
        self.server.mode = 'lost'
        value = self.request()
        entry = self.dispatch_review(value)
        self.assertTrue(self.rt.agent(value['agentId'])['inFlight'])
        self.assertEqual(self.request(), value)
        self.rt.dispatch()
        self.assertEqual(len(self.server.reviews), 1)
        self.server.finish(entry)
        self.assertEqual(len(self.parent_results()), 1)
        entry['future'].set_result({'turn': entry['turn']})
        self.assertTrue(entry['handled'].wait(3))
        self.assertEqual(self.rt.agent(value['agentId'])['status'], 'completed')
        self.assertFalse(self.rt.agent(value['agentId'])['inFlight'])
        self.assertEqual(self.rt.agent(value['agentId'])['lastAnswer'], FINDINGS)
        self.assertEqual(len(self.parent_results()), 1)
        self.rt.dispatch()
        self.assertEqual(len(self.server.reviews), 1)

    def test_parent_pause_cancels_queued_review_without_native_submission(self):
        value = self.request()
        self.rt.stop(self.parent['id'])
        self.rt.dispatch()
        child = self.rt.agent(value['agentId'])
        self.assertEqual(child['status'], 'paused')
        self.assertFalse(child['autoWake'])
        self.assertFalse(self.server.reviews)

    def test_parent_pause_interrupts_active_child_exact_turn(self):
        value = self.request()
        entry = self.dispatch_review(value)
        self.rt.stop(self.parent['id'])
        interrupts = [p for m, p in self.server.calls if m == 'turn/interrupt' and p['threadId'] == entry['params']['threadId']]
        self.assertEqual(interrupts, [{'threadId': entry['params']['threadId'], 'turnId': entry['turn']['id']}])
        self.assertEqual(self.rt.agent(value['agentId'])['status'], 'paused')
        self.assertEqual(len(self.parent_results()), 0)

    def test_claude_and_native_reviewer_cannot_create_reviews(self):
        self.assertIn('orchestration_review', {t['name'] for t in self.rt.tool_definitions(self.parent)})
        claude = self.update(self.parent['id'], provider='claude')
        self.assertNotIn('orchestration_review', {t['name'] for t in self.rt.tool_definitions(claude)})
        result = self.invoke(actor=claude)
        self.assertFalse(result['success'])
        self.assertIn('only for Codex', result['contentItems'][0]['text'])
        self.update(self.parent['id'], provider='codex')
        value = self.request(args={'request_id': 'allowed'})
        child = self.rt.prepare(self.rt.agent(value['agentId']))
        child = self.update(child['id'], status='running', inFlight=True, turnId='forged-review-tool-turn')
        result = self.invoke(args={'request_id': 'nested'}, actor=child)
        self.assertFalse(result['success'])
        self.assertIn('cannot create another review', result['contentItems'][0]['text'])
        self.assertEqual(self.rt.tool_definitions(child), [])

    def test_nondefault_account_owns_review_rpc_and_receipt(self):
        other_home = self.root / 'other-home'
        other_home.mkdir()
        (other_home / 'auth.json').write_text(json.dumps({'tokens': {
            'account_id': 'review-other-account', 'access_token': 'fixture'}}))
        account = self.rt.accounts.register(str(other_home))
        original_server = self.server
        self.server = self.rt.connect(account)
        self.parent = self.rt.new_lead({'cwd': str(self.root), 'account_key': account})
        self.parent = self.rt.prepare(self.parent)
        self.parent = self.update(self.parent['id'], autoWake=True, inFlight=True,
                                  status='running', turnId='other-parent-turn')
        value = self.request()
        self.assertTrue(value['requestId'].startswith(account + ':'))
        self.assertEqual(self.rt.agent(value['agentId'])['accountKey'], account)
        self.dispatch_review(value)
        self.assertFalse(original_server.reviews)
        self.assertEqual(len(self.server.reviews), 1)
        self.assertEqual(self.request(), value)

    def test_cancelled_tool_receipt_prevents_creation_on_later_callback(self):
        message = {'id': 99, 'params': {'threadId': self.parent['threadId'],
            'turnId': self.parent['turnId'], 'callId': 'cancel-reserved',
            'tool': 'orchestration_review', 'arguments': {'request_id': 'cancel-before-create'}}}
        receipt = self.rt.reserve_tool_request(message, 'default', self.rt.connection_ids['default'])
        cancelled = self.rt.request_action(self.parent['id'], {'action': 'cancel', 'request_id': receipt['id']})
        self.assertEqual(cancelled['outcome'], 'not_applied')
        result = self.invoke({'request_id': 'cancel-before-create'})
        self.assertFalse(result['success'])
        with self.rt.db() as db:
            self.assertEqual(len(self.rt.records(db, 'agents')), 1)
        self.assertFalse(self.server.reviews)

    def test_pause_during_unknown_submission_interrupts_late_acknowledgement(self):
        self.server.mode = 'lost'
        value = self.request()
        entry = self.dispatch_review(value)
        self.rt.stop(self.parent['id'])
        entry['future'].set_result({'turn': entry['turn']})
        self.assertTrue(entry['handled'].wait(3))
        child = self.rt.agent(value['agentId'])
        self.assertEqual(child['status'], 'paused')
        self.assertFalse(child['autoWake'])
        interrupts = [p for m, p in self.server.calls if m == 'turn/interrupt'
                      and p['threadId'] == entry['params']['threadId']]
        self.assertEqual(interrupts, [
            {'threadId': entry['params']['threadId'], 'turnId': ''},
            {'threadId': entry['params']['threadId'], 'turnId': entry['turn']['id']}])
        self.assertEqual(len(self.parent_results()), 0)
        self.rt.dispatch()
        self.assertEqual(len(self.server.reviews), 1)

    def test_native_cancel_completion_before_late_response_does_not_restart_child(self):
        self.server.mode = 'lost'
        value = self.request()
        entry = self.dispatch_review(value)
        self.rt.stop(self.parent['id'])
        completion = {'method': 'turn/completed', 'params': {
            'threadId': entry['params']['threadId'],
            'turn': {'id': entry['turn']['id'], 'status': 'interrupted'}}}
        self.server.notify(completion)
        entry['future'].set_result({'turn': entry['turn']})
        self.assertTrue(entry['handled'].wait(3))
        self.server.notify(completion)
        self.rt.dispatch()
        child = self.rt.agent(value['agentId'])
        self.assertEqual(child['status'], 'paused')
        self.assertFalse(child['inFlight'])
        self.assertIsNone(child['turnId'])
        self.assertEqual(child['lastCompletedTurnStatus'], 'interrupted')
        interrupts = [p for m, p in self.server.calls if m == 'turn/interrupt'
                      and p['threadId'] == entry['params']['threadId']]
        self.assertEqual(interrupts, [{'threadId': entry['params']['threadId'], 'turnId': ''}])
        self.assertEqual(len(self.parent_results()), 0)
        self.assertEqual(len(self.server.reviews), 1)

    def test_unknown_review_cancel_rejects_changed_account_or_connection(self):
        self.server.mode = 'lost'
        value = self.request()
        entry = self.dispatch_review(value)
        child = self.rt.agent(value['agentId'])
        self.update(child['id'], autoWake=False, status='paused', epoch=child['epoch'] + 1)
        baseline = len([m for m, _ in self.server.calls if m == 'turn/interrupt'])
        for change in ({'accountKey': 'different-account'}, {'connectionId': 'retired-connection'}):
            with self.subTest(change=change):
                current = self.rt.agent(child['id'])
                attempt = {**current['startAttempt'], **change}
                self.update(child['id'], startAttempt=attempt)
                self.rt.interrupt(current)
                self.assertEqual(len([m for m, _ in self.server.calls if m == 'turn/interrupt']), baseline)
                self.update(child['id'], startAttempt=child['startAttempt'])
        current = self.rt.agent(child['id'])
        self.rt.interrupt(current)
        self.assertEqual(self.server.calls[-1], ('turn/interrupt', {'threadId': entry['params']['threadId'], 'turnId': ''}))
        self.assertEqual(len([m for m, _ in self.server.calls if m == 'turn/interrupt']), baseline + 1)

    def test_existing_inline_ui_review_stays_on_selected_chat(self):
        self.update(self.parent['id'], inFlight=False, status='completed', turnId=None)
        self.rt.native_action(self.parent['id'], 'review')
        self.assertEqual(len(self.server.reviews), 1)
        self.assertEqual(self.server.reviews[0]['params'], {'threadId': self.parent['threadId'],
            'target': {'type': 'uncommittedChanges'}, 'delivery': 'inline'})
        with self.rt.db() as db:
            self.assertEqual(len(self.rt.records(db, 'agents')), 1)


if __name__ == '__main__':
    unittest.main()

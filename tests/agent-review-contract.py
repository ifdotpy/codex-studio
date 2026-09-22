#!/usr/bin/env python3
"""Native review admission and reservation use isolated Studio databases."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location('review_fixture', Path(__file__).with_name('workspace-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_agent_review import claim, request, validate


class AgentReviewContract(unittest.TestCase):
    tearDown = fixture.WorkspaceContract.tearDown
    lead = fixture.WorkspaceContract.lead
    agent_update = fixture.WorkspaceContract.agent_update
    events = fixture.WorkspaceContract.events

    def setUp(self):
        fixture.WorkspaceContract.setUp(self)
        self.actor = self.runtime.prepare(self.lead())
        self.actor = self.agent_update(self.actor, status='running', turnId='actor-turn')
        self.key = self.actor['threadId'] + ':review:stable'

    def children(self):
        with self.runtime.db() as db:
            return [a for a in self.runtime.records(db, 'agents') if a.get('parentId') == self.actor['id']]

    def make(self, args=None):
        return request(self.runtime, self.actor, args or {}, self.key)

    def message(self, call='review-1', target=None):
        return {'id': call, 'method': 'item/tool/call', 'params': {
            'threadId': self.actor['threadId'], 'turnId': self.actor['turnId'], 'callId': call,
            'tool': 'orchestration_review', 'arguments': {'request_id': 'stable',
                'target': target or {'type': 'uncommittedChanges'}}}}

    def test_dynamic_lost_reply_recovers_exact_child_across_calls(self):
        message = self.message()
        with patch.object(self.runtime.server, 'write', side_effect=BrokenPipeError('Reply lost')):
            self.runtime.dynamic(message)
        receipt = self.runtime.request_action(self.actor['id'], {'action': 'get', 'request_id': 'stable'})
        self.assertEqual(receipt['outcome'], 'applied')
        self.assertEqual(receipt['agentIds'], [self.children()[0]['id']])
        self.runtime.dynamic(self.message('review-2'))
        result = self.runtime.server.responses[-1]['result']
        self.assertTrue(result['success'], result)
        self.assertEqual(result, receipt['result'])
        self.assertEqual(len(self.children()), 1)
        self.runtime.dynamic(self.message('review-3', {'type': 'baseBranch', 'branch': 'main'}))
        self.assertFalse(self.runtime.server.responses[-1]['result']['success'])
        self.assertEqual(len(self.children()), 1)

    def test_dispatch_starts_native_review_with_target_and_no_turn_start(self):
        target = {'type': 'baseBranch', 'branch': 'main'}
        result = self.make({'target': target})
        child = self.runtime.agent(result['agentId'])
        server = self.runtime.server
        original = server.call
        def native_review(method, params, timeout=60):
            if method != 'review/start':
                return original(method, params, timeout)
            server.calls.append((method, params))
            turn = {'id': 'native-review-turn', 'status': 'inProgress'}
            server.notify({'method': 'turn/started', 'params': {'threadId': params['threadId'], 'turn': turn}})
            return {'turn': turn, 'reviewThreadId': params['threadId']}
        with patch.object(server, 'call', side_effect=native_review):
            self.runtime.dispatch()
            fixture.eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
            self.runtime.dispatch()
        calls = [params for method, params in server.calls if method == 'review/start']
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]['target'], target)
        self.assertEqual(calls[0]['delivery'], 'inline')
        self.assertFalse(any(method == 'turn/start' for method, _ in server.calls))
        prepared = [params for method, params in server.calls if method == 'thread/start'][-1]
        self.assertEqual(prepared['dynamicTools'], [])
        self.assertEqual(prepared['sandbox'], 'read-only')
        child = self.runtime.agent(child['id'])
        server.complete(child['threadId'], child['turnId'], 'Native review findings')
        fixture.eventually(lambda: self.runtime.agent(child['id'])['status'] == 'completed')
        self.assertTrue(any('Native review findings' in event['text'] for event in self.events(self.actor)))

    def test_exact_retry_and_conflict_preserve_one_child_and_receipt(self):
        args = {'request_id': 'stable', 'target': {'type': 'baseBranch', 'branch': 'main'}}
        first = self.make(args)
        self.assertEqual(first['agentId'], str(uuid.uuid5(uuid.NAMESPACE_URL, self.key)))
        self.assertEqual(self.make(args), first)
        child = self.children()[0]
        self.agent_update(child, status='completed')
        self.assertEqual(self.make(args), first)
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.make({'target': {'type': 'baseBranch', 'branch': 'other'}})
        self.assertEqual(len(self.children()), 1)
        with self.runtime.db() as db:
            result = json.loads(db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (self.key,)).fetchone()[0])
        self.assertEqual(json.loads(result['contentItems'][0]['text']), first)

    def test_inherits_actor_settings_and_directory_but_forces_read_only(self):
        self.actor = self.agent_update(self.actor, model='gpt-5.6-sol', effort='high', fastMode=True,
            workerDefaults={'model': 'gpt-5.6-luna', 'effort': 'low', 'fastMode': False}, yoloMode=True)
        self.make()
        child = self.children()[0]
        for field in ('cwd', 'accountKey', 'model', 'effort', 'fastMode'):
            self.assertEqual(child[field], self.actor[field])
        self.assertEqual(child['role'], 'reviewer')
        self.assertFalse(child['yoloMode'])
        self.assertFalse(child['worktree'])
        self.assertEqual(self.runtime.turn_permissions(child)['sandboxPolicy']['type'], 'readOnly')
        self.assertEqual(self.events(child), [])
        self.assertFalse(any(method in {'turn/start', 'review/start'} for method, _ in self.runtime.server.calls))

    def test_worker_can_request_review_without_team_defaults(self):
        worker = self.runtime.create({'name': 'Worker', 'prompt': 'Inspect', 'role': 'reviewer',
                                      'model': 'gpt-5.6-sol', 'effort': 'high'}, self.actor['id'], defer=True)
        worker = self.agent_update(worker, autoWake=True)
        result = request(self.runtime, worker, {}, self.key)
        child = self.runtime.agent(result['agentId'])
        self.assertEqual(child['parentId'], worker['id'])
        self.assertEqual(child['rootId'], self.actor['id'])
        self.assertEqual(child['model'], worker['model'])
        with self.assertRaisesRegex(ValueError, 'cannot create another'):
            request(self.runtime, child, {}, self.key + ':nested')

    def test_single_mode_and_capacity_reject_without_child(self):
        self.actor = self.agent_update(self.actor, agentMode='single')
        with self.assertRaisesRegex(ValueError, 'Single agent mode'):
            self.make()
        self.actor = self.agent_update(self.actor, agentMode='multi', maxAgents=1)
        with self.assertRaisesRegex(ValueError, 'limit'):
            self.make()
        self.assertEqual(self.children(), [])

    def test_provider_rejects_without_catalog_or_child(self):
        self.actor = self.agent_update(self.actor, provider='claude')
        with patch.object(self.runtime, 'catalog', side_effect=AssertionError('No remote call')):
            with self.assertRaisesRegex(ValueError, 'only for Codex'):
                self.make()
        self.assertEqual(self.children(), [])

    def test_pending_reviewer_cannot_change_account_or_provider(self):
        from codex_account_transfer import transfer_store
        child_id = self.make()['agentId']
        with patch.object(self.runtime.accounts, 'get', return_value={'provider': 'claude'}):
            with self.assertRaisesRegex(ValueError, 'account is fixed'):
                self.runtime.set_account(child_id, 'claude-destination')
            with self.assertRaisesRegex(ValueError, 'Choose the orchestrator'):
                transfer_store(self.runtime).request(child_id, 'claude-destination', str(uuid.uuid4()))
        child = self.runtime.agent(child_id)
        self.assertEqual(child['provider'], 'codex')
        self.assertEqual(child['accountKey'], self.actor['accountKey'])
        self.assertEqual(child['nativeReview']['status'], 'pending')

    def test_actor_changes_during_catalog_cannot_create_review(self):
        for changes in ({'epoch': 1}, {'accountKey': 'other'}, {'autoWake': False}, {'deletedAt': 1}):
            with self.subTest(changes=changes):
                before = self.runtime.agent(self.actor['id'])
                original = self.runtime.catalog
                def changed_catalog(*args):
                    result = original(*args)
                    self.agent_update(self.actor, **changes)
                    return result
                with patch.object(self.runtime, 'catalog', side_effect=changed_catalog):
                    with self.assertRaises(ValueError):
                        self.make()
                with self.runtime.lock, self.runtime.db() as db:
                    self.runtime.put(db, 'agents', before)
                self.assertEqual(self.children(), [])

    def test_cancelled_or_old_connection_receipt_prevents_child(self):
        for receipt in ({'cancelRequested': True}, {'accountKey': 'default', 'connectionId': 'obsolete'}):
            with self.subTest(receipt=receipt), patch.object(self.runtime, 'tool_request', return_value=receipt):
                with self.assertRaises(ValueError):
                    self.make()
                self.assertEqual(self.children(), [])

    def test_result_failure_rolls_back_child(self):
        with patch.object(self.runtime, 'tool_request', return_value={'accountKey': 'default',
                    'connectionId': self.runtime.connection_ids['default']}), \
                patch.object(self.runtime, 'finish_tool_request', side_effect=RuntimeError('Receipt write failed')):
            with self.assertRaisesRegex(RuntimeError, 'Receipt write failed'):
                self.make()
        self.assertEqual(self.children(), [])
        with self.runtime.db() as db:
            self.assertIsNone(db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (self.key,)).fetchone())

    def test_claim_is_once_and_has_no_input_after_restart(self):
        target = {'type': 'commit', 'sha': 'abc123', 'title': 'Fix'}
        result = self.make({'target': target})
        child = self.runtime.agent(result['agentId'])
        with self.runtime.lock, self.runtime.db() as db:
            attempt = claim(self.runtime, db, child)
            self.assertEqual(attempt['reviewTarget'], target)
            self.assertEqual(attempt['action'], 'review')
            self.assertEqual(attempt['events'], [])
            self.assertFalse(attempt['submitted'])
            self.assertEqual(child['status'], 'starting')
            self.assertIsNone(claim(self.runtime, db, child))
        self.runtime.close()
        self.runtime = fixture.ControlledRuntime(self.state, fixture.WorkspaceServer)
        child = self.runtime.agent(child['id'])
        with self.runtime.lock, self.runtime.db() as db:
            self.assertIsNone(claim(self.runtime, db, child))
        self.assertEqual(self.events(child), [])
        self.assertEqual(self.make({'target': target}), result)

    def test_stop_and_stale_selection_do_not_claim(self):
        self.make()
        child = self.children()[0]
        self.agent_update(child, autoWake=False, status='paused', epoch=1)
        with self.runtime.lock, self.runtime.db() as db:
            self.assertIsNone(claim(self.runtime, db, child))
        self.agent_update(child, autoWake=True, status='queued', epoch=1)
        with self.runtime.lock, self.runtime.db() as db:
            self.assertIsNone(claim(self.runtime, db, child))
            fresh = self.runtime.agent(child['id'], db)
            self.assertIsNotNone(claim(self.runtime, db, fresh))

    def test_target_validation_rejects_mixed_or_missing_fields(self):
        self.assertEqual(validate({}), {'type': 'uncommittedChanges'})
        self.assertEqual(validate({'target': {'type': 'custom', 'instructions': 'Inspect all callers'}}),
                         {'type': 'custom', 'instructions': 'Inspect all callers'})
        invalid = [None, [], {'extra': True}, {'request_id': ' bad'}, {'request_id': 'x' * 201},
                   {'target': None}, {'target': {'type': 'unknown'}}, {'target': {'type': 'baseBranch'}},
                   {'target': {'type': 'commit', 'sha': ''}}, {'target': {'type': 'custom', 'instructions': False}},
                   {'target': {'type': 'uncommittedChanges', 'branch': 'main'}},
                   {'target': {'type': 'baseBranch', 'branch': 'main', 'instructions': 'extra'}},
                   {'target': {'type': 'commit', 'sha': 'abc', 'title': 7}}]
        for args in invalid:
            with self.subTest(args=args), self.assertRaises(ValueError):
                validate(args)


if __name__ == '__main__':
    unittest.main()

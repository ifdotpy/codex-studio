#!/usr/bin/env python3
"""Behavioral orchestration tests. No model service, no user state."""
import base64
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime


def eventually(predicate, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(.01)
    raise AssertionError('Condition did not become true')


class FakeServer:
    def __init__(self, root, notify, request, died):
        self.notify, self.request, self.died = notify, request, died
        self.calls, self.responses = [], []
        self.seq = 0
        self.gate = threading.Event()
        self.closed = False
        self.fail_start = False
        self.finish_before_reply = False
        self.start_gate = None

    def call(self, method, params, timeout=60):
        self.calls.append((method, params))
        if method == 'config/read':
            # Fake commands have no host shell setup. Native parity is covered
            # by monitor-shell-native.py with snapshots both enabled and disabled.
            return {'config': {'features': {'shell_snapshot': False}}}
        if method in ('thread/start', 'thread/resume'):
            self.seq += 1
            assert params['config']['features.multi_agent'] is False
            if method == 'thread/start':
                assert all(t['type'] == 'function' for t in params['dynamicTools'])
            return {'thread': {'id': params.get('threadId', f'thread-{self.seq}')}, 'model': params.get('model', 'gpt-6-astra'),
                    'sandbox': {'type': 'readOnly'}, 'approvalPolicy': 'on-request'}
        if method == 'turn/start':
            if self.start_gate is not None:
                self.start_gate.wait(5)
            if self.fail_start:
                raise RuntimeError('response timed out; outcome unknown')
            self.seq += 1
            turn = {'id': f'turn-{self.seq}', 'status': 'inProgress'}
            self.notify({'method': 'turn/started', 'params': {'threadId': params['threadId'], 'turn': turn}})
            if self.finish_before_reply:
                self.complete(params['threadId'], turn['id'])
            return {'turn': turn}
        if method == 'command/exec':
            assert params['sandboxPolicy']['type'] in {'readOnly', 'workspaceWrite', 'dangerFullAccess'}
            self.notify({'method': 'command/exec/outputDelta', 'params': {'processId': params['processId'],
                'deltaBase64': base64.b64encode(b'early output\n').decode(), 'stream': 'stdout'}})
            self.gate.wait(5)
            return {'exitCode': 7, 'stdout': '', 'stderr': ''}
        if method == 'command/exec/terminate':
            self.gate.set()
            return {}
        if method == 'turn/interrupt':
            self.notify({'method': 'turn/completed', 'params': {'threadId': params['threadId'],
                'turn': {'id': params['turnId'], 'status': 'interrupted'}}})
            return {}
        if method == 'account/rateLimits/read':
            return {'rateLimits': {'limitId': 'codex', 'primary': {'usedPercent': 42, 'windowDurationMins': 300, 'resetsAt': 2000000000}}}
        if method == 'model/list':
            return {'data': [{'model': model, 'defaultReasoningEffort': 'medium', 'supportedReasoningEfforts': [{'reasoningEffort': effort} for effort in ['low', 'medium', 'high', 'xhigh', 'max', 'ultra']], 'serviceTiers': [{'id': 'priority'}]} for model in ['test-model', 'gpt-6-astra', 'gpt-5.6-sol', 'gpt-5.6-luna', 'gpt-5.6-terra']]}
        raise AssertionError(method)

    def write(self, value):
        self.responses.append(value)

    def submit(self, method, params):
        import concurrent.futures
        future = concurrent.futures.Future()
        def work():
            try: future.set_result(self.call(method, params))
            except Exception as error: future.set_exception(error)
        threading.Thread(target=work, daemon=True).start()
        return future

    def wait(self, future, timeout=60):
        return future.result(timeout)

    def complete(self, tid, turn, text='Result with evidence'):
        self.notify({'method': 'item/completed', 'params': {'threadId': tid,
            'item': {'id': turn + '-answer', 'type': 'agentMessage', 'text': text}}})
        self.notify({'method': 'turn/completed', 'params': {'threadId': tid,
            'turn': {'id': turn, 'status': 'completed'}}})

    def close(self):
        self.closed = True
        self.gate.set()


class RuntimeContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.runtime = Runtime(self.root, FakeServer)

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def lead(self, **extra):
        a = self.runtime.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': 'Coordinate the work', **extra})
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        return self.runtime.agent(a['id'])

    def complete(self, a):
        self.runtime.server.complete(a['threadId'], a['turnId'])

    def test_empty_current_chat_reuses_identity_even_after_restart(self):
        import uuid
        first = self.runtime.new_lead({})
        request = {'id': str(uuid.uuid4()), 'previous': first['id']}
        with ThreadPoolExecutor(max_workers=4) as pool:
            results = list(pool.map(lambda _: self.runtime.new_lead(request), range(8)))
        self.assertEqual({a['id'] for a in results}, {first['id']})
        self.assertEqual(len(self.runtime.snapshot()['agents']), 1)
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.runtime.new_lead(request)['id'], first['id'])
        self.runtime.send(first['id'], 'Start this task')
        second = self.runtime.new_lead({'previous': first['id']})
        self.assertNotEqual(second['id'], first['id'])
        self.assertEqual(self.runtime.new_lead(request)['id'], first['id'])

    def test_chat_delivers_to_finished_parent_and_peer_and_enforces_privacy(self):
        lead = self.lead()
        children = [self.runtime.create({'name': str(i), 'prompt': 'Review', 'role': 'reviewer'}, lead['id'], defer=True) for i in range(3)]
        self.complete(lead)
        with self.runtime.lock:
            with self.runtime.db() as db:
                for child in children[:2]:
                    child.update(status='completed', autoWake=True)
                    self.runtime.put(db, 'agents', child)
            first, peer, outsider = children
            directory = self.runtime.peers(first['id'])
            self.assertEqual(len(directory['peers']), 4)
            parent = self.runtime.chat_message(first['id'], 'parent', 'Found the cause', 'msg-parent', 0)
            private = self.runtime.chat_message(first['id'], peer['id'], 'Check this symbol', 'msg-private', 0)
            self.assertEqual(parent['deliveries'], {lead['id']: 'queued'})
            self.assertEqual(private['deliveries'], {peer['id']: 'queued'})
            self.assertEqual(self.runtime.agent(lead['id'])['status'], 'queued')
            self.assertEqual(self.runtime.agent(peer['id'])['status'], 'queued')
            self.assertEqual(self.runtime.chat_read(private['room'], peer['id'])['messages'][0]['sender'], first['id'])
            with self.assertRaisesRegex(ValueError, 'not a participant'):
                self.runtime.chat_read(private['room'], outsider['id'])
            self.assertEqual(len(self.runtime.chat_read(private['room'])['messages']), 1, 'user can inspect private chat')
            self.assertEqual(self.runtime.chat_message(first['id'], peer['id'], 'Check this symbol', 'msg-private', 0), private)
            with self.assertRaisesRegex(ValueError, 'different content'):
                self.runtime.chat_message(first['id'], peer['id'], 'Changed', 'msg-private', 0)
            with self.runtime.db() as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE kind='agent_message'").fetchone()[0], 2)
        eventually(lambda: self.runtime.agent(peer['id'])['status'] == 'running')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        inputs = [p['input'][0]['text'] for method, p in self.runtime.server.calls if method == 'turn/start']
        self.assertTrue(any('Found the cause' in t and 'agent_message' in t for t in inputs))
        self.assertTrue(any('Check this symbol' in t for t in inputs))

    def test_broadcast_scopes_stop_boundary_pagination_and_restart(self):
        lead = self.lead()
        peer = self.runtime.create({'name': 'Peer', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'], defer=True)
        other = self.lead(name='Other lead')
        blank = self.runtime.new_lead({})
        with self.runtime.lock:
            team = self.runtime.chat_message(lead['id'], 'broadcast', 'Team update', 'broadcast-team')
            self.assertEqual(team['deliveries'], {peer['id']: 'stored_only'})
            all_teams = self.runtime.chat_message(lead['id'], 'all', 'Shared finding', 'broadcast-all')
            self.assertEqual(all_teams['deliveries'][other['id']], 'queued')
            self.assertEqual(all_teams['deliveries'][blank['id']], 'stored_only')
            self.assertEqual(self.runtime.agent(peer['id'])['status'], 'paused')
            with self.assertRaisesRegex(ValueError, 'not a participant'):
                self.runtime.chat_read(team['room'], other['id'])
            for i in range(105):
                self.runtime.chat_message(lead['id'], peer['id'], f'Finding {i}', f'page-{i}')
            room = self.runtime.peers(peer['id'])['rooms'][0]
            newest = self.runtime.chat_read(room['id'], peer['id'])
            older = self.runtime.chat_read(room['id'], peer['id'], newest['nextBefore'])
            self.assertEqual(len(newest['messages']), 100)
            self.assertEqual(len(older['messages']), 5)
            self.assertIsNone(older['nextBefore'])
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(len(self.runtime.chat_read(room['id'], peer['id'])['messages']), 100)

    def test_chat_tools_are_exposed_and_dispatch_from_a_worker(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Peer', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        for i, (name, args) in enumerate([
            ('orchestration_message', {'target': 'lead', 'text': 'Progress before final'}),
            ('orchestration_peers', {}),
            ('orchestration_send', {'agent_id': 'parent', 'text': 'Old schema works'}),
        ]):
            self.runtime.dynamic({'id': 700+i, 'params': {'threadId': child['threadId'], 'callId': str(i), 'tool': name, 'arguments': args}})
            reply = next(r for r in self.runtime.server.responses if r['id'] == 700+i)
            self.assertTrue(reply['result']['success'], reply)
        room = self.runtime.peers(child['id'])['rooms'][0]
        self.runtime.dynamic({'id': 704, 'params': {'threadId': child['threadId'], 'callId': 'read',
            'tool': 'orchestration_chat_read', 'arguments': {'room_id': room['id']}}})
        self.assertTrue(next(r for r in self.runtime.server.responses if r['id'] == 704)['result']['success'])
        params = next(p for method, p in self.runtime.server.calls if method == 'thread/start')
        self.assertTrue({'orchestration_message', 'orchestration_peers', 'orchestration_chat_read'} <= {t['name'] for t in params['dynamicTools']})

    def test_deleted_worker_does_not_consume_team_capacity(self):
        lead = self.lead(maxAgents=2)
        child = self.runtime.create({'name': 'Old worker', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'], defer=True)
        self.runtime.delete_conversation(child['id'])
        replacement = self.runtime.create({'name': 'New worker', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'], defer=True)
        self.assertNotEqual(child['id'], replacement['id'])
        self.assertEqual(len(self.runtime.team(lead['id'])['agents']), 2)

    def test_delete_stops_tree_and_rejects_retry_or_late_wakeup(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Peer', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        message = self.runtime.chat_message(child['id'], 'parent', 'Progress', 'delete-message')
        deleted = self.runtime.delete_conversation(lead['id'])
        self.assertEqual(set(deleted['deleted']), {lead['id'], child['id']})
        self.assertEqual(self.runtime.snapshot()['agents'], [])
        self.assertEqual(self.runtime.snapshot()['rooms'], [])
        self.runtime.server.complete(child['threadId'], child['turnId'], 'Late result')
        self.assertFalse(self.runtime.agent(lead['id'])['autoWake'])
        with self.assertRaisesRegex(ValueError, 'deleted'):
            self.runtime.send(lead['id'], 'Replay')
        self.assertEqual(self.runtime.delete_conversation(lead['id'])['deleted'], deleted['deleted'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_chat_messages').fetchone()[0], 1)
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.runtime.snapshot()['agents'], [])

    def test_complaint_message_allows_direct_response_then_notifies_reporter(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Reporter', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        c = self.runtime.complaint(child['id'], {'action': 'submit', 'text': 'The build log is missing. Cannot verify the result.'}, 'complaint-1', 0)
        self.assertEqual(self.runtime.complaint(child['id'], {'action': 'submit', 'text': c['text']}, 'complaint-1', 0)['id'], c['id'])
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.runtime.complaint(child['id'], {'action': 'submit', 'text': 'Changed'}, 'complaint-1', 0)
        self.runtime.complaint(child['id'], {'action': 'read'}, 'worker-read', 0)
        self.assertIsNone(self.runtime.complaint_detail(c['id'])['readAt'], 'worker and UI reads are not lead reads')
        response = {'action': 'respond', 'complaint_id': c['id'], 'text': 'I attached the log and checked the failed test.', 'status': 'resolved'}
        with self.assertRaisesRegex(ValueError, 'responsible lead'):
            self.runtime.complaint(child['id'], response, 'forged', 0)
        self.complete(lead)
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        prompt = [p for method, p in self.runtime.server.calls if method == 'turn/start' and p['threadId'] == lead['threadId']][-1]['input'][0]['text']
        self.assertEqual(prompt.count(c['text']), 1, 'deliver the full complaint once, not an ID-only reminder')
        self.assertIn(c['id'], prompt)
        self.assertIn(child['id'], prompt)
        self.assertIn('Reporter', prompt)
        self.assertNotIn('Read the complaint book', prompt)
        first = self.runtime.complaint(lead['id'], response, 'response', 0)
        self.assertIsNotNone(first['readAt'], 'a direct response confirms receipt without a separate read tool call')
        self.assertEqual(self.runtime.complaint(lead['id'], response, 'response', 0), first)
        self.assertEqual(len(first['responses']), 1)
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE agent=? AND kind='complaint_response'", (child['id'],)).fetchone()[0], 1)
        self.assertFalse(self.runtime.snapshot()['complaints'][0]['needsResponse'])
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.runtime.complaint_detail(c['id'])['status'], 'resolved')

    def test_complaint_wakes_finished_lead_and_blocks_false_completion(self):
        lead = self.lead()
        self.complete(lead)
        c = self.runtime.complaint(lead['id'], {'action': 'submit', 'text': 'Do not ignore this broken check.'}, 'user-complaint', user=True)
        for index in range(3):
            eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
            prompt = [p for method, p in self.runtime.server.calls if method == 'turn/start'][-1]['input'][0]['text']
            self.assertEqual(prompt.count(c['text']), 1)
            self.assertIn('"author": "user"', prompt)
            self.complete(self.runtime.agent(lead['id']))
        stopped = self.runtime.agent(lead['id'])
        self.assertFalse(stopped['autoWake'])
        self.assertEqual(stopped['status'], 'failed')
        self.assertIn('Three turns', stopped['error'])
        self.assertTrue(self.runtime.snapshot()['complaints'][0]['needsResponse'])
        self.runtime.send(lead['id'], 'Read the complaint and act')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        a = self.runtime.agent(lead['id'])
        self.runtime.complaint(a['id'], {'action':'respond','complaint_id':c['id'],'text':'I will recover the missing log before the next review.','status':'in_progress'}, 'act', a['epoch'])
        self.complete(a)
        self.assertEqual(self.runtime.agent(a['id'])['status'], 'completed')

    def test_no_complaint_book_check_for_normal_turns(self):
        lead = self.lead()
        first = [p for method, p in self.runtime.server.calls if method == 'turn/start'][-1]['input'][0]['text']
        self.assertNotIn('complaint', first.lower())
        self.complete(lead)
        self.runtime.send(lead['id'], 'Continue the work')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        second = [p for method, p in self.runtime.server.calls if method == 'turn/start'][-1]['input'][0]['text']
        self.assertNotIn('complaint', second.lower())
        self.complete(self.runtime.agent(lead['id']))
        self.assertEqual(self.runtime.agent(lead['id'])['status'], 'completed')
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE kind='complaint'").fetchone()[0], 0)

    def test_complaint_honors_stop_and_old_thread_tool_route(self):
        a = self.lead()
        self.runtime.dynamic({'id':900,'params':{'threadId':a['threadId'],'callId':'old-complaint','tool':'orchestration_send',
            'arguments':{'agent_id':'complaint','text':json.dumps({'action':'submit','text':'A concrete old-thread problem'})}}})
        self.assertTrue(next(r for r in self.runtime.server.responses if r['id']==900)['result']['success'])
        self.runtime.stop(a['id'])
        self.runtime.complaint(a['id'], {'action':'submit','text':'User complaint while stopped'}, 'stopped-complaint', user=True)
        self.assertFalse(self.runtime.agent(a['id'])['autoWake'])
        self.assertEqual(len(self.runtime.snapshot()['complaints']), 2)

    def test_context_metrics_compaction_duplicates_and_limits(self):
        a = self.lead()
        self.runtime.notification({'method':'thread/tokenUsage/updated','params':{'threadId':a['threadId'],
            'tokenUsage':{'total':{'totalTokens':1000000},'last':{'totalTokens':80000},'modelContextWindow':200000}}})
        measured = self.runtime.agent(a['id'])
        self.assertEqual(measured['contextUsage']['tokens'], 80000)
        self.assertEqual(measured['contextUsage']['window'], 200000)
        message = {'method':'item/completed','params':{'threadId':a['threadId'],'item':{'id':'compact-1','type':'contextCompaction'}}}
        self.runtime.notification(message)
        self.runtime.notification(message)
        self.assertEqual(self.runtime.agent(a['id'])['compactions'], 1)
        self.assertIsNone(self.runtime.agent(a['id'])['contextUsage'])
        self.runtime.rate_limits['data'] = {'accountId': 'test-account', 'rateLimitResetCredits': {'availableCount': 3, 'credits': []}}
        self.runtime.notification({'method':'account/rateLimits/updated','params':{'rateLimits':{'limitId':'codex','primary':{'usedPercent':42,'windowDurationMins':300}}}})
        self.assertEqual(self.runtime.rate_limits['data']['accountId'], 'test-account')
        self.assertEqual(self.runtime.rate_limits['data']['rateLimitResetCredits']['availableCount'], 3)
        self.assertEqual(self.runtime.snapshot()['rateLimits']['data']['rateLimits']['primary']['usedPercent'], 42)
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.runtime.agent(a['id'])['compactions'], 1)

    def test_sidebar_rename_and_room_hide_preserve_agent_history(self):
        a = self.lead()
        b = self.lead(name='Another lead')
        room = self.runtime.chat_message(a['id'], b['id'], 'First message', 'rename-first')['room']
        self.runtime.rename(a['id'], 'Manual title')
        self.runtime.dynamic({'id':901,'params':{'threadId':a['threadId'],'callId':'late-title','tool':'orchestration_title','arguments':{'title':'LLM title'}}})
        self.assertEqual(self.runtime.agent(a['id'])['name'], 'Manual title')
        self.runtime.rename(room, 'Review chat')
        self.runtime.hide_room(room)
        self.assertEqual(self.runtime.snapshot()['rooms'], [])
        self.assertEqual(len(self.runtime.chat_read(room,a['id'])['messages']), 1)
        self.runtime.chat_message(a['id'], b['id'], 'New message', 'rename-next')
        self.assertEqual(self.runtime.snapshot()['rooms'][0]['name'], 'Review chat')
        self.assertEqual(self.runtime.snapshot()['rooms'][0]['lastMessage']['text'], 'New message')

    def test_blank_lead_is_persistent_and_has_no_model_call(self):
        import uuid
        key = str(uuid.uuid4())
        a = self.runtime.new_lead({'id': key})
        self.assertTrue(a['isLead'])
        self.assertEqual(a['model'], 'gpt-6-astra')
        self.assertEqual(a['status'], 'idle')
        self.assertEqual(a['prompt'], '')
        self.assertIsNone(self.runtime.server)
        self.assertEqual(self.runtime.new_lead({'id': key})['id'], key)
        with self.assertRaisesRegex(ValueError, 'different settings'):
            self.runtime.new_lead({'id': key, 'model': 'gpt-5.6-sol'})
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertTrue(self.runtime.agent(key)['isLead'])
        self.assertEqual(self.runtime.agent(key)['status'], 'idle')
        self.assertIsNone(self.runtime.server)

    def test_lead_model_and_role_are_server_enforced(self):
        for model in ['gpt-5.6-luna', 'gpt-5.6-terra', 'fake-sol']:
            with self.assertRaisesRegex(ValueError, 'Astra or Sol'):
                self.runtime.new_lead({'model': model})
            with self.assertRaisesRegex(ValueError, 'Astra or Sol'):
                self.runtime.create({'cwd': str(self.root), 'prompt': 'Task', 'model': model})
        a = self.runtime.new_lead({'model': 'gpt-5.6-sol'})
        with self.assertRaisesRegex(ValueError, 'Astra or Sol'):
            self.runtime.conversation_settings(a['id'], {'model': 'gpt-5.6-luna'})
        reviewer = self.runtime.create({'cwd': str(self.root), 'prompt': 'Review', 'role': 'reviewer', 'model': 'gpt-5.6-luna'}, defer=True)
        self.assertFalse(reviewer['isLead'])
        with self.assertRaisesRegex(ValueError, 'Select a lead'):
            self.runtime.new_lead({'previous': reviewer['id']})
        with self.assertRaisesRegex(ValueError, 'Invalid agent role'):
            self.runtime.create({'prompt': 'Task', 'role': 'orchestrator'}, parent=a['id'])

    def test_subagent_model_change_resumes_same_thread_and_keeps_permissions(self):
        lead = self.runtime.new_lead({'yolo_mode': False})
        worker = self.runtime.create({'prompt': 'Review', 'role': 'reviewer', 'effort': 'ultra'}, parent=lead['id'], defer=True)
        worker = self.runtime.prepare(worker)
        self.runtime.catalog = lambda account: {'data': [{
            'model': 'test-model', 'defaultReasoningEffort': 'low',
            'supportedReasoningEfforts': [{'reasoningEffort': 'low'}]}]}
        changed = self.runtime.conversation_settings(worker['id'], {'id': worker['id'], 'model': 'test-model'})
        self.assertEqual(changed['model'], 'test-model')
        self.assertEqual(changed['effort'], 'low')
        for field in ['threadId', 'rootId', 'parentId', 'role', 'cwd', 'accountKey', 'approvalPolicy', 'sandbox']:
            self.assertEqual(changed[field], worker[field])
        self.assertNotIn(worker['id'], self.runtime.loaded)
        self.runtime.send(worker['id'], 'Continue the review')
        eventually(lambda: self.runtime.agent(worker['id'])['status'] == 'running')
        resumed = [params for method, params in self.runtime.server.calls if method == 'thread/resume'][-1]
        self.assertEqual(resumed['threadId'], worker['threadId'])
        self.assertEqual(resumed['model'], 'test-model')
        self.assertEqual(resumed['sandbox'], 'read-only')
        turns = [params for method, params in self.runtime.server.calls if method == 'turn/start']
        self.assertEqual(turns[-1]['effort'], 'low')

    def test_subagent_model_rejects_unavailable_and_privileged_settings(self):
        lead = self.runtime.new_lead({})
        worker = self.runtime.create({'prompt': 'Review', 'role': 'reviewer'}, parent=lead['id'], defer=True)
        for extra in [{'cwd': str(self.root)}, {'dangerously_skip_rules': True}, {'isLead': True}]:
            with self.assertRaisesRegex(ValueError, 'Only a lead'):
                self.runtime.conversation_settings(worker['id'], {'model': 'test-model', **extra})
        for model in ['', None, [], 'missing-model']:
            with self.assertRaises(ValueError):
                self.runtime.conversation_settings(worker['id'], {'model': model})
        self.runtime.catalog = lambda account: {'data': [{'model': 'hidden', 'hidden': True}]}
        with self.assertRaisesRegex(ValueError, 'not available'):
            self.runtime.conversation_settings(worker['id'], {'model': 'hidden'})
        self.assertEqual(self.runtime.agent(worker['id'])['model'], worker['model'])
        self.assertFalse(self.runtime.agent(worker['id'])['dangerouslySkipAccountRules'])
        self.assertFalse(self.runtime.agent(worker['id'])['isLead'])

    def test_subagent_model_rechecks_active_turn_after_catalog_read(self):
        worker = self.runtime.create({'cwd': str(self.root), 'prompt': 'Review', 'role': 'reviewer'}, defer=True)
        def catalog(account):
            self.assertEqual(account, worker['accountKey'])
            with self.runtime.lock, self.runtime.db() as db:
                current = self.runtime.agent(worker['id'], db)
                current.update(status='starting', inFlight=True)
                self.runtime.put(db, 'agents', current)
            return {'data': [{'model': 'test-model'}]}
        self.runtime.catalog = catalog
        with self.assertRaisesRegex(ValueError, 'Wait for this turn'):
            self.runtime.conversation_settings(worker['id'], {'model': 'test-model'})
        self.assertEqual(self.runtime.agent(worker['id'])['model'], worker['model'])

    def test_model_generates_title_and_current_time_request_is_answered(self):
        a = self.runtime.new_lead({})
        self.runtime.send(a['id'], 'Review the release')
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        a = self.runtime.agent(a['id'])
        self.runtime.server.request({'id': 210, 'method': 'item/tool/call', 'params': {
            'threadId': a['threadId'], 'callId': 'title', 'tool': 'orchestration_title',
            'arguments': {'title': 'Release review'}}})
        eventually(lambda: self.runtime.agent(a['id'])['name'] == 'Release review')
        self.assertFalse(self.runtime.agent(a['id'])['needsTitle'])
        self.runtime.server.request({'id': 211, 'method': 'currentTime/read', 'params': {'threadId': a['threadId']}})
        reply = next(r for r in self.runtime.server.responses if r['id'] == 211)
        self.assertLess(abs(reply['result']['currentTimeAt'] - time.time()), 2)
        self.assertEqual(self.runtime.snapshot()['requests'], [])
        params = next(p for method, p in self.runtime.server.calls if method == 'thread/start')
        self.assertFalse(params['config']['agents.enabled'])
        self.assertFalse(params['config']['features.multi_agent_v2'])

    def test_async_question_can_resume_a_finished_agent(self):
        a = self.lead()
        self.runtime.notification({'method': 'item/completed', 'params': {'threadId': a['threadId'],
            'item': {'type': 'agentMessage', 'id': 'question-1', 'text': 'Choose a scope',
                     'questions': [{'title': 'Choose a scope', 'options': ['One file', 'All files']}]}}})
        self.complete(a)
        question = self.runtime.snapshot()['requests'][0]
        self.assertEqual(question['method'], 'agent/asyncQuestion')
        self.assertEqual(self.runtime.agent(a['id'])['status'], 'completed')
        self.runtime.answer(question['id'], {'answers': {'0': {'answers': ['One file']}}})
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        self.assertEqual(self.runtime.snapshot()['requests'], [])
        self.assertIn('One file', self.runtime.transcript(a['id'])['items'][-1]['text'])

    def test_lead_migration_does_not_promote_standalone_workers(self):
        a = self.lead()
        self.complete(a)
        b = self.runtime.create({'cwd': str(self.root), 'prompt': 'Review', 'role': 'reviewer'}, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            for key in (a['id'], b['id']):
                row = self.runtime.agent(key, db)
                row.pop('isLead')
                self.runtime.put(db, 'agents', row)
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertTrue(self.runtime.agent(a['id'])['isLead'])
        self.assertFalse(self.runtime.agent(b['id'])['isLead'])

    def test_live_phases_streaming_stop_and_old_turn_events(self):
        a = self.lead()
        def event(method, **params):
            self.runtime.notification({'method': method, 'params': {'threadId': a['threadId'], 'turnId': a['turnId'], **params}})
        event('item/started', item={'id': 'reason', 'type': 'reasoning'})
        self.assertEqual(self.runtime.agent(a['id'])['activity']['phase'], 'thinking')
        event('item/started', item={'id': 'text', 'type': 'agentMessage', 'text': ''})
        event('item/agentMessage/delta', itemId='text', delta='First paragraph.\n\nUnfinished')
        transcript = self.runtime.transcript(a['id'])
        self.assertEqual(transcript['agent']['activity']['phase'], 'writing')
        self.assertTrue(transcript['items'][-1]['streaming'])
        event('item/started', item={'id': 'cmd', 'type': 'commandExecution', 'command': 'test', 'status': 'inProgress'})
        event('item/commandExecution/outputDelta', itemId='cmd', delta='Live output\n')
        transcript = self.runtime.transcript(a['id'])
        self.assertEqual(transcript['agent']['activity']['phase'], 'tool')
        self.assertIn('Live output', transcript['items'][-1]['text'])
        self.assertEqual(transcript['items'][-1]['toolStatus'], 'running')
        event('item/completed', item={'id':'cmd', 'type':'commandExecution', 'command':'test', 'status':'completed', 'exitCode':7})
        self.assertEqual(self.runtime.transcript(a['id'])['items'][-1]['toolStatus'], 'failed')
        self.runtime.stop(a['id'])
        self.assertFalse(next(i for i in self.runtime.transcript(a['id'])['items'] if i['id'].endswith(':text'))['streaming'])
        self.runtime.send(a['id'], 'Resume')
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        current = self.runtime.agent(a['id'])['activity'].copy()
        event('item/agentMessage/delta', itemId='old-text', delta='Late old turn')
        self.assertEqual(self.runtime.agent(a['id'])['activity'], current)
        self.assertFalse(any(i['id'].endswith(':old-text') for i in self.runtime.transcript(a['id'])['items']))

    def test_transcript_wait_wakes_on_committed_change_and_closes(self):
        a = self.lead()
        revision, initial = self.runtime.wait_transcript(a['id'], -1)
        self.assertEqual(initial['agent']['id'], a['id'])
        self.assertEqual(self.runtime.wait_transcript(a['id'], revision, .01), (revision, None))
        with ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(self.runtime.wait_transcript, a['id'], revision, 2)
            self.runtime.send(a['id'], 'Visible queued text')
            next_revision, update = future.result(2)
        self.assertGreater(next_revision, revision)
        self.assertTrue(any(i['text'] == 'Visible queued text' for i in update['items']))
        self.runtime.close()
        self.assertEqual(self.runtime.wait_transcript(a['id'], next_revision), (None, None))

    def test_turn_transcript_preserves_user_and_event_sources(self):
        a = self.lead()
        user_text = '[Orchestration event: agent_message]\nThis is literal user text.'
        self.runtime.send(a['id'], user_text)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.enqueue(db, self.runtime.agent(a['id'], db), 'agent_message', '{"text":"Worker result"}')
        self.complete(a)
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        record = self.runtime.transcript(a['id'])['items'][-1]
        self.assertEqual([r['kind'] for r in record['inputs']], ['user', 'agent_message'])
        self.assertEqual(record['inputs'][0]['text'], user_text)
        self.assertEqual(record['inputs'][1]['text'], '{"text":"Worker result"}')
        self.assertIn(user_text, record['text'])
        self.assertIn('[Orchestration event: agent_message]\n{"text":"Worker result"}', record['text'])

    def test_pending_user_message_is_visible_before_next_turn(self):
        a = self.lead()
        self.runtime.send(a['id'], 'Queued followup')
        messages = self.runtime.transcript(a['id'])['items']
        self.assertTrue(any(i.get('pending') and i['text'] == 'Queued followup' for i in messages))

    def test_forty_children_respect_limit_and_wake_finished_parent(self):
        lead = self.lead(concurrency=5)
        self.runtime.server.request({'id': 100, 'method': 'item/tool/call', 'params': {
            'threadId': lead['threadId'], 'callId': 'batch', 'tool': 'orchestration_spawn',
            'arguments': {'agents': [{'name': f'Review {i}', 'prompt': f'Review file {i}', 'role': 'reviewer'} for i in range(40)]}}})
        eventually(lambda: bool(self.runtime.server.responses))
        self.assertTrue(self.runtime.server.responses[0]['result']['success'])
        eventually(lambda: len(self.runtime.snapshot()['agents']) == 41)
        self.complete(lead)
        peak = 0
        for _ in range(1000):
            agents = self.runtime.snapshot()['agents']
            running = [a for a in agents if a['status'] in ('running', 'starting', 'approval')]
            peak = max(peak, len(running))
            self.assertLessEqual(len(running), 5)
            for a in running:
                if a['status'] == 'running':
                    self.complete(a)
            if all(a['status'] == 'completed' for a in self.runtime.snapshot()['agents']):
                break
            time.sleep(.01)
        else:
            self.fail('Team did not finish')
        self.assertGreaterEqual(peak, 2)
        starts = [p for m,p in self.runtime.server.calls if m == 'turn/start' and p['threadId'] == lead['threadId']]
        self.assertGreater(len(starts), 1)
        text = '\n'.join(p['input'][0]['text'] for p in starts[1:])
        self.assertIn('child_result', text)
        self.assertIn('Review 39', text)
        self.assertIn('Result with evidence', text)

    def test_monitor_output_does_not_call_model_and_exit_wakes_parent(self):
        lead = self.lead()
        self.complete(lead)
        before = sum(m == 'turn/start' for m,p in self.runtime.server.calls)
        m = self.runtime.monitor(lead['id'], {'command': 'example-command'}, approved=True)
        eventually(lambda: bool(self.runtime.snapshot()['monitors'][0]['tail']))
        time.sleep(.12)
        self.assertEqual(sum(method == 'turn/start' for method,p in self.runtime.server.calls), before)
        self.runtime.server.gate.set()
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        watch = self.runtime.snapshot()['monitors'][0]
        self.assertEqual(watch['exitCode'], 7)
        self.assertEqual(watch['status'], 'failed')
        self.assertEqual(Path(watch['log']).read_text(), 'early output\n')
        turns = [p for method,p in self.runtime.server.calls if method == 'turn/start']
        self.assertIn('monitor_exit', turns[-1]['input'][0]['text'])

    def test_stop_cancels_descendants_and_suppresses_late_wake(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        self.runtime.stop(lead['id'])
        before = len([m for m,p in self.runtime.server.calls if m == 'turn/start'])
        self.complete(child)
        time.sleep(.15)
        self.assertEqual(before, len([m for m,p in self.runtime.server.calls if m == 'turn/start']))
        self.assertTrue(all(not a['autoWake'] for a in self.runtime.snapshot()['agents']))
        self.runtime.send(lead['id'], 'Resume only the lead')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        self.assertFalse(self.runtime.agent(child['id'])['autoWake'])

    def test_duplicate_completion_delivers_once(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        self.complete(child)
        self.complete(child)
        events = [e for e in self.runtime.snapshot()['events'] if e['kind'] == 'child_result']
        self.assertEqual(len(events), 1)

    def test_restart_keeps_pending_events_without_replaying_unknown_work(self):
        lead = self.lead()
        self.runtime.send(lead['id'], 'Pending result', 'stable-event')
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.runtime.agent(lead['id'])['status'], 'interrupted')
        self.assertIsNone(self.runtime.server)
        self.assertEqual(len([e for e in self.runtime.snapshot()['events'] if e['id'] == 'stable-event']), 1)
        self.runtime.send(lead['id'], 'Review interrupted work and continue')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        self.assertTrue(any(m == 'thread/resume' for m,p in self.runtime.server.calls))

    def test_only_one_runtime_owns_state(self):
        with self.assertRaisesRegex(RuntimeError, 'owns'):
            Runtime(self.root, FakeServer)

    def test_command_requires_approval_and_stop_invalidates_it(self):
        lead = self.lead()
        m = self.runtime.monitor(lead['id'], {'command': 'example-command'})
        self.assertEqual(m['status'], 'approval')
        self.assertFalse(any(method == 'command/exec' for method,p in self.runtime.server.calls))
        r = self.runtime.snapshot()['requests'][0]
        self.runtime.stop(lead['id'])
        with self.assertRaisesRegex(ValueError, 'no longer pending'):
            self.runtime.answer(r['id'], {'decision': 'accept'})

    def test_completion_before_rpc_response_stays_terminal(self):
        self.runtime.connect().finish_before_reply = True
        a = self.runtime.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': 'Finish'})
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'completed')
        time.sleep(.1)
        self.assertEqual(self.runtime.agent(a['id'])['status'], 'completed')

    def test_budget_stops_team_and_new_budget_allows_explicit_resume(self):
        lead = self.lead(tokenBudget=100)
        self.runtime.notification({'method': 'thread/tokenUsage/updated', 'params': {
            'threadId': lead['threadId'], 'tokenUsage': {'total': {'totalTokens': 101}}}})
        eventually(lambda: not self.runtime.agent(lead['id'])['autoWake'])
        with self.assertRaisesRegex(ValueError, 'budget'):
            self.runtime.send(lead['id'], 'Continue')
        self.runtime.configure(lead['id'], {'tokenBudget': 1000, 'concurrency': 2})
        self.runtime.send(lead['id'], 'Continue')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')

    def test_invalid_batch_creates_no_workers(self):
        lead = self.lead(maxAgents=2)
        self.runtime.server.request({'id': 100, 'method': 'item/tool/call', 'params': {
            'threadId': lead['threadId'], 'callId': 'batch', 'tool': 'orchestration_spawn',
            'arguments': {'agents': [{'name': 'A', 'prompt': 'Review', 'role': 'reviewer'},
                                    {'name': 'B', 'prompt': 'Review', 'role': 'reviewer'}]}}})
        eventually(lambda: bool(self.runtime.server.responses))
        self.assertFalse(self.runtime.server.responses[0]['result']['success'])
        self.assertEqual(len(self.runtime.snapshot()['agents']), 1)

    def test_implementer_uses_isolated_committed_worktree(self):
        import subprocess
        repo = self.root / 'project'; repo.mkdir()
        def git(*args):
            return subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True, text=True)
        git('init'); git('config', 'user.name', 'Test'); git('config', 'user.email', 'test@example.invalid')
        (repo / 'file.txt').write_text('committed')
        git('add', 'file.txt'); git('commit', '-m', 'fixture')
        (repo / 'file.txt').write_text('parent uncommitted change')
        lead = self.lead(cwd=str(repo))
        child = self.runtime.create({'name': 'Implementer', 'prompt': 'Change file.txt'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] in ('running', 'failed'))
        child = self.runtime.agent(child['id'])
        self.assertEqual(child['status'], 'running', child['error'])
        self.assertNotEqual(child['cwd'], str(repo))
        self.assertEqual((Path(child['cwd']) / 'file.txt').read_text(), 'committed')
        self.assertEqual((repo / 'file.txt').read_text(), 'parent uncommitted change')

    def test_monitor_cancel_preserves_exit_history_and_prevents_wake(self):
        lead = self.lead(); self.complete(lead)
        m = self.runtime.monitor(lead['id'], {'command': 'example-command'}, approved=True)
        eventually(lambda: self.runtime.snapshot()['monitors'][0]['status'] == 'running')
        self.runtime.cancel_monitor(m['id'])
        time.sleep(.1)
        self.assertEqual(self.runtime.snapshot()['monitors'][0]['status'], 'cancelled')
        self.assertFalse(any(e['kind'] == 'monitor_exit' for e in self.runtime.snapshot()['events']))

    def test_managed_chat_delivery_and_offline_membership(self):
        from codex_canvas import Canvas
        import uuid
        c = Canvas(self.root); c.runtime = self.runtime
        lead = self.lead()
        room = str(uuid.uuid4())
        c.create_chat('Team', [lead['id']], room)
        with ThreadPoolExecutor(max_workers=4) as pool:
            futures = [pool.submit(c.post, room, f'Instruction {i}', str(uuid.uuid4())) for i in range(8)]
            results = [f.result(5) for f in futures]
        self.assertTrue(all(x['deliveries'][lead['id']] == 'queued' for x in results))
        reader = Canvas(self.root, read_only=True)
        self.assertTrue(any(a['id'] == lead['id'] for a in reader.threads()))
        self.assertEqual(len(reader.messages(room)), 8)

    def test_approval_and_question_responses_reach_codex(self):
        lead = self.lead()
        self.runtime.request({'id': 101, 'method': 'item/commandExecution/requestApproval',
                              'params': {'threadId': lead['threadId'], 'command': 'git status'}})
        request = self.runtime.snapshot()['requests'][0]
        self.runtime.answer(request['id'], {'decision': 'decline'})
        self.assertEqual(self.runtime.server.responses[-1], {'id': 101, 'result': {'decision': 'decline'}})
        self.runtime.request({'id': 102, 'method': 'item/tool/requestUserInput',
                              'params': {'threadId': lead['threadId'], 'questions': [{'id': 'q', 'question': 'Which file?'}]}})
        request = self.runtime.snapshot()['requests'][0]
        answers = {'q': {'answers': ['file.txt']}}
        self.runtime.answer(request['id'], {'answers': answers})
        self.assertEqual(self.runtime.server.responses[-1]['result'], {'answers': answers})

    def test_stop_then_resume_does_not_overlap_pending_start(self):
        lead = self.lead(); self.complete(lead)
        self.runtime.server.start_gate = threading.Event()
        self.runtime.send(lead['id'], 'Old pending instruction')
        eventually(lambda: sum(m == 'turn/start' for m,p in self.runtime.server.calls) == 2)
        self.runtime.stop(lead['id'])
        self.runtime.send(lead['id'], 'New instruction after Stop')
        time.sleep(.1)
        self.assertEqual(sum(m == 'turn/start' for m,p in self.runtime.server.calls), 2)
        self.runtime.server.start_gate.set()
        eventually(lambda: sum(m == 'turn/start' for m,p in self.runtime.server.calls) == 3)
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        calls = [p for m,p in self.runtime.server.calls if m == 'turn/start']
        self.assertEqual(calls[-1]['input'][0]['text'].split('\n\n[Time awareness, message receipt time]')[0], 'New instruction after Stop')
        self.assertIn('accepted at ', calls[-1]['input'][0]['text'])
        self.assertTrue(any(m == 'turn/interrupt' for m,p in self.runtime.server.calls))

    def test_old_turn_cannot_spawn_or_monitor_after_resume(self):
        lead = self.lead(); self.runtime.stop(lead['id'])
        self.runtime.send(lead['id'], 'Resume with a new task')
        with self.assertRaisesRegex(ValueError, 'turn was stopped'):
            self.runtime.create({'name': 'Late child', 'prompt': 'Old work', 'role': 'reviewer'},
                                lead['id'], parent_epoch=lead['epoch'])
        with self.assertRaisesRegex(ValueError, 'turn was stopped'):
            self.runtime.monitor(lead['id'], {'command': 'old command'}, epoch=lead['epoch'])

    def test_child_waiting_for_monitor_reports_only_after_final_result(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        self.runtime.monitor(child['id'], {'command': 'example-command'}, approved=True)
        eventually(lambda: self.runtime.snapshot()['monitors'][0]['status'] == 'running')
        self.complete(child)
        self.assertEqual(self.runtime.agent(child['id'])['status'], 'waiting')
        self.assertFalse(any(e['kind'] == 'child_result' for e in self.runtime.snapshot()['events']))
        self.runtime.server.gate.set()
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        self.complete(self.runtime.agent(child['id']))
        self.assertEqual(len([e for e in self.runtime.snapshot()['events'] if e['kind'] == 'child_result']), 1)

    def test_timeout_does_not_retry_model_call(self):
        self.runtime.connect().fail_start = True
        a = self.runtime.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': 'Finish'})
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'failed')
        time.sleep(.15)
        self.assertEqual(sum(m == 'turn/start' for m,p in self.runtime.server.calls), 1)
        self.assertEqual(self.runtime.snapshot()['events'][0]['status'], 'uncertain')

    def test_background_command_outlives_turn_and_late_exit_does_not_change_new_turn(self):
        a = self.lead()
        def event(method, **params):
            self.runtime.notification({'method': method, 'params': {'threadId': a['threadId'], 'turnId': a['turnId'], **params}})
        command = {'id': 'background-command', 'type': 'commandExecution', 'command': 'run build', 'processId': '42'}
        event('item/started', item=command)
        key = a['id'] + ':background-command'
        self.complete(a)
        self.assertEqual(self.runtime.task_detail(key)['status'], 'running')
        self.assertEqual(next(i for i in self.runtime.transcript(a['id'])['items'] if i['id'] == key)['toolStatus'], 'running')
        self.runtime.notification({'method': 'turn/started', 'params': {'threadId': a['threadId'], 'turn': {'id': 'new-turn'}}})
        event('item/commandExecution/outputDelta', itemId=command['id'], delta='x' * 14000)
        self.assertEqual(len(self.runtime.task_detail(key)['tail']), 12000)
        self.assertTrue(self.runtime.task_detail(key)['outputTruncated'])
        event('item/completed', item={**command, 'exitCode': 7, 'durationMs': 2500, 'aggregatedOutput': 'failed check'})
        finished = self.runtime.task_detail(key)
        self.assertEqual(finished['status'], 'failed')
        self.assertEqual(finished['exitCode'], 7)
        self.assertEqual(self.runtime.agent(a['id'])['turnId'], 'new-turn')
        self.assertEqual(self.runtime.agent(a['id'])['activity']['phase'], 'thinking')
        item = next(i for i in self.runtime.transcript(a['id'])['items'] if i['id'] == key)
        self.assertEqual(item['toolStatus'], 'failed')
        self.assertEqual(json.loads(item['text'])['exitCode'], 7)
        event('item/started', item=command)
        event('item/completed', item={**command, 'exitCode': 0})
        self.assertEqual(self.runtime.task_detail(key), finished)
        event('item/started', item={**command, 'id': 'unknown-old-command'})
        self.assertEqual(len(self.runtime.snapshot()['tasks']), 1)
        self.assertNotIn('tail', self.runtime.snapshot()['tasks'][0])

    def test_task_history_is_bounded_but_active_tasks_are_not_hidden(self):
        a = self.lead()
        def event(method, item):
            self.runtime.notification({'method': method, 'params': {'threadId': a['threadId'], 'turnId': a['turnId'], 'item': item}})
        for i in range(105):
            event('item/completed', {'id': f'tool-{i}', 'type': 'mcpToolCall', 'tool': 'test', 'status': 'completed'})
        for i in range(110):
            event('item/started', {'id': f'active-{i}', 'type': 'commandExecution', 'command': 'wait', 'processId': str(i)})
        tasks = self.runtime.snapshot()['tasks']
        self.assertEqual(sum(t['status'] == 'running' for t in tasks), 110)
        self.assertEqual(sum(t['status'] == 'completed' for t in tasks), 100)
        self.assertEqual(self.runtime.task_detail(a['id'] + ':tool-0')['status'], 'completed')
        with self.runtime.lock, self.runtime.db() as db:
            a['deletedAt'] = time.time()
            self.runtime.put(db, 'agents', a)
        self.assertEqual(self.runtime.snapshot()['tasks'], [])
        with self.assertRaisesRegex(ValueError, 'deleted'):
            self.runtime.task_detail(a['id'] + ':tool-0')

    def test_tool_outcome_is_unknown_after_disconnect_and_restart(self):
        a = self.lead()
        self.runtime.notification({'method': 'item/started', 'params': {'threadId': a['threadId'], 'turnId': a['turnId'],
            'item': {'id': 'tool', 'type': 'mcpToolCall', 'tool': 'watch'}}})
        self.runtime.disconnected()
        self.assertEqual(self.runtime.snapshot()['tasks'][0]['status'], 'lost')
        with self.runtime.lock, self.runtime.db() as db:
            task = self.runtime.task_detail(a['id'] + ':tool')
            task.update(status='running')
            self.runtime.put(db, 'tasks', task)
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.runtime.snapshot()['tasks'][0]['status'], 'lost')

    def test_cancel_pending_monitor_expires_approval_and_stops_clock(self):
        a = self.lead()
        monitor = self.runtime.monitor(a['id'], {'command': 'pending approval'})
        self.assertEqual(len(self.runtime.snapshot()['requests']), 1)
        self.runtime.cancel_monitor(monitor['id'])
        self.assertEqual(self.runtime.snapshot()['requests'], [])
        record = self.runtime.snapshot()['monitors'][0]
        self.assertEqual(record['status'], 'cancelled')
        self.assertGreaterEqual(record['finished'], record['created'])

if __name__ == '__main__':
    unittest.main(verbosity=2)

#!/usr/bin/env python3
"""Behavioral orchestration tests. No model service, no user state."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

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
from unittest.mock import patch

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from studio_api.testing import read_runtime_state
from codex_runtime import Runtime

# Keep general protocol fixtures independent of host image support. Dedicated
# image workspace contracts exercise the supported path with explicit mocks.
Runtime.image_workspace_support = staticmethod(
    lambda _repo: (False, "disabled in protocol fixture")
)


def eventually(predicate, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(.01)
    raise AssertionError('Condition did not become true')


class FakeServer:
    def __init__(self, root, notify, request, died):
        self._notify, self.request, self.died = notify, request, died
        self.calls, self.responses = [], []
        self.seq = 0
        self.gate = threading.Event()
        self.closed = False
        self.fail_start = False
        self.finish_before_reply = False
        self.start_gate = None
        self.active_turns = {}

    def notify(self, message):
        if message.get('method') == 'turn/completed':
            params = message['params']
            active = self.active_turns.get(params['threadId'])
            if active and active['id'] == params['turn']['id']:
                self.active_turns.pop(params['threadId'], None)
        self._notify(message)

    def call(self, method, params, timeout=60):
        self.calls.append((method, params))
        if method == 'skills/extraRoots/set':
            return {}
        if method == 'thread/settings/update':
            return {}
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
                self.start_gate.wait(30)
            if self.fail_start:
                raise RuntimeError('response timed out; outcome unknown')
            turn = self.active_turns.get(params['threadId'])
            if turn is None:
                self.seq += 1
                turn = {'id': f'turn-{self.seq}', 'status': 'inProgress'}
                self.active_turns[params['threadId']] = turn
                self.notify({'method': 'turn/started', 'params': {'threadId': params['threadId'], 'turn': turn}})
            if self.finish_before_reply:
                self.complete(params['threadId'], turn['id'])
            return {'turn': turn}
        if method == 'turn/steer':
            return {'turnId': params['expectedTurnId']}
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
            self.active_turns.pop(params['threadId'], None)
            self.notify({'method': 'turn/completed', 'params': {'threadId': params['threadId'],
                'turn': {'id': params['turnId'], 'status': 'interrupted'}}})
            return {}
        if method == 'account/rateLimits/read':
            return {'rateLimits': {'limitId': 'codex', 'primary': {'usedPercent': 42, 'windowDurationMins': 300, 'resetsAt': 2000000000}}}
        if method == 'model/list':
            return {'data': [{'model': model, 'defaultReasoningEffort': 'medium', 'supportedReasoningEfforts': [{'reasoningEffort': effort} for effort in ['low', 'medium', 'high', 'xhigh', 'max', 'ultra']], 'serviceTiers': [{'id': 'priority'}]} for model in ['test-model', 'gpt-6-astra', 'gpt-6-sol', 'gpt-6-luna', 'gpt-5.6-sol', 'gpt-5.6-luna', 'gpt-5.6-terra']]}
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

    def on_result(self, future, callback):
        future.add_done_callback(callback)

    def after_events(self, callback):
        # This fake delivers notifications and callbacks synchronously.
        callback()

    def join_callbacks(self, timeout=10):
        return True

    def complete(self, tid, turn, text='Result with evidence'):
        if self.active_turns.get(tid, {}).get('id') == turn:
            self.active_turns.pop(tid, None)
        self.notify({'method': 'item/completed', 'params': {'threadId': tid,
            'item': {'id': turn + '-answer', 'type': 'agentMessage', 'text': text}}})
        self.notify({'method': 'turn/completed', 'params': {'threadId': tid,
            'turn': {'id': turn, 'status': 'completed'}}})

    def close(self):
        self.closed = True
        self.gate.set()


class RuntimeContract(unittest.TestCase):
    def track_delivery_starts(self):
        """Track queued start callables; executor sentinels are not barriers."""
        executor = self.runtime.delivery_executor()
        original_submit = executor.submit
        condition = threading.Condition()
        counts = {'submitted': 0, 'completed': 0}

        def submit(function, *args, **kwargs):
            is_start = getattr(function, '__name__', None) == 'start' and bool(args)
            if is_start:
                with condition:
                    counts['submitted'] += 1
            future = original_submit(function, *args, **kwargs)
            if is_start:
                def completed(_future):
                    with condition:
                        counts['completed'] += 1
                        condition.notify_all()
                future.add_done_callback(completed)
            return future

        submit_patch = patch.object(executor, 'submit', side_effect=submit)
        submit_patch.start()
        self.addCleanup(submit_patch.stop)

        def wait_until_balanced():
            with condition:
                return condition.wait_for(
                    lambda: counts['submitted'] == counts['completed'], timeout=30
                )

        return wait_until_balanced

    def test_busy_input_uses_one_start_request_and_local_identity(self):
        lead = self.lead()
        original_turn = lead['turnId']
        self.runtime.send(lead['id'], 'Busy input', 'exact-busy', delivery='steer')
        eventually(lambda: self.runtime.delivery_receipt('exact-busy')['status'] == 'delivered')
        calls = [p for method, p in self.runtime.server.calls if method == 'turn/start']
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[-1]['clientUserMessageId'], 'exact-busy')
        self.assertEqual(self.runtime.agent(lead['id'])['turnId'], original_turn)
        self.runtime.send(lead['id'], 'Busy input', 'exact-busy', delivery='queue')
        self.runtime.dispatch()
        self.assertEqual(len([1 for method, _ in self.runtime.server.calls if method == 'turn/start']), 2)
        self.assertFalse(any(method == 'turn/steer' for method, _ in self.runtime.server.calls))

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.runtime = Runtime(self.root, FakeServer)

    def test_budget_schema_is_ready_before_runtime_workers(self):
        from unittest.mock import patch
        from codex_sync import SyncStore
        import uuid

        thread_id = str(uuid.uuid4())
        agent = self.runtime.create({'name': 'History import', 'cwd': str(self.root),
                                     'prompt': 'fixture', 'threadId': thread_id}, defer=True)
        agent.update(threadId=thread_id, turnId='history-turn', status='paused')
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', agent)
        profile = self.root / 'history-profile'
        profile.mkdir()
        rollout = profile / 'rollout.jsonl'
        at = agent['created'] + 1
        usage = {'input_tokens': 1, 'output_tokens': 2, 'total_tokens': 3}
        rollout.write_text(''.join(json.dumps(record) + '\n' for record in (
            {'type': 'session_meta', 'timestamp': at, 'payload': {'id': thread_id}},
            {'type': 'token_usage_record', 'timestamp': at,
             'payload': {'thread_id': thread_id, 'turn_id': 'history-turn',
                         'response_id': 'history-response', 'usage': usage,
                         'thread_token_usage': usage}},
        )))

        store = SyncStore(self.runtime.db, lambda _agent: {})
        store._ensure_versions()
        with self.runtime.db() as db:
            tables = {row[0] for row in db.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            )}
            before = db.execute('PRAGMA schema_version').fetchone()[0]
            triggers = {row[0] for row in db.execute(
                "SELECT name FROM sqlite_master WHERE type='trigger'"
            )}
        self.assertIn('runtime_budget', tables)
        self.assertIn('runtime_budget_usage', tables)
        with patch.object(self.runtime.accounts, 'home', return_value=profile), \
                patch.object(self.runtime, '_analytics_rollout_path', return_value=(rollout, None)):
            self.assertTrue(self.runtime.analytics_history_step())
        with self.runtime.db() as db:
            after = db.execute('PRAGMA schema_version').fetchone()[0]
            imported = db.execute('SELECT COUNT(*) FROM analytics_usage WHERE agent=?', (agent['id'],)).fetchone()[0]
            charged = db.execute('SELECT COUNT(*) FROM runtime_budget_usage WHERE agent=?', (agent['id'],)).fetchone()[0]
        self.assertEqual(after, before)
        self.assertEqual(imported, 1)
        self.assertEqual(charged, 1)

    def test_scheduler_schema_is_ready_before_scheduler_starts(self):
        started = threading.Event()
        release = threading.Event()

        def hold_scheduler(_runtime):
            started.set()
            release.wait(5)

        with tempfile.TemporaryDirectory() as directory, \
                patch.object(Runtime, 'schedule', hold_scheduler):
            runtime = Runtime(Path(directory), FakeServer)
            try:
                self.assertTrue(started.wait(2))
                with runtime.db() as db:
                    indexes = {row[0] for row in db.execute(
                        "SELECT name FROM sqlite_master WHERE type='index'"
                    )}
                    tables = {row[0] for row in db.execute(
                        "SELECT name FROM sqlite_master WHERE type='table'"
                    )}
                self.assertTrue({
                    'runtime_agent_dispatch_active',
                    'runtime_agent_dispatch_busy_input',
                    'runtime_agent_dispatch_workspace',
                    'runtime_work_archive_due',
                } <= indexes)
                self.assertIn('runtime_account_transfers', tables)
            finally:
                release.set()
                runtime.close()

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def lead(self, **extra):
        a = self.runtime.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': 'Coordinate the work', **extra})
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        return self.runtime.agent(a['id'])

    def snapshot(self):
        return read_runtime_state(self.runtime)

    def delete_client(self):
        from fastapi.testclient import TestClient
        from codex_canvas import Canvas
        from studio_api.app import create_app
        from studio_api.context import ApiContext

        class FixtureRemote:
            def request_origin(self, _headers, _peer, _port):
                return 'http://testserver'

        canvas = Canvas(self.root)
        canvas.runtime = self.runtime
        context = ApiContext(canvas, token='delete-fixture-token', remote=FixtureRemote())
        client = TestClient(create_app(context))
        self.addCleanup(client.close)
        return client, {'Origin': 'http://testserver', 'X-Canvas-Token': context.token}

    def complete(self, a):
        self.runtime.server.complete(a['threadId'], a['turnId'])

    def test_after_turn_input_waits_for_the_active_turn(self):
        lead = self.lead()
        self.runtime.send(lead['id'], 'Queued for later', 'after-turn-1', delivery='after_turn')
        self.runtime.dispatch()
        time.sleep(.3)
        def carried():
            return [m for m, p in self.runtime.server.calls if m in ('turn/start', 'turn/steer')
                    and 'Queued for later' in json.dumps(p)]
        self.assertEqual(carried(), [], 'after_turn input is not steered into the active turn')
        self.complete(lead)
        eventually(lambda: carried() == ['turn/start'])
        time.sleep(.3)
        self.assertEqual(carried(), ['turn/start'], 'delivered once, as a new turn')

    def test_budget_save_keeps_the_chat_visible_in_entity_sync(self):
        lead = self.lead()
        import codex_budget
        state = {'spent': 1234, 'floor': 0, 'after': 1234, 'before': 0, 'noticeSpent': 0,
                 'historicalNotices': 0, 'faults': 0, 'ambiguousNotices': 0}
        with self.runtime.lock, self.runtime.db() as db:
            codex_budget.budget_init(db)
            codex_budget._save(db, self.runtime.agent(lead['id'], db), state)
            row = db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                             (lead['id'],)).fetchone()
        value = json.loads(row[0])['value']
        self.assertEqual((value.get('source'), value.get('kind')), ('managed', 'agent'))
        self.assertIn('canSend', value)
        self.assertEqual(value.get('tokensUsed'), 1234)

    def test_empty_current_chat_reuses_identity_even_after_restart(self):
        import uuid
        first = self.runtime.new_lead({})
        request = {'id': str(uuid.uuid4()), 'previous': first['id']}
        with ThreadPoolExecutor(max_workers=4) as pool:
            results = list(pool.map(lambda _: self.runtime.new_lead(request), range(8)))
        self.assertEqual({a['id'] for a in results}, {first['id']})
        self.assertEqual(len(read_runtime_state(self.runtime)['agents']), 1)
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
        eventually(lambda: self.runtime.chat_read(private['room'], first['id'])['messages'][0]
                   ['deliveries'].get(peer['id']) == 'delivered')
        self.assertEqual(self.runtime.chat_read(parent['room'], first['id'])['messages'][0]
                         ['deliveries'][lead['id']], 'delivered')
        with self.runtime.lock, self.runtime.db() as db:
            db.execute('UPDATE runtime_chat_messages SET deliveries=? WHERE id=?',
                       (json.dumps({peer['id']: 'queued'}), 'msg-private'))
        self.assertEqual(self.runtime.chat_read(private['room'], first['id'])['messages'][0]
                         ['deliveries'][peer['id']], 'delivered')
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
            with self.assertRaisesRegex(ValueError, 'limited to one team'):
                self.runtime.chat_message(lead['id'], 'all', 'Shared finding', 'broadcast-all')
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

    def test_broadcast_does_not_restart_inactive_assignments(self):
        lead = self.lead()
        states = ['completed', 'failed', 'interrupted', 'paused', 'idle',
                  'queued', 'starting', 'running', 'waiting', 'approval']
        with self.runtime.lock:
            children = []
            for state in states:
                child = self.runtime.create({'name': state, 'prompt': 'Review', 'role': 'reviewer'}, lead['id'], defer=True)
                child.update(status=state, autoWake=True)
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', child)
                children.append(child)
            for target in ['broadcast']:
                receipt = self.runtime.chat_message(lead['id'], target, 'Policy update only', 'inactive-' + target)
                for child in children:
                    active = child['status'] in states[5:]
                    self.assertEqual(receipt['deliveries'][child['id']], 'queued' if active else 'stored_only')
                    if not active:
                        self.assertEqual(self.runtime.agent(child['id'])['status'], child['status'])
                        with self.runtime.db() as db:
                            self.assertIsNone(db.execute("SELECT id FROM runtime_events WHERE id=?", (
                                'chat:inactive-' + target + ':' + child['id'],)).fetchone())
                    history = self.runtime.chat_read(receipt['room'], child['id'])
                    self.assertEqual(history['messages'][-1]['text'], 'Policy update only')
                self.assertEqual(self.runtime.chat_message(lead['id'], target, 'Policy update only', 'inactive-' + target), receipt)
            # A direct request still resumes an idle worker, and child results
            # still wake a finished lead (covered by the private-chat test).
            child = children[0]
            direct = self.runtime.chat_message(lead['id'], child['id'], 'New scoped assignment', 'direct-resume')
            self.assertEqual(direct['deliveries'][child['id']], 'queued')
            self.assertEqual(self.runtime.agent(child['id'])['status'], 'queued')

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
        self.assertEqual(len(read_runtime_state(self.runtime)['agents']), 2)

    def test_finished_workers_do_not_consume_active_team_capacity(self):
        lead = self.lead(maxAgents=2)
        finished = [self.runtime.create({'name': str(i), 'prompt': 'Review', 'role': 'reviewer'},
                                        lead['id'], defer=True) for i in range(3)]
        self.assertEqual(len(finished), 3)
        active = self.runtime.create({'name': 'Active', 'prompt': 'Review', 'role': 'reviewer'},
                                     lead['id'], defer=False)
        self.assertIn(active['status'], {'queued', 'starting', 'running'})
        with self.assertRaisesRegex(ValueError, '3 finished agents.*archive_finished'):
            self.runtime.create({'name': 'Overflow', 'prompt': 'Review', 'role': 'reviewer'},
                                lead['id'], defer=False)
        self.runtime.dynamic({'id': 7701, 'params': {'threadId': lead['threadId'],
            'callId': 'capacity-batch', 'tool': 'orchestration_spawn',
            'arguments': {'agents': [{'name': 'Overflow', 'prompt': 'Review', 'role': 'reviewer'}]}}})
        reply = next(r for r in self.runtime.server.responses if r['id'] == 7701)['result']
        self.assertFalse(reply['success'])
        self.assertIn('3 finished agents', reply['contentItems'][0]['text'])

    def test_delete_stops_tree_and_rejects_retry_or_late_wakeup(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Peer', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        message = self.runtime.chat_message(child['id'], 'parent', 'Progress', 'delete-message')
        deleted = self.runtime.delete_conversation(lead['id'])
        self.assertEqual(set(deleted['deleted']), {lead['id'], child['id']})
        self.assertEqual(self.snapshot()['agents'], [])
        self.assertEqual(self.snapshot()['rooms'], [])
        self.runtime.server.complete(child['threadId'], child['turnId'], 'Late result')
        self.assertFalse(self.runtime.agent(lead['id'])['autoWake'])
        with self.assertRaisesRegex(ValueError, 'deleted'):
            self.runtime.send(lead['id'], 'Replay')
        self.assertEqual(self.runtime.delete_conversation(lead['id'])['deleted'], deleted['deleted'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_chat_messages').fetchone()[0], 1)
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.snapshot()['agents'], [])

    def test_delete_route_handles_broadcast_root_tombstone_order(self):
        lead = self.lead()
        child = self.runtime.create(
            {'name': 'Delete route worker', 'prompt': 'Review', 'role': 'reviewer'},
            lead['id'],
        )
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'rooms', {
                'id': 'broadcast:' + lead['id'],
                'kind': 'broadcast',
                'rootId': lead['id'],
                'updated': time.time(),
            })

        client, headers = self.delete_client()
        response = client.post(
            '/api/conversation/delete',
            json={'id': lead['id']},
            headers=headers,
        )

        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()['deleted'], sorted([lead['id'], child['id']]))
        replay = client.post('/api/conversation/delete', json={'id': lead['id']}, headers=headers)
        self.assertEqual(replay.status_code, 200, replay.text)
        self.assertEqual(replay.json()['deleted'], response.json()['deleted'])
        self.assertFalse(self.runtime.agent(lead['id'])['autoWake'])
        self.assertFalse(self.runtime.agent(child['id'])['autoWake'])

    def test_delete_route_handles_unknown_no_thread_voice_and_work(self):
        lead = self.lead(name='Delete route lead')
        child = self.runtime.create(
            {'name': 'No thread worker', 'prompt': 'Review', 'role': 'reviewer'},
            lead['id'],
        )
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        no_thread = self.runtime.create(
            {'name': 'No native thread', 'cwd': str(self.root), 'prompt': 'Fixture task'}, defer=True
        )
        self.assertIsNone(self.runtime.agent(no_thread['id']).get('threadId'))
        voice = self.runtime.voice()
        session = 'delete-voice-session'
        with self.runtime.lock, self.runtime.db() as db:
            db.execute(
                'INSERT INTO voice_sessions(id,agent,created,state) VALUES(?,?,?,?)',
                (session, lead['id'], time.time(), 'ready'),
            )
        voice.record(lead['id'], session, 'delete-voice-record', 'user', 'fixture transcript')
        work = self.runtime.work_action(lead['id'], {'action': 'create', 'title': 'Delete work'})
        self.runtime.work_action(
            lead['id'], {'action': 'update', 'task_id': work['id'], 'owner': child['id']}
        )
        self.runtime.work_action(child['id'], {'action': 'claim', 'task_id': work['id']}, actor=child['id'])

        client, headers = self.delete_client()
        unknown = client.post(
            '/api/conversation/delete', json={'id': 'unknown-delete-agent'}, headers=headers
        )
        self.assertEqual(unknown.status_code, 400, unknown.text)
        self.assertIsNone(self.runtime.agent(lead['id']).get('deletedAt'))

        no_thread_response = client.post(
            '/api/conversation/delete', json={'id': no_thread['id']}, headers=headers
        )
        self.assertEqual(no_thread_response.status_code, 200, no_thread_response.text)
        self.assertEqual(no_thread_response.json()['deleted'], [no_thread['id']])

        response = client.post('/api/conversation/delete', json={'id': lead['id']}, headers=headers)
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()['deleted'], sorted([lead['id'], child['id']]))
        with self.runtime.db() as db:
            released = next(item for item in self.runtime.work_records(db, lead['id']) if item['id'] == work['id'])
        self.assertIsNone(released['owner'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM voice_sessions WHERE agent=?', (lead['id'],)).fetchone()[0], 0)
            self.assertEqual(db.execute('SELECT COUNT(*) FROM voice_records WHERE agent=?', (lead['id'],)).fetchone()[0], 0)

    def test_delete_route_tolerates_orphaned_room_roots_and_members(self):
        orphan_root = self.lead(name='Removed root')
        orphan_member = self.runtime.create(
            {'name': 'Surviving member', 'prompt': 'Review', 'role': 'reviewer'},
            orphan_root['id'],
            defer=True,
        )
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'rooms', {
                'id': 'broadcast:' + orphan_root['id'],
                'kind': 'broadcast',
                'rootId': orphan_root['id'],
                'updated': time.time(),
            })
            self.runtime.put(db, 'rooms', {
                'id': 'private:orphan-member',
                'kind': 'private',
                'members': ['missing-room-member'],
                'updated': time.time(),
            })
            db.execute('DELETE FROM runtime_agents WHERE id=?', (orphan_root['id'],))
        target = self.runtime.create(
            {'name': 'Delete target', 'cwd': str(self.root), 'prompt': 'Fixture task'}, defer=True
        )

        client, headers = self.delete_client()
        response = client.post(
            '/api/conversation/delete', json={'id': target['id']}, headers=headers
        )

        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()['deleted'], [target['id']])
        with self.runtime.db() as db:
            visible = self.runtime.chat_rooms(db)
        self.assertNotIn('broadcast:' + orphan_root['id'], [room['id'] for room in visible])
        self.assertNotIn('private:orphan-member', [room['id'] for room in visible])
        self.assertIsNone(self.runtime.agent(orphan_member['id']).get('deletedAt'))

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
        eventually(lambda: any(c['text'] in p['input'][0]['text'] for method, p in self.runtime.server.calls
                               if method == 'turn/start' and p['threadId'] == lead['threadId']))
        prompts = [p['input'][0]['text'] for method, p in self.runtime.server.calls
                   if method == 'turn/start' and p['threadId'] == lead['threadId']]
        prompt = next(text for text in prompts if c['text'] in text)
        self.assertEqual(sum(text.count(c['text']) for text in prompts), 1,
                         'deliver the full complaint once, not an ID-only reminder')
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
        self.assertFalse(self.snapshot()['complaints'][0]['needsUserResponse'])
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
            self.assertEqual(prompt.count(c['text']), 1 if index == 0 else 0)
            self.assertIn(c['id'], prompt)
            if index == 0:
                self.assertIn('"author": "user"', prompt)
            else:
                self.assertIn('still requiring a response', prompt)
            self.complete(self.runtime.agent(lead['id']))
        stopped = self.runtime.agent(lead['id'])
        self.assertFalse(stopped['autoWake'])
        self.assertEqual(stopped['status'], 'failed')
        self.assertIn('Three turns', stopped['error'])
        self.assertFalse(self.snapshot()['complaints'][0]['needsUserResponse'])
        self.runtime.send(lead['id'], 'Read the complaint and act')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        a = self.runtime.agent(lead['id'])
        self.runtime.complaint(a['id'], {'action':'respond','complaint_id':c['id'],'text':'I will recover the missing log before the next review.','status':'in_progress'}, 'act', a['epoch'])
        self.complete(a)
        self.assertEqual(self.runtime.agent(a['id'])['status'], 'completed')

    def test_no_complaint_book_check_for_normal_turns(self):
        lead = self.lead()
        first = [p for method, p in self.runtime.server.calls if method == 'turn/start'][-1]['input'][0]['text']
        self.assertNotIn('complaint', first.replace(self.runtime.role_guidance(lead), '').lower())
        self.complete(lead)
        self.runtime.send(lead['id'], 'Continue the work')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        second = [p for method, p in self.runtime.server.calls if method == 'turn/start'][-1]['input'][0]['text']
        self.assertNotIn('complaint', second.lower())
        self.complete(self.runtime.agent(lead['id']))
        self.assertEqual(self.runtime.agent(lead['id'])['status'], 'completed')
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE kind='complaint'").fetchone()[0], 0)

    def test_complaint_honors_stop_with_current_tool(self):
        a = self.lead()
        self.runtime.dynamic({'id':900,'params':{'threadId':a['threadId'],'callId':'complaint','tool':'orchestration_complaint',
            'arguments':{'action':'submit','text':'A concrete problem'}}})
        self.assertTrue(next(r for r in self.runtime.server.responses if r['id']==900)['result']['success'])
        self.runtime.stop(a['id'])
        self.runtime.complaint(a['id'], {'action':'submit','text':'User complaint while stopped'}, 'stopped-complaint', user=True)
        self.assertFalse(self.runtime.agent(a['id'])['autoWake'])
        self.assertEqual(len(self.snapshot()['complaints']), 2)

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
        self.assertEqual(self.runtime.rate_limits['data']['rateLimits']['primary']['usedPercent'], 42)
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.runtime.agent(a['id'])['compactions'], 1)

    def test_rate_limit_notification_does_not_wait_for_unrelated_runtime_lock(self):
        entered, release = threading.Event(), threading.Event()
        def hold_runtime():
            with self.runtime.lock:
                entered.set()
                release.wait(3)
        holder = threading.Thread(target=hold_runtime)
        holder.start()
        self.assertTrue(entered.wait(1))
        try:
            started = time.monotonic()
            self.runtime.notification({'method': 'account/rateLimits/updated', 'params': {
                'rateLimits': {'limitId': 'codex', 'primary': {'usedPercent': 42}}}})
            self.assertLess(time.monotonic() - started, 1)
            self.assertEqual(self.runtime.rate_limits['data']['rateLimits']['primary']['usedPercent'], 42)
        finally:
            release.set()
            holder.join(3)

    def test_dispatcher_database_reuses_connection_and_commits_each_callback(self):
        import sqlite3
        self.runtime.notification({'method': 'account/rateLimits/updated', '_studioDispatchedAt': time.time(),
            'params': {'rateLimits': {'limitId': 'codex', 'primary': {'usedPercent': 2}}}})
        cached = self.runtime._callback_db.connection
        try:
            with self.runtime.db() as db:
                self.assertIs(db, cached)
                db.execute('INSERT INTO runtime_completed_turns VALUES (?)', ('fixture-commit',))
            with sqlite3.connect(self.runtime.db_path) as other:
                self.assertEqual(other.execute('SELECT COUNT(*) FROM runtime_completed_turns WHERE id=?',
                                               ('fixture-commit',)).fetchone()[0], 1)
            with self.assertRaises(sqlite3.IntegrityError):
                with self.runtime.db() as db:
                    db.execute('INSERT INTO runtime_completed_turns VALUES (?)', ('fixture-rollback',))
                    db.execute('INSERT INTO runtime_completed_turns VALUES (?)', ('fixture-rollback',))
            with self.runtime.db() as db:
                self.assertIs(db, cached)
                self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_completed_turns WHERE id=?',
                                            ('fixture-rollback',)).fetchone()[0], 0)
        finally:
            self.runtime._callback_db.reuse = False
            del self.runtime._callback_db.connection
            cached.close()

    def test_sidebar_rename_and_room_hide_preserve_agent_history(self):
        a = self.lead()
        b = self.runtime.create({'name': 'Reviewer', 'prompt': 'Review', 'role': 'reviewer'}, a['id'], defer=True)
        room = self.runtime.chat_message(a['id'], b['id'], 'First message', 'rename-first')['room']
        self.runtime.rename(a['id'], 'Manual title')
        self.runtime.dynamic({'id':901,'params':{'threadId':a['threadId'],'callId':'late-title','tool':'orchestration_title','arguments':{'title':'LLM title'}}})
        self.assertEqual(self.runtime.agent(a['id'])['name'], 'Manual title')
        self.runtime.rename(room, 'Review chat', 'room-rename')
        self.assertEqual(self.runtime.rename(room, 'Review chat', 'room-rename')['name'], 'Review chat')
        self.runtime.hide_room(room)
        self.assertEqual(len(self.snapshot()['rooms']), 1)
        self.assertTrue(self.snapshot()['rooms'][0]['userHidden'])
        self.assertEqual(len(self.runtime.chat_read(room,a['id'])['messages']), 1)
        self.runtime.chat_message(a['id'], b['id'], 'New message', 'rename-next')
        self.assertEqual(self.snapshot()['rooms'][0]['name'], 'Review chat')
        self.assertEqual(self.snapshot()['rooms'][0]['lastMessage']['text'], 'New message')

    def test_rename_receipt_sets_exact_title_and_generates_once(self):
        import codex_rename
        a = self.lead()
        worker = self.runtime.create({'name': 'Worker', 'prompt': 'Check the UI', 'role': 'reviewer'}, a['id'], defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(db, a['id'], 'rename-user', 'user', 'Review the release and report blockers.', 'You')
            self.runtime.item(db, a['id'], 'rename-work', 'assistant', 'The worker found two blockers.', 'Agent')
        before = len(self.runtime.server.calls)
        exact = self.runtime.rename(worker['id'], 'Exact worker name', 'rename-exact')
        self.assertEqual(exact['name'], 'Exact worker name')
        self.assertEqual(self.runtime.rename(worker['id'], 'Exact worker name', 'rename-exact'), exact)
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.runtime.rename(worker['id'], 'Another name', 'rename-exact')
        self.assertFalse(any(method in {'turn/start', 'turn/steer'} for method, _ in self.runtime.server.calls[before:]))
        gate = threading.Event()
        calls = []
        def title(_runtime, _agent, first, recent):
            calls.append((first, recent))
            gate.wait(5)
            return 'Specific release review'
        with patch.object(codex_rename, '_generate', side_effect=title):
            pending = self.runtime.rename(a['id'], None, 'rename-auto')
            self.assertEqual(pending['status'], 'pending')
            self.assertEqual(self.runtime.rename(a['id'], None, 'rename-auto'), pending)
            gate.set()
            eventually(lambda: self.runtime.rename(a['id'], None, 'rename-auto')['status'] == 'applied')
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][0], 'Coordinate the work')
        self.assertIn(('User', 'Review the release and report blockers.'), calls[0][1])
        self.assertIn(('Agent', 'The worker found two blockers.'), calls[0][1])
        self.assertEqual(self.runtime.agent(a['id'])['name'], 'Specific release review')
        self.assertFalse(any(method in {'turn/start', 'turn/steer'} for method, _ in self.runtime.server.calls[before:]))
        interrupted = threading.Event()
        with patch.object(codex_rename, '_generate', side_effect=lambda *_: (interrupted.wait(5), 'Other title')[1]):
            self.assertEqual(self.runtime.rename(a['id'], None, 'rename-lost')['status'], 'pending')
            with self.runtime.lock:
                self.runtime._rename_jobs.clear()
            self.assertEqual(self.runtime.rename(a['id'], None, 'rename-lost')['status'], 'failed')
            interrupted.set()
            self.assertEqual(self.runtime.rename(a['id'], None, 'rename-lost')['status'], 'failed')
        self.assertEqual(self.runtime.agent(a['id'])['name'], 'Specific release review')

    def test_rename_model_uses_chat_account_without_a_chat_turn(self):
        import codex_rename
        from types import SimpleNamespace
        profile = self.root / 'profile'
        profile.mkdir()
        seen = []
        def run(command, **options):
            seen.append((command, options))
            if '--output-last-message' in command:
                Path(command[command.index('--output-last-message') + 1]).write_text('Release blocker report')
            return SimpleNamespace(returncode=0, stdout='Release blocker report')
        codex_catalog = {'data': [
            {'model': 'gpt-6-astra', 'supportedReasoningEfforts': [{'reasoningEffort': 'low'}]},
            {'model': 'gpt-6-luna', 'supportedReasoningEfforts': [{'reasoningEffort': 'low'}]},
        ]}
        with patch.object(self.runtime.accounts, 'get', return_value={'provider': 'codex'}), \
                patch.object(self.runtime.accounts, 'home', return_value=profile), \
                patch.object(self.runtime, 'catalog', return_value=codex_catalog) as catalog, \
                patch.object(codex_rename.subprocess, 'run', side_effect=run):
            self.assertEqual(codex_rename._generate(self.runtime,
                {'id': 'chat', 'accountKey': 'profile-a', 'provider': 'codex', 'model': 'gpt-6-astra'},
                'Review release', [('Agent', 'Found blocker')]), 'Release blocker report')
            catalog.assert_called_once_with('profile-a')
        self.assertEqual(seen[0][1]['env']['CODEX_HOME'], str(profile))
        self.assertIn('--ephemeral', seen[0][0])
        self.assertEqual(seen[0][0][seen[0][0].index('--model') + 1], 'gpt-6-luna')
        self.assertIn('model_reasoning_effort="low"', seen[0][0])
        self.assertIn('cli_auth_credentials_store="file"', seen[0][0])
        seen.clear()
        with patch.object(self.runtime.accounts, 'get', return_value={'provider': 'codex'}), \
                patch.object(self.runtime.accounts, 'home', return_value=profile), \
                patch.object(self.runtime, 'catalog', return_value={'data': [codex_catalog['data'][0], {'model': 'gpt-6-sol'}]}), \
                patch.object(codex_rename.subprocess, 'run', side_effect=run):
            codex_rename._generate(self.runtime,
                {'id': 'chat', 'accountKey': 'profile-a', 'provider': 'codex', 'model': 'gpt-6-astra'},
                'Review release', [])
        self.assertEqual(seen[0][0][seen[0][0].index('--model') + 1], 'gpt-6-astra')
        seen.clear()
        claude_catalog = {'data': [{'model': 'sonnet'}, {'model': 'haiku', 'resolvedModel': 'claude-haiku-4-5'}]}
        with patch.object(self.runtime.accounts, 'get', return_value={'claudeOptions': {}}), \
                patch.object(self.runtime, 'catalog', return_value=claude_catalog), \
                patch('codex_claude.installed', return_value='/fake/claude'), \
                patch('codex_claude.subscription_env', return_value={'CLAUDE_CONFIG_DIR': str(profile)}), \
                patch.object(codex_rename.subprocess, 'run', side_effect=run):
            self.assertEqual(codex_rename._generate(self.runtime,
                {'id': 'chat', 'accountKey': 'profile-b', 'provider': 'claude', 'model': 'sonnet'},
                'Review release', []), 'Release blocker report')
        self.assertEqual(seen[0][1]['env']['CLAUDE_CONFIG_DIR'], str(profile))
        self.assertIn('--no-session-persistence', seen[0][0])
        self.assertEqual(seen[0][0][seen[0][0].index('--model') + 1], 'haiku')
        seen.clear()
        with patch.object(self.runtime.accounts, 'get', return_value={'claudeOptions': {}}), \
                patch.object(self.runtime, 'catalog', return_value={'data': [{'model': 'sonnet'}]}), \
                patch('codex_claude.installed', return_value='/fake/claude'), \
                patch('codex_claude.subscription_env', return_value={'CLAUDE_CONFIG_DIR': str(profile)}), \
                patch.object(codex_rename.subprocess, 'run', side_effect=run):
            codex_rename._generate(self.runtime,
                {'id': 'chat', 'accountKey': 'profile-b', 'provider': 'claude', 'model': 'sonnet'},
                'Review release', [])
        self.assertEqual(seen[0][0][seen[0][0].index('--model') + 1], 'sonnet')

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

    def test_lead_models_follow_account_catalog_and_role_identity(self):
        for model in ['gpt-5.6-luna', 'gpt-5.6-terra', 'test-model']:
            lead = self.runtime.new_lead({'model': model})
            self.assertTrue(lead['isLead'])
            self.assertEqual(lead['model'], model)
            created = self.runtime.create({'cwd': str(self.root), 'prompt': 'Task', 'model': model}, draft=True)
            self.assertEqual(created['model'], model)
        with self.assertRaisesRegex(ValueError, 'not available'):
            self.runtime.new_lead({'model': 'fake-sol'})
        with self.assertRaisesRegex(ValueError, 'not available'):
            self.runtime.create({'cwd': str(self.root), 'prompt': 'Task', 'model': 'fake-sol'})
        a = self.runtime.new_lead({'model': 'gpt-5.6-sol'})
        self.assertEqual(self.runtime.conversation_settings(a['id'], {'model': 'gpt-5.6-luna'})['model'], 'gpt-5.6-luna')
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
        self.assertNotIn('dangerouslySkipAccountRules', self.runtime.agent(worker['id']))
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
        self.assertEqual(self.snapshot()['requests'], [])
        params = next(p for method, p in self.runtime.server.calls if method == 'thread/start')
        self.assertFalse(params['config']['agents.enabled'])
        self.assertFalse(params['config']['features.multi_agent_v2'])

    def test_async_question_can_resume_a_finished_agent(self):
        a = self.lead()
        self.runtime.notification({'method': 'item/completed', 'params': {'threadId': a['threadId'],
            'item': {'type': 'agentMessage', 'id': 'question-1', 'text': 'Choose a scope',
                     'questions': [{'title': 'Choose a scope', 'options': ['One file', 'All files']}]}}})
        self.complete(a)
        question = self.snapshot()['requests'][0]
        self.assertEqual(question['method'], 'agent/asyncQuestion')
        self.assertEqual(self.runtime.agent(a['id'])['status'], 'completed')
        self.runtime.answer(question['id'], {'answers': {'0': {'answers': ['One file']}}})
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        self.assertEqual(self.snapshot()['requests'], [])
        self.assertIn('One file', self.runtime.transcript(a['id'])['items'][-1]['text'])

    def test_role_identity_survives_restart(self):
        a = self.lead()
        self.complete(a)
        b = self.runtime.create({'cwd': str(self.root), 'prompt': 'Review', 'role': 'reviewer'}, defer=True)
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
        eventually(lambda: any(i['id'].endswith(':text') and i['text'].endswith('Unfinished')
                               for i in self.runtime.transcript(a['id'])['items']))
        transcript = self.runtime.transcript(a['id'])
        self.assertEqual(transcript['agent']['activity']['phase'], 'writing')
        self.assertTrue(transcript['items'][-1]['streaming'])
        event('item/started', item={'id': 'cmd', 'type': 'commandExecution', 'command': 'test', 'status': 'inProgress'})
        event('item/commandExecution/outputDelta', itemId='cmd', delta='Live output\n')
        eventually(lambda: 'Live output' in self.runtime.transcript(a['id'])['items'][-1]['text'])
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

    def test_transcript_read_does_not_hold_ui_condition(self):
        acquired = threading.Event()
        def read(_):
            def writer():
                with self.runtime.ui_condition:
                    acquired.set()
            thread = threading.Thread(target=writer, daemon=True)
            thread.start()
            self.assertTrue(acquired.wait(1), 'Transcript blocks the UI notification lock')
            thread.join(1)
            return {'ok': True}
        self.runtime.transcript = read
        self.assertEqual(self.runtime.wait_transcript('probe', -1, 0), (0, {'ok': True}))

    def test_batched_text_preserves_content_and_notification_analytics(self):
        from codex_analytics import encoded
        a = self.lead()
        eventually(lambda: (self.runtime.agent(a['id']).get('startAttempt') or {}).get('turnId') == a['turnId'])
        samples = [{'threadId': a['threadId'], 'turnId': a['turnId'], 'itemId': 'batch', 'delta': text}
                   for text in ['Первый ', 'абзац.\n\n', 'Second paragraph.']]
        method = 'item/agentMessage/delta'
        def counters():
            with self.runtime.db() as db:
                return tuple(db.execute('SELECT coalesce(sum(count),0),coalesce(sum(bytes),0) FROM analytics_notifications WHERE agent=? AND method=?', (a['id'], method)).fetchone())
        before = counters()
        count = self.runtime.agent(a['id'])['events']
        self.runtime.notification({'method': method, 'params': {**samples[0], 'delta': ''.join(p['delta'] for p in samples)},
                                   '_studioNotificationSamples': samples})
        eventually(lambda: counters()[0] - before[0] == len(samples))
        after = counters()
        self.assertEqual(after[0] - before[0], len(samples))
        self.assertEqual(after[1] - before[1], sum(len(encoded(p).encode('utf-8')) for p in samples))
        self.assertEqual(self.runtime.agent(a['id'])['events'] - count, len(samples))
        item = next(i for i in self.runtime.transcript(a['id'])['items'] if i['id'].endswith(':batch'))
        self.assertEqual(item['text'], ''.join(p['delta'] for p in samples))

    def test_batched_terminal_output_preserves_content_and_notification_analytics(self):
        from codex_analytics import encoded
        a = self.lead()
        eventually(lambda: (self.runtime.agent(a['id']).get('startAttempt') or {}).get('turnId') == a['turnId'])
        samples = [{'threadId': a['threadId'], 'turnId': a['turnId'], 'itemId': 'batch', 'delta': text}
                   for text in ['Первый ', 'абзац.\n\n', 'Second paragraph.']]
        method = 'item/commandExecution/outputDelta'
        def counters():
            with self.runtime.db() as db:
                return tuple(db.execute('SELECT coalesce(sum(count),0),coalesce(sum(bytes),0) FROM analytics_notifications WHERE agent=? AND method=?', (a['id'], method)).fetchone())
        self.runtime.notification({'method': 'item/started', 'params': {
            'threadId': a['threadId'], 'turnId': a['turnId'],
            'item': {'id': 'batch', 'type': 'commandExecution', 'command': 'fixture'}}})
        before = counters()
        count = self.runtime.agent(a['id'])['events']
        self.runtime.notification({'method': method, 'params': {**samples[0], 'delta': ''.join(p['delta'] for p in samples)},
                                   '_studioNotificationSamples': samples})
        eventually(lambda: counters()[0] - before[0] == len(samples))
        after = counters()
        self.assertEqual(after[0] - before[0], len(samples))
        self.assertEqual(after[1] - before[1], sum(len(encoded(p).encode('utf-8')) for p in samples))
        self.assertEqual(self.runtime.agent(a['id'])['events'] - count, len(samples))
        item = next(i for i in self.runtime.transcript(a['id'])['items'] if i['id'].endswith(':batch'))
        self.assertEqual(json.loads(item['text'])['aggregatedOutput'], ''.join(p['delta'] for p in samples))

    def test_transcript_wait_wakes_on_committed_change_and_closes(self):
        a = self.lead()
        # turn/started can arrive before the start response binds the input batch.
        # Wait for that write before asserting that an idle wait has no revision.
        eventually(lambda: (self.runtime.agent(a['id']).get('startAttempt') or {}).get('turnId') == a['turnId'])
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
        # Admit the whole batch before the scheduler can reserve its first input.
        with self.runtime.lock:
            queued = self.runtime.send(a['id'], user_text)
            with self.runtime.db() as db:
                worker = self.runtime.create({'name': 'Reviewer', 'prompt': 'Review', 'role': 'reviewer'}, a['id'], defer=True)
                worker.update(autoWake=True)
                self.runtime.put(db, 'agents', worker)
            self.runtime.chat_message(worker['id'], a['id'], 'Worker result', 'source-result')
        event_id = 'chat:source-result:' + a['id']
        eventually(lambda: any(e['id'] == event_id and e['status'] == 'delivered'
                               for e in read_runtime_state(self.runtime)['events']))
        with self.runtime.db() as db:
            event_text = db.execute("SELECT text FROM runtime_events WHERE id=?", (event_id,)).fetchone()[0]
        items = self.runtime.transcript(a['id'])['items']
        user_record = next(item for item in items if item['id'] == a['id'] + ':' + queued['id'])
        self.assertEqual([r['kind'] for r in user_record['inputs']], ['user', 'agent_message'])
        self.assertEqual(user_record['inputs'][0]['text'], user_text)
        self.assertEqual(user_record['inputs'][1]['text'], event_text)
        self.assertIn(user_text, user_record['text'])
        self.assertIn('[Orchestration event: agent_message]\n' + event_text, user_record['text'])

    def test_pending_user_message_is_visible_before_next_turn(self):
        a = self.lead()
        with self.runtime.lock:
            self.runtime.send(a['id'], 'Queued followup')
            messages = self.runtime.transcript(a['id'])['items']
            self.assertTrue(any(i.get('pending') and i['text'] == 'Queued followup' for i in messages))

    def test_agent_interrupt_names_the_agent_not_the_user(self):
        lead = self.lead()
        self.runtime.server.request({'id': 101, 'method': 'item/tool/call', 'params': {
            'threadId': lead['threadId'], 'callId': 'spawn-one', 'tool': 'orchestration_spawn',
            'arguments': {'agents': [{'name': 'Stop target', 'prompt': 'Wait', 'role': 'reviewer'}]}}})
        eventually(lambda: len(read_runtime_state(self.runtime)['agents']) == 2)
        child = next(a for a in read_runtime_state(self.runtime)['agents'] if a['id'] != lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running'
                   and self.runtime.agent(child['id']).get('turnId'))
        child = self.runtime.agent(child['id'])
        message = {'id': 102, 'method': 'item/tool/call', 'params': {
            'threadId': lead['threadId'], 'callId': 'stop-one', 'tool': 'orchestration_interrupt',
            'arguments': {'agent_id': child['id']}}}
        key = self.runtime.tool_request_key(message)
        self.runtime.server.request(message)
        eventually(lambda: (self.runtime.tool_request(key) or {}).get('outcome') == 'applied')
        self.assertGreater(self.runtime.agent(child['id'])['epoch'], child['epoch'])
        self.assertEqual(self.runtime.agent(child['id'])['status'], 'paused')
        self.assertEqual(self.runtime.agent(child['id'])['error'], 'Stopped by agent ' + lead['name'])

    def test_forty_children_respect_limit_and_wake_finished_parent(self):
        lead = self.lead()
        self.runtime.conversation_settings(lead['id'], {'subagent_concurrency': 5,
            'expected_mode_revision': lead['agentModeRevision'], 'request_id': 'five-worker-limit'})
        self.runtime.server.request({'id': 100, 'method': 'item/tool/call', 'params': {
            'threadId': lead['threadId'], 'callId': 'batch', 'tool': 'orchestration_spawn',
            'arguments': {'agents': [{'name': f'Review {i}', 'prompt': f'Review file {i}', 'role': 'reviewer'} for i in range(40)]}}})
        eventually(lambda: bool(self.runtime.server.responses))
        self.assertTrue(self.runtime.server.responses[0]['result']['success'])
        eventually(lambda: len(self.snapshot()['agents']) == 41)
        self.complete(lead)
        peak = 0
        for _ in range(1000):
            agents = self.snapshot()['agents']
            active = [a for a in agents if a['status'] in ('running', 'starting', 'approval')]
            running_workers = [a for a in active if a['id'] != lead['id']]
            peak = max(peak, len(running_workers))
            self.assertLessEqual(len(running_workers), 5)
            for a in active:
                if a['status'] == 'running':
                    self.complete(a)
            if all(a['status'] == 'completed' for a in self.snapshot()['agents']):
                break
            time.sleep(.01)
        else:
            self.fail('Team did not finish')
        self.assertGreaterEqual(peak, 2)
        # snapshot() can reuse a copy up to 2 s old; wait until the last child results are delivered.
        def delivered():
            with self.runtime.db() as db:
                return not db.execute("SELECT 1 FROM runtime_events WHERE kind='child_result' AND status!='delivered'").fetchone()
        eventually(delivered)
        starts = [p for m,p in self.runtime.server.calls if m == 'turn/start' and p['threadId'] == lead['threadId']]
        self.assertGreater(len(starts), 1)
        steers = [p for m,p in self.runtime.server.calls if m == 'turn/steer' and p['threadId'] == lead['threadId']]
        text = '\n'.join(p['input'][0]['text'] for p in starts[1:] + steers)
        self.assertIn('child_result', text)
        self.assertIn('Review 39', text)
        self.assertIn('Result with evidence', text)

    def test_monitor_output_does_not_call_model_and_exit_wakes_parent(self):
        lead = self.lead()
        self.complete(lead)
        before = sum(m == 'turn/start' for m,p in self.runtime.server.calls)
        starts_balanced = self.track_delivery_starts()
        m = self.runtime.monitor(lead['id'], {'command': 'example-command'}, approved=True)
        eventually(lambda: bool(read_runtime_state(self.runtime)['monitors'][0]['tail']))
        scheduler_tick_finished = threading.Event()
        original_dispatch = self.runtime.dispatch

        def dispatch_after_output(*args, **kwargs):
            result = original_dispatch(*args, **kwargs)
            scheduler_tick_finished.set()
            return result

        with patch.object(self.runtime, 'dispatch', side_effect=dispatch_after_output):
            self.runtime.changed.set()
            self.assertTrue(scheduler_tick_finished.wait(3), 'scheduler tick after monitor output')
        self.assertTrue(starts_balanced(), 'submitted delivery starts did not finish after scheduler tick')
        self.assertEqual(sum(method == 'turn/start' for method,p in self.runtime.server.calls), before)
        self.runtime.server.gate.set()
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        watch = read_runtime_state(self.runtime)['monitors'][0]
        self.assertEqual(watch['exitCode'], 7)
        self.assertEqual(watch['status'], 'failed')
        self.assertEqual(Path(watch['log']).read_text(), 'early output\n')
        turns = [p for method,p in self.runtime.server.calls if method == 'turn/start']
        self.assertIn('monitor_exit', turns[-1]['input'][0]['text'])

    def test_monitor_producer_includes_identity_for_sync(self):
        from codex_sync_entities import project

        lead = self.lead()
        monitor = self.runtime.monitor(lead['id'], {'command': 'producer contract'})
        projected = project('monitor', monitor)
        self.assertEqual(projected['id'], monitor['id'])
        self.assertEqual(projected['agent'], lead['id'])
        self.assertEqual(projected['created'], monitor['created'])
        self.assertEqual(projected['status'], 'approval')

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
        self.assertTrue(all(not self.runtime.agent(a['id'])['autoWake']
                            for a in read_runtime_state(self.runtime)['agents']))
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
        events = [e for e in self.snapshot()['events'] if e['kind'] == 'child_result']
        self.assertEqual(len(events), 1)

    def test_child_result_waits_until_turn_ends_with_empty_input_queue(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        first = self.runtime.agent(child['id'])
        self.runtime.send(child['id'], 'The lead answered your question')
        self.complete(first)
        eventually(lambda: (self.runtime.agent(child['id'])['status'] == 'running'
                            and self.runtime.agent(child['id'])['turnId'] != first['turnId']))
        self.assertFalse(any(e['kind'] == 'child_result' for e in read_runtime_state(self.runtime)['events']))
        final = self.runtime.agent(child['id'])
        self.runtime.server.complete(final['threadId'], final['turnId'], 'Final result after the lead answer')
        eventually(lambda: len([e for e in read_runtime_state(self.runtime)['events'] if e['kind'] == 'child_result']) == 1)
        with self.runtime.db() as db:
            result = db.execute("SELECT text FROM runtime_events WHERE kind='child_result'").fetchone()[0]
        self.assertEqual(json.loads(result)['result'], 'Final result after the lead answer')

    def test_failed_child_result_is_immediate_with_pending_input(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        current = self.runtime.agent(child['id'])
        self.runtime.send(child['id'], 'A late answer')
        self.runtime.server.notify({'method': 'turn/completed', 'params': {'threadId': current['threadId'],
            'turn': {'id': current['turnId'], 'status': 'failed', 'error': {'message': 'failed now'}}}})
        eventually(lambda: len([e for e in read_runtime_state(self.runtime)['events'] if e['kind'] == 'child_result']) == 1)
        with self.runtime.db() as db:
            result = db.execute("SELECT text FROM runtime_events WHERE kind='child_result'").fetchone()[0]
        self.assertEqual(json.loads(result)['status'], 'failed')

    @unittest.expectedFailure  # Product defect: restart retains the transient starting state.
    def test_restart_keeps_pending_events_without_replaying_unknown_work(self):
        lead = self.lead()
        # Reserve the input, but stop before native submission.
        from unittest.mock import patch
        submission_blocked = threading.Event()
        executor = self.runtime.delivery_executor()
        original_submit = executor.submit

        def stop_before_start(function, *args, **kwargs):
            if getattr(function, '__name__', None) == 'start':
                submission_blocked.set()
                return None
            return original_submit(function, *args, **kwargs)

        with patch.object(executor, 'submit', side_effect=stop_before_start):
            self.runtime.send(lead['id'], 'Pending result', 'stable-event')
            self.runtime.dispatch()
            self.assertTrue(submission_blocked.wait(5), 'dispatch did not reach its start hook')
            pending = self.runtime.agent(lead['id'])
            self.assertEqual(pending['status'], 'starting')
            self.assertFalse(pending['startAttempt']['submitted'])
            self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.runtime.agent(lead['id'])['status'], 'queued')
        self.assertIsNone(self.runtime.server)
        self.assertEqual(len([e for e in self.snapshot()['events'] if e['id'] == 'stable-event']), 1)
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
        r = self.snapshot()['requests'][0]
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
        from unittest.mock import patch
        lead = self.lead(tokenBudget=100)
        self.runtime.notification({'method': 'thread/tokenUsage/updated', 'params': {
            'threadId': lead['threadId'], 'tokenUsage': {'total': {'totalTokens': 101}}}})
        eventually(lambda: not self.runtime.agent(lead['id'])['autoWake'])
        with self.assertRaisesRegex(ValueError, 'budget'):
            self.runtime.send(lead['id'], 'Continue')
        current = self.runtime.agent(lead['id'])
        self.runtime.conversation_settings(lead['id'], {'subagent_concurrency': 2,
            'expected_mode_revision': current['agentModeRevision'],
            'request_id': 'budget-concurrency-change'})
        self.runtime.configure(lead['id'], {'tokenBudget': 1000})
        with self.assertRaisesRegex(ValueError, 'budget cannot be verified'):
            self.runtime.send(lead['id'], 'Continue')
        self.assertFalse(self.runtime.agent(lead['id'])['autoWake'])
        home = self.root / 'budget-native-home'
        home.mkdir()
        path = home / 'rollout-fixture.jsonl'
        records = [
            ('session_meta', {'id': lead['threadId']}),
            ('event_msg', {'type': 'task_started', 'turn_id': lead['turnId']}),
            ('token_usage_record', {'thread_id': lead['threadId'], 'turn_id': lead['turnId'],
                'response_id': 'budget-response-exact', 'usage': {'total_tokens': 101},
                'thread_token_usage': {'total_tokens': 101}}),
            ('event_msg', {'type': 'turn_aborted', 'turn_id': lead['turnId']}),
        ]
        path.write_text(''.join(json.dumps({'timestamp': lead['created'] + .001, 'type': kind,
                                          'payload': payload}) + '\n' for kind, payload in records))
        # FakeServer IDs are short; native discovery expects a UUID suffix.
        with patch.object(self.runtime.accounts, 'home', return_value=home), \
                patch.object(self.runtime, '_analytics_rollout_path', return_value=(path, None)):
            self.assertTrue(self.runtime.analytics_history_step())
        with self.runtime.db() as db:
            history = json.loads(db.execute('SELECT record FROM analytics_history WHERE agent=?', (lead['id'],)).fetchone()[0])
        self.assertEqual(history['status'], 'current')
        self.assertTrue(history['context']['requestUsageAvailable'])
        self.assertEqual(self.runtime.agent(lead['id'])['tokensUsed'], 101)
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
        self.assertEqual(len(self.snapshot()['agents']), 1)

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
        eventually(lambda: read_runtime_state(self.runtime)['monitors'][0]['status'] == 'running')
        self.runtime.cancel_monitor(m['id'])
        time.sleep(.1)
        self.assertEqual(read_runtime_state(self.runtime)['monitors'][0]['status'], 'cancelled')
        exits = [e for e in self.snapshot()['events'] if e['kind'] == 'monitor_exit']
        self.assertEqual(len(exits), 1)

    def test_managed_chat_delivery_and_offline_membership(self):
        from codex_canvas import Canvas
        import uuid
        c = Canvas(self.root); c.runtime = self.runtime
        lead = self.lead()
        room = str(uuid.uuid4())
        # Membership validation must work while native startup owns its lock.
        with self.runtime.start_lock:
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
                              'params': {'kind': 'writeStdin', 'threadId': lead['threadId'],
                                         'turnId': lead['turnId'], 'itemId': 'exec-1',
                                         'startedAtMs': 1750000000000, 'approvalId': 'approval-stdin',
                                         'reason': 'Send input to an existing terminal to continue the reviewed command.'}})
        request = read_runtime_state(self.runtime)['requests'][0]
        self.runtime.answer(request['id'], {'decision': 'decline'})
        self.assertEqual(self.runtime.server.responses[-1], {'id': 101, 'result': {'decision': 'decline'}})
        self.runtime.request({'id': 102, 'method': 'item/tool/requestUserInput',
                              'params': {'threadId': lead['threadId'], 'questions': [{'id': 'q', 'question': 'Which file?'}]}})
        request = self.snapshot()['requests'][0]
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

    def test_turn_status_marks_only_items_since_the_start_attempt(self):
        lead = self.lead()
        eventually(lambda: self.runtime.agent(lead['id'])['startAttempt'].get('turnId'))
        lead = self.runtime.agent(lead['id'])
        attempt = lead['startAttempt']
        self.assertEqual(attempt['turnId'], lead['turnId'])
        self.assertLessEqual(attempt['created'], time.time())
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(db, lead['id'], 'current', 'assistant', 'Now', turnId=lead['turnId'])
            self.runtime.item(db, lead['id'], 'old-copy', 'assistant', 'Old', turnId=lead['turnId'])
            db.execute("UPDATE runtime_items SET created=? WHERE id=?", (attempt['created'] - 3600, lead['id'] + ':old-copy'))
        queries = []
        original = self.runtime.db
        from contextlib import contextmanager

        @contextmanager
        def traced(**options):
            with original(**options) as db:
                db.set_trace_callback(queries.append)
                yield db
        self.runtime.db = traced
        try:
            self.complete(self.runtime.agent(lead['id']))
        finally:
            self.runtime.db = original
        with self.runtime.db() as db:
            status = {r[0]: json.loads(r[1]).get('turnStatus') for r in db.execute(
                "SELECT id, record FROM runtime_items WHERE id IN (?,?)",
                (lead['id'] + ':current', lead['id'] + ':old-copy'))}
            update = next(q for q in queries if q.startswith("UPDATE runtime_items ") and "json_set(record,'$.turnStatus'" in q)
            plan = ' '.join(r[3] for r in db.execute('EXPLAIN QUERY PLAN ' + update))
        self.assertEqual(status, {lead['id'] + ':current': 'completed', lead['id'] + ':old-copy': None})
        self.assertIn('created>?', plan)

    def test_lead_waits_only_for_active_children_after_its_turn(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        self.complete(self.runtime.agent(lead['id']))
        self.assertEqual(self.runtime.agent(lead['id'])['status'], 'waiting')
        with self.runtime.lock, self.runtime.db() as db:
            stopped = self.runtime.agent(child['id'], db)
            stopped['autoWake'] = False
            self.runtime.put(db, 'agents', stopped)
            current = self.runtime.agent(lead['id'], db)
            current.update(status='running', inFlight=True, turnId='lead-turn-2')
            self.runtime.put(db, 'agents', current)
        self.complete(self.runtime.agent(lead['id']))
        self.assertEqual(self.runtime.agent(lead['id'])['status'], 'completed')

    def test_child_waiting_for_monitor_reports_only_after_final_result(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        self.runtime.monitor(child['id'], {'command': 'example-command'}, approved=True)
        eventually(lambda: read_runtime_state(self.runtime)['monitors'][0]['status'] == 'running')
        self.complete(child)
        self.assertEqual(self.runtime.agent(child['id'])['status'], 'waiting')
        self.assertFalse(any(e['kind'] == 'child_result' for e in self.snapshot()['events']))
        self.runtime.server.gate.set()
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        self.complete(self.runtime.agent(child['id']))
        self.assertEqual(len([e for e in self.snapshot()['events'] if e['kind'] == 'child_result']), 1)

    def test_timeout_does_not_retry_model_call(self):
        starts_balanced = self.track_delivery_starts()
        start_error_persisted = threading.Event()
        dispatch_changed = threading.Condition()
        dispatch_count = [0]
        original_start_error = self.runtime.start_error
        original_dispatch = self.runtime.dispatch

        def observe_start_error(agent_id, *args, **kwargs):
            result = original_start_error(agent_id, *args, **kwargs)
            if kwargs.get('unknown'):
                start_error_persisted.set()
            return result

        def observe_dispatch(*args, **kwargs):
            result = original_dispatch(*args, **kwargs)
            with dispatch_changed:
                dispatch_count[0] += 1
                dispatch_changed.notify_all()
            return result

        self.runtime.start_error = observe_start_error
        self.runtime.dispatch = observe_dispatch
        self.runtime.connect().fail_start = True
        a = self.runtime.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': 'Finish'})
        self.assertTrue(start_error_persisted.wait(30), 'unknown start outcome was not persisted')
        self.assertEqual(self.runtime.agent(a['id'])['status'], 'waiting')
        self.assertTrue(self.runtime.agent(a['id'])['inFlight'])
        with dispatch_changed:
            before_retry = dispatch_count[0]
        self.runtime.changed.set()
        with dispatch_changed:
            self.assertTrue(dispatch_changed.wait_for(lambda: dispatch_count[0] > before_retry, timeout=30),
                            'scheduler did not finish a tick after unknown start')
            before_send = dispatch_count[0]
        self.runtime.send(a['id'], 'Additional work must wait')
        with dispatch_changed:
            self.assertTrue(dispatch_changed.wait_for(lambda: dispatch_count[0] > before_send, timeout=30),
                            'scheduler did not finish a tick after queued input')
        self.assertTrue(starts_balanced(), 'submitted delivery starts did not finish after queued-input tick')
        self.assertEqual(sum(m == 'turn/start' for m,p in self.runtime.server.calls), 1)
        self.assertEqual(sum(e['status'] == 'uncertain' for e in self.snapshot()['events']), 1)

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
        eventually(lambda: len(self.runtime.task_detail(key).get('tail', '')) == 12000)
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
        self.assertEqual(len(self.snapshot()['tasks']), 1)
        self.assertNotIn('tail', self.snapshot()['tasks'][0])

    def test_native_item_timestamps_drive_saved_tool_duration(self):
        a = self.lead()
        item = {'id': 'timed-tool', 'type': 'mcpToolCall', 'tool': 'search', 'status': 'completed'}
        self.runtime.notification({'method': 'item/started', 'params': {
            'threadId': a['threadId'], 'turnId': a['turnId'], 'startedAtMs': 1000, 'item': item}})
        self.runtime.notification({'method': 'item/completed', 'params': {
            'threadId': a['threadId'], 'turnId': a['turnId'], 'startedAtMs': 1000,
            'completedAtMs': 3425, 'item': item}})
        key = a['id'] + ':timed-tool'
        self.assertEqual(self.runtime.task_detail(key)['durationMs'], 2425)
        saved = next(i for i in self.runtime.transcript(a['id'])['items'] if i['id'] == key)
        payload = json.loads(saved['text'])
        self.assertEqual(payload['startedAtMs'], 1000)
        self.assertEqual(payload['completedAtMs'], 3425)
        self.assertEqual(payload['durationMs'], 2425)
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        restored = next(i for i in self.runtime.transcript(a['id'])['items'] if i['id'] == key)
        self.assertEqual(json.loads(restored['text'])['durationMs'], 2425)

    def test_task_history_is_bounded_but_active_tasks_are_not_hidden(self):
        a = self.lead()
        def event(method, item):
            self.runtime.notification({'method': method, 'params': {'threadId': a['threadId'], 'turnId': a['turnId'], 'item': item}})
        for i in range(105):
            event('item/completed', {'id': f'tool-{i}', 'type': 'mcpToolCall', 'tool': 'test', 'status': 'completed'})
        for i in range(110):
            event('item/started', {'id': f'active-{i}', 'type': 'commandExecution', 'command': 'wait', 'processId': str(i)})
        tasks = self.snapshot()['tasks']
        self.assertEqual(sum(t['status'] == 'running' for t in tasks), 110)
        self.assertEqual(sum(t['status'] == 'completed' for t in tasks), 100)
        self.assertEqual(self.runtime.task_detail(a['id'] + ':tool-0')['status'], 'completed')
        with self.runtime.lock, self.runtime.db() as db:
            a['deletedAt'] = time.time()
            self.runtime.put(db, 'agents', a)
        self.assertEqual(self.snapshot()['tasks'], [])
        with self.assertRaisesRegex(ValueError, 'deleted'):
            self.runtime.task_detail(a['id'] + ':tool-0')

    def test_tool_outcome_is_unknown_after_disconnect_and_restart(self):
        a = self.lead()
        self.runtime.notification({'method': 'item/started', 'params': {'threadId': a['threadId'], 'turnId': a['turnId'],
            'item': {'id': 'tool', 'type': 'mcpToolCall', 'tool': 'watch'}}})
        self.runtime.disconnected()
        self.assertEqual(self.snapshot()['tasks'][0]['status'], 'lost')
        with self.runtime.lock, self.runtime.db() as db:
            task = self.runtime.task_detail(a['id'] + ':tool')
            task.update(status='running')
            self.runtime.put(db, 'tasks', task)
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.snapshot()['tasks'][0]['status'], 'lost')

    def test_cancel_pending_monitor_expires_approval_and_stops_clock(self):
        a = self.lead()
        monitor = self.runtime.monitor(a['id'], {'command': 'pending approval'})
        self.assertEqual(len(self.snapshot()['requests']), 1)
        self.runtime.cancel_monitor(monitor['id'])
        self.assertEqual(self.snapshot()['requests'], [])
        record = read_runtime_state(self.runtime)['monitors'][0]
        self.assertEqual(record['status'], 'cancelled')
        self.assertGreaterEqual(record['finished'], record['created'])

if __name__ == '__main__':
    unittest.main(verbosity=2)

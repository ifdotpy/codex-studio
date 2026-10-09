#!/usr/bin/env python3
"""Two runtimes, signed channel, native history and execution handoff."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import time
import unittest
import uuid
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('move_fixture', Path(__file__).with_name('multi-server-signed-integration.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_agent_move import DEADLINE
from codex_native_errors import NativeRpcError


class HistoryServer(f.fixture.f.FakeServer):
    def __init__(self, root, *args):
        super().__init__(root, *args)
        self.home = Path(root) / 'history-home'
        self.home.mkdir(parents=True, exist_ok=True)
        self.histories = {}
        self.sessions = {}
        self.cli_version = '2.1.291'
        self.sdk_version = '0.3.285'
        self.organization = 'org-fixture'
        self.move_snapshot = True
        self.move_catalog = 'fixture-tools'
        self.native_binary = {'version': 'fixture-native'}

    def call(self, method, params, timeout=60):
        if method == 'claude/moveProof':
            if not self.move_snapshot:
                raise ValueError('The session has no verified saved prompt snapshot')
            return {'snapshotHash': 'fixture-prompt', 'catalog': self.move_catalog,
                    'session': copy.deepcopy(self.sessions[params['threadId']])}
        if method == 'claude/movePreflight':
            if params['proof']['catalog'] != self.move_catalog:
                raise ValueError('The effective target tool definitions differ')
            return {'snapshotHash': params['proof']['snapshotHash'], 'catalog': self.move_catalog}
        if method == 'claude/moveVersions':
            return {'cli': self.cli_version, 'sdk': self.sdk_version}
        if method == 'claude/moveIdentity':
            return {'email': f.OWNER, 'organizationId': self.organization}
        if method == 'claude/moveExport':
            return {'path': str(self.histories[params['threadId']]), 'session': copy.deepcopy(self.sessions[params['threadId']])}
        if method == 'claude/moveImport':
            tid = params['session']['id']
            target = self.home / 'sessions' / (tid + '.jsonl')
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(Path(params['path']).read_bytes())
            self.histories[tid] = target
            self.sessions[tid] = {**copy.deepcopy(params['session']), 'cwd': params['cwd']}
            self.calls.append((method, copy.deepcopy(params)))
            return {'thread': {'id': tid}}
        if method in {'mcpServerStatus/list', 'thread/backgroundTerminals/list'}:
            self.calls.append((method, copy.deepcopy(params)))
            return {'data': []}
        if method == 'thread/unsubscribe':
            self.calls.append((method, copy.deepcopy(params)))
            return {}
        if method in {'thread/archive', 'thread/unarchive'}:
            self.calls.append((method, copy.deepcopy(params)))
            return {}
        if method == 'thread/read':
            path = self.histories[params['threadId']]
            return {'thread': {'id': params['threadId'], 'path': str(path), 'status': {'type': 'idle'}}}
        if method == 'thread/start':
            self.calls.append((method, copy.deepcopy(params)))
            tid = str(uuid.uuid4())
            path = self.home / 'sessions' / ('rollout-' + tid + '.jsonl')
            path.parent.mkdir(parents=True, exist_ok=True)
            records = [
                {'type': 'session_meta', 'payload': {'id': tid, 'base_instructions': {'text': 'Exact native base'}, 'dynamic_tools': params['dynamicTools']}},
                {'type': 'turn_context', 'payload': {'model': params['model'], 'effort': 'medium', 'summary': 'auto', 'developer_instructions': params['developerInstructions']}},
                {'type': 'response_item', 'payload': {'type': 'message', 'role': 'user', 'content': [{'type': 'input_text', 'text': 'café source history'}]}},
            ]
            path.write_bytes(b''.join((json.dumps(row, ensure_ascii=False) + '\n').encode() for row in records))
            self.histories[tid] = path
            self.sessions[tid] = {**copy.deepcopy(params), 'id': tid, 'nativeId': tid, 'started': True, 'turns': []}
            return {'thread': {'id': tid}, 'model': params['model']}
        if method == 'thread/resume' and params.get('path'):
            self.histories[params['threadId']] = Path(params['path'])
        return super().call(method, params, timeout)


class Moves(f.SignedIntegration):
    test_pair_spawn_native_turn_retry_task_fetch_stop_and_revocation = None

    def setUp(self):
        with patch.object(f.fixture.f, 'FakeServer', HistoryServer):
            super().setUp()
        for endpoint in (self.a, self.b):
            rt = endpoint.runtime
            server = rt.connect('default')
            row = {'id': 'default', 'provider': 'codex', 'status': 'ready', 'email': f.OWNER, 'accountId': 'org-fixture', 'home': str(server.home)}
            self.stack.enter_context(patch.object(rt.accounts, 'get', side_effect=lambda _key, row=row: copy.deepcopy(row)))
            self.stack.enter_context(patch.object(rt.accounts, 'list', side_effect=lambda row=row: [copy.deepcopy(row)]))
            self.stack.enter_context(patch.object(rt.accounts, 'home', return_value=server.home))
        invitation = self.a.local({'action': 'create_invite', 'requestId': 'invite-a'})['invitation']
        self.b.local({'action': 'accept_invite', 'invitation': invitation, 'requestId': 'accept-b'})
        self.lead = self.a.runtime.create({'name': 'Lead', 'prompt': '', 'cwd': str(self.a.folder), 'concurrency': 4}, draft=True)
        self.lead = self.a.runtime.prepare(self.lead)

    def finish_move(self, actor, key='move-one', tool='orchestration_move'):
        reply = self.tool(self.a, actor, tool, {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': key}, key)
        self.assertEqual(reply['status'], 'accepted')
        self.assertTrue(self.a.runtime.agent(actor['id']).get('executionMove'))
        self.a.runtime.multi_server().moves().tick()
        # The move contract permits 300 seconds, including signed transport and storage.
        f.fixture.f.eventually(lambda: not self.a.runtime.multi_server().moves().running, timeout=DEADLINE)
        source = self.a.runtime.agent(actor['id'])
        self.assertNotEqual(source.get('executionMove', {}).get('phase'), 'unknown')
        self.assertEqual(source['movedTo']['server'], self.b.server_id)
        try:
            self.drive(lambda: bool(self.b.runtime.agent(actor['id']).get('turnId')))
        except AssertionError:
            target = self.b.runtime.agent(actor['id'])
            self.fail({name: target.get(name) for name in ('status', 'error', 'startAttempt', 'remoteOrigin', 'provider', 'epoch', 'autoWake')})
        return self.b.runtime.agent(actor['id'])

    def test_new_native_session_keeps_teleport_name_and_exact_role_after_resume_and_move(self):
        source = self.a.runtime.agent(self.lead['id'])
        self.assertEqual(source['nativeTeleportTool'], 'orchestration_teleport')
        original = self.a.runtime.tool_definitions(source)
        names = [tool['name'] for tool in original]
        self.assertIn('orchestration_teleport', names)
        self.assertNotIn('orchestration_move', names)
        guidance = self.a.runtime.role_guidance(source)
        self.assertIn('orchestration_teleport', guidance)
        self.a.runtime.loaded.discard(source['id'])
        resumed = self.a.runtime.prepare(source)
        self.assertEqual(resumed['nativeRoleGuidance'], guidance)
        moved = self.finish_move(resumed, 'teleport-name', tool='orchestration_teleport')
        self.assertEqual(moved['nativeTeleportTool'], 'orchestration_teleport')
        self.assertEqual(self.b.runtime.tool_definitions(moved), original)
        self.assertEqual(self.b.runtime.role_guidance(moved), guidance)
        receipt = self.tool(self.b, moved, 'orchestration_request',
            {'action': 'get', 'request_id': 'teleport-name'}, 'teleport-name-receipt')
        self.assertEqual(receipt['outcome'], 'applied')
        self.assertEqual(receipt['tool'], 'orchestration_teleport')
        self.assertEqual(receipt['move']['phase'], 'active')

    def test_legacy_session_keeps_its_name_and_request_receipt_after_move(self):
        with self.a.runtime.lock, self.a.runtime.db() as db:
            old = self.a.runtime.agent(self.lead['id'], db)
            old.pop('nativeTeleportTool', None)
            old.pop('nativeRoleGuidance', None)
            self.a.runtime.put(db, 'agents', old)
        original = self.a.runtime.tool_definitions(old)
        from codex_native_tools import mark_current
        mark_current(old, original)
        with self.a.runtime.db() as db:
            self.a.runtime.put(db, 'agents', old)
        self.assertIn('orchestration_move', [tool['name'] for tool in original])
        path = self.a.runtime.server.histories[old['threadId']]
        rows = [json.loads(line) for line in path.read_text().splitlines()]
        rows[0]['payload']['dynamic_tools'] = original
        rows[1]['payload']['developer_instructions'] = self.a.runtime.new_thread_params(old)['developerInstructions']
        path.write_text(''.join(json.dumps(row) + '\n' for row in rows))
        self.a.runtime.loaded.discard(old['id'])
        old = self.a.runtime.prepare(self.a.runtime.agent(old['id']))
        guidance = self.a.runtime.role_guidance(old)
        moved = self.finish_move(old, 'legacy-name', tool='orchestration_move')
        self.assertEqual(moved['nativeTeleportTool'], 'orchestration_move')
        self.assertEqual(self.b.runtime.tool_definitions(moved), original)
        self.assertEqual(self.b.runtime.role_guidance(moved), guidance)
        receipt = self.tool(self.b, moved, 'orchestration_request',
            {'action': 'get', 'request_id': 'legacy-name'}, 'legacy-name-receipt')
        self.assertEqual(receipt['outcome'], 'applied')
        self.assertEqual(receipt['tool'], 'orchestration_move')

    def test_legacy_tool_catalog_and_role_text_are_byte_identical_to_020fc8a2(self):
        # Captured from the unmodified method at 020fc8a2, with a fixed cwd.
        fingerprints = {
            ('codex', True): '15576daaee9852acc40fd01fa9e95ebc74f640a61f43f26ff388c5f27ea9ac8c',
            ('codex', False): 'c8fe5c59737ccf9200f8e2e69a5ed4ab4ddb1ecc7c1ab961112ba12644097fb2',
            ('claude', True): '60fc0115e5b574a0fcdcd54b66df4a673ede2a40032482fba535dcc9c5a2417b',
            ('claude', False): '8593db37dd80986902c5137d54675a17b4f9692eca55e1640a87581c07afe87d',
        }
        for (provider, lead), expected in fingerprints.items():
            with self.subTest(provider=provider, lead=lead):
                actor = {**self.lead, 'provider': provider, 'isLead': lead, 'cwd': '/fixture/project'}
                actor.pop('nativeTeleportTool', None)
                actor.pop('nativeRoleGuidance', None)
                definitions = self.a.runtime.tool_definitions(actor)
                wire = json.dumps(definitions, separators=(',', ':'), ensure_ascii=False).encode()
                self.assertEqual(hashlib.sha256(wire).hexdigest(), expected)
                params = self.a.runtime.new_thread_params(actor)
                self.assertEqual(params['dynamicTools'], definitions)
                self.assertEqual(actor['nativeTeleportTool'], 'orchestration_move')
                if lead:
                    role = actor['nativeRoleGuidance']
                    body = role.split('\n', 3)[3].removesuffix('\n[End Studio role skill]')
                    self.assertEqual(hashlib.sha256(body.encode()).hexdigest(),
                        '7d2fead8ce2a05e17c7edc20b3dd63c9aecbb6f24ecddf0a9ed38c15b68f9125')
                    self.assertIn('/codex-orchestrator/SKILL.md\n', role)
                    self.assertNotIn('legacy-move-role.md', role)

    def test_saved_role_text_does_not_read_a_later_role_revision(self):
        original = self.a.runtime.new_thread_params(self.lead)
        with patch.object(self.a.runtime, 'role_guidance', return_value='Later role revision') as guidance:
            resumed = self.a.runtime.new_thread_params(self.a.runtime.agent(self.lead['id']))
        guidance.assert_not_called()
        self.assertEqual(resumed['developerInstructions'], original['developerInstructions'])
        self.assertEqual(resumed['dynamicTools'], original['dynamicTools'])

    def test_session_name_and_role_are_saved_before_native_creation(self):
        rt = self.a.runtime
        draft = rt.create({'name': 'New session', 'prompt': '', 'cwd': str(self.a.folder)}, draft=True)
        submit = rt.submit_reserved
        observed = []
        def check(server, method, params, **kwargs):
            if method == 'thread/start':
                saved = rt.agent(draft['id'])
                self.assertEqual(saved['nativeTeleportTool'], 'orchestration_teleport')
                self.assertIn(saved['nativeRoleGuidance'], params['developerInstructions'])
                observed.append(saved['nativeTeleportTool'])
            return submit(server, method, params, **kwargs)
        with patch.object(rt, 'submit_reserved', side_effect=check):
            rt.prepare(draft)
        self.assertEqual(observed, ['orchestration_teleport'])

    def test_old_and_new_teleport_request_keys_use_the_same_recovery_namespace(self):
        keys = []
        for name in ('orchestration_move', 'orchestration_teleport'):
            keys.append(self.a.runtime.tool_request_key({'id': name, 'params': {
                'threadId': self.lead['threadId'], 'callId': name, 'tool': name,
                'arguments': {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'same-request'}}}))
        self.assertEqual(keys[0], keys[1])
        self.assertTrue(keys[0].endswith('move:same-request'))

    def third_server(self):
        executable = self.folder / 'bin' / 'tailscale'
        executable.write_text(executable.read_text().replace('"100.64.0.12"]', '"100.64.0.12", "100.64.0.13"]'))
        with patch.object(f.fixture.f, 'FakeServer', HistoryServer):
            endpoint = f.Endpoint(self.folder / 'c', 'c', 18767, '100.64.0.13')
        self.stack.callback(endpoint.close)
        self.endpoints[endpoint.origin] = endpoint
        rt = endpoint.runtime
        server = rt.connect('default')
        row = {'id': 'default', 'provider': 'codex', 'status': 'ready', 'email': f.OWNER, 'accountId': 'org-fixture', 'home': str(server.home)}
        self.stack.enter_context(patch.object(rt.accounts, 'get', side_effect=lambda _key: copy.deepcopy(row)))
        self.stack.enter_context(patch.object(rt.accounts, 'list', side_effect=lambda: [copy.deepcopy(row)]))
        self.stack.enter_context(patch.object(rt.accounts, 'home', return_value=server.home))
        for home in (self.a, self.b):
            invitation = home.local({'action': 'create_invite', 'requestId': 'invite-c-' + home.name})['invitation']
            endpoint.local({'action': 'accept_invite', 'invitation': invitation, 'requestId': 'accept-c-' + home.name})
        return endpoint

    def drain(self):
        for endpoint in self.endpoints.values():
            with endpoint.runtime.read_db() as db:
                queued = [row[0] for row in db.execute("SELECT id FROM runtime_server_outbox WHERE state='queued' ORDER BY rowid")]
            for key in queued:
                endpoint.runtime.multi_server().deliver(key)

    def next_move(self, source, target, actor, key):
        if actor.get('turnId'):
            source.runtime.server.complete(actor['threadId'], actor['turnId'], 'Ready for the next move')
            self.drain()
        actor = source.runtime.agent(actor['id'])
        reply = self.tool(source, actor, 'orchestration_move',
            {'server': target.server_id, 'cwd': str(target.folder), 'request_id': key}, key)
        self.assertEqual(reply['status'], 'accepted')
        service = source.runtime.multi_server().moves()
        service.tick()
        f.fixture.f.eventually(lambda: not service.running)
        self.assertFalse(source.runtime.agent(actor['id']).get('executionMove'), source.runtime.agent(actor['id']).get('error'))
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            self.drain()
            target.runtime.dispatch()
            moved = target.runtime.agent(actor['id'])
            if moved.get('turnId'):
                return moved
            time.sleep(0.01)
        self.fail({field: moved.get(field) for field in ('status', 'error', 'remoteOrigin')})

    def test_three_servers_keep_one_home_route_and_allow_an_intermediate_return(self):
        third = self.third_server()
        moved = self.finish_move(self.lead)
        old_link = moved['remoteOrigin']['link']
        moved = self.next_move(self.b, third, moved, 'move-to-third')
        home = self.a.runtime.agent(moved['id'])
        self.assertEqual(home['remoteWorker'], {'server': third.server_id, 'link': moved['remoteOrigin']['link']})
        self.assertEqual(moved['remoteOrigin']['home'], self.a.server_id)
        with self.b.runtime.db() as db:
            old_parent = self.b.runtime.agent(moved['id'], db)
            self.b.runtime.enqueue_recovery_event(db, old_parent, 'child_result', json.dumps({'agent_id': 'old-child', 'result': 'Child on the intermediate server'}), 'third-child-result')
        self.drain()
        self.drain()
        self.assertEqual(len(self.events(third, moved['id'], 'child_result')), 1)
        with self.a.runtime.read_db() as db:
            link = self.a.runtime.multi_server().link(db, old_link)
        stale = self.a.runtime.multi_server().receive(self.b.server_id, {'requestId': 'stale-route-state', 'action': 'state',
            'payload': {'worker': moved['id'], 'link': old_link, 'parentEpoch': link['parentEpoch'], 'sequence': 99999,
                        'record': {'epoch': 0, 'status': 'failed', 'inFlight': False}}})
        self.assertEqual(stale['outcome'], 'not_applied')
        task = self.tool(third, moved, 'orchestration_task', {'action': 'create', 'title': 'Canonical home task'}, 'third-task')
        self.assertTrue(task['id'])
        restored = self.next_move(third, self.b, moved, 'return-to-intermediate')
        self.assertEqual(restored['remoteOrigin']['home'], self.a.server_id)
        self.assertEqual(self.a.runtime.agent(moved['id'])['remoteWorker']['server'], self.b.server_id)
        self.assertEqual(len(restored['executionArchives']), 3)

    def test_move_preserves_an_existing_control_epoch(self):
        with self.a.runtime.db() as db:
            self.lead.update(epoch=3, turnEpoch=3)
            self.a.runtime.put(db, 'agents', self.lead)
        moved = self.finish_move(self.lead)
        self.assertEqual(moved['remoteControlEpoch'], 3)
        with self.a.runtime.db() as db:
            home = self.a.runtime.agent(moved['id'], db)
            self.a.runtime.enqueue_recovery_event(db, home, 'agent_message', 'Message after an earlier resume', 'nonzero-control-event')
        self.drain()
        self.assertEqual(len(self.events(self.b, moved['id'], 'agent_message', 'Message after an earlier resume')), 1)

    def test_home_stop_during_return_import_never_starts_target_input(self):
        moved = self.finish_move(self.lead)
        self.b.runtime.server.complete(moved['threadId'], moved['turnId'], 'Ready to return')
        self.drain()
        moved = self.b.runtime.agent(moved['id'])
        original = self.a.runtime.server.call
        def stop_during_import(method, params, **kwargs):
            result = original(method, params, **kwargs)
            if method == 'thread/resume' and params.get('threadId') == moved['threadId']:
                self.a.runtime.stop(moved['id'], True, reason='Stopped during the return import')
            return result
        with patch.object(self.a.runtime.server, 'call', side_effect=stop_during_import):
            self.tool(self.b, moved, 'orchestration_move',
                {'server': self.a.server_id, 'cwd': str(self.a.folder), 'request_id': 'stopped-return'}, 'stopped-return')
            service = self.b.runtime.multi_server().moves()
            service.tick()
            f.fixture.f.eventually(lambda: not service.running)
        home = self.a.runtime.agent(moved['id'])
        self.assertFalse(home['autoWake'])
        self.assertIsNone(home['threadId'])
        self.assertFalse([1 for method, _params in self.a.runtime.server.calls if method == 'turn/start'])

    def test_lost_home_redirect_reply_reconciles_without_another_native_import(self):
        third = self.third_server()
        moved = self.finish_move(self.lead)
        self.b.runtime.server.complete(moved['threadId'], moved['turnId'], 'Ready for the third server')
        self.drain()
        moved = self.b.runtime.agent(moved['id'])
        self.drop_action = 'move_home_relocate'
        self.tool(self.b, moved, 'orchestration_move',
            {'server': third.server_id, 'cwd': str(third.folder), 'request_id': 'lost-home-redirect'}, 'lost-home-redirect')
        service = self.b.runtime.multi_server().moves()
        service.tick()
        f.fixture.f.eventually(lambda: not service.running)
        self.assertEqual(self.b.runtime.agent(moved['id'])['executionMove']['phase'], 'unknown')
        self.assertEqual(self.a.runtime.agent(moved['id'])['remoteWorker']['server'], third.server_id)
        self.assertFalse(third.runtime.agent(moved['id']).get('autoWake'))
        self.assertTrue(third.runtime.agent(moved['id'])['moveImportPending'])
        service.tick()
        f.fixture.f.eventually(lambda: not service.running)
        self.assertFalse(self.b.runtime.agent(moved['id']).get('executionMove'))
        self.assertFalse(third.runtime.agent(moved['id']).get('moveImportPending'))
        self.assertEqual(len([1 for method, params in third.runtime.server.calls if method == 'thread/resume' and params.get('path')]), 1)
        old = self.b.runtime.agent(moved['id'])
        origin = old['remoteOrigin']
        with self.b.runtime.read_db() as db:
            link = self.b.runtime.multi_server().link(db, origin['link'])
        late = self.b.runtime.multi_server().receive(self.a.server_id, {'requestId': 'late-home-input', 'action': 'input',
            'payload': {'link': origin['link'], 'worker': old['id'], 'kind': 'followup', 'text': 'Late input for the current server',
                        'parentEpoch': link['parentEpoch'], 'controlEpoch': 0, 'epoch': 0}})
        self.assertEqual(late['outcome'], 'applied')
        self.drain()
        self.drain()
        self.assertEqual(len(self.events(third, moved['id'], 'followup', 'Late input for the current server')), 1)
        self.assertIsNone(self.b.runtime.agent(moved['id'])['threadId'])

    def test_lead_move_keeps_uuid_history_and_exact_parameters(self):
        old_bytes = self.a.runtime.server.histories[self.lead['threadId']].read_bytes()
        original_tools = self.a.runtime.tool_definitions(self.lead)
        moved = self.finish_move(self.lead)
        self.assertEqual(moved['threadId'], self.lead['threadId'])
        self.assertEqual(self.b.runtime.server.histories[moved['threadId']].read_bytes(), old_bytes)
        self.assertEqual(self.b.runtime.tool_definitions(moved), original_tools)
        self.assertFalse(moved['worktree'])
        self.assertEqual(moved['cwd'], str(self.b.folder))
        starts = [p for m, p in self.b.runtime.server.calls if m == 'turn/start']
        self.assertEqual(starts[-1]['cwd'], str(self.b.folder))
        self.assertIsNone(self.a.runtime.agent(self.lead['id'])['threadId'])
        receipt = self.tool(self.b, moved, 'orchestration_request', {'action': 'get', 'request_id': 'move-one'}, 'target-move-receipt')
        self.assertEqual(receipt['move']['phase'], 'active')
        self.assertEqual(receipt['operationResult']['agentId'], moved['id'])

    def test_exact_retry_on_target_does_not_move_or_start_another_turn(self):
        moved = self.finish_move(self.lead)
        args = {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'move-one'}
        reply = self.tool(self.b, moved, 'orchestration_move', args, 'retry-on-target')
        self.assertEqual(reply['agentId'], moved['id'])
        self.assertEqual(len([1 for m, _ in self.b.runtime.server.calls if m == 'turn/start']), 1)

    def test_review_first_target_cache_is_visible_on_the_source_receipt(self):
        moved = self.finish_move(self.lead)
        params = {'threadId': moved['threadId'], 'turnId': moved['turnId'],
                  'tokenUsage': {'last': {'inputTokens': 100, 'cachedInputTokens': 900,
                                         'cacheWriteInputTokens': 30, 'outputTokens': 5, 'totalTokens': 1035}}}
        self.b.runtime.notification({'method': 'thread/tokenUsage/updated', 'params': params})
        receipt = self.tool(self.b, moved, 'orchestration_request',
            {'action': 'get', 'request_id': 'move-one'}, 'target-cache-receipt')
        self.assertEqual(receipt['move']['firstTurnCache']['cachedInputTokens'], 900)
        status = self.a.runtime.multi_server().moves().status(moved['id'], 'move-one')
        self.assertEqual(status['firstTurnCache']['cachedInputTokens'], 900)
        self.assertEqual(status['firstTurnCache']['cacheWriteInputTokens'], 30)
        self.assertEqual(status['acceptance']['cacheProof'], 'exact_native_history_and_catalog')

    def test_review_target_uuid_owner_is_checked_during_preflight(self):
        victim = self.b.runtime.prepare(self.b.runtime.create(
            {'name': 'Victim', 'prompt': '', 'cwd': str(self.b.folder)}, draft=True))
        with self.a.runtime.db() as db:
            attacker = self.a.runtime.agent(self.lead['id'], db)
            attacker['threadId'] = victim['threadId']
            self.a.runtime.put(db, 'agents', attacker)
        with self.assertRaisesRegex(ValueError, 'native identity.*owned'):
            self.a.runtime.multi_server().moves().start(attacker,
                {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'victim-preflight'}, 'victim-preflight')
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))
        self.assertFalse([1 for method, _ in self.b.runtime.server.calls if method == 'thread/resume'])

    def test_review_native_uuid_cannot_belong_to_another_target_agent(self):
        victim = self.b.runtime.create({'name': 'Victim', 'prompt': '', 'cwd': str(self.b.folder)}, draft=True)
        victim = self.b.runtime.prepare(victim)
        args = {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'uuid-collision'}
        service = self.a.runtime.multi_server().moves()
        reply = service.start(self.lead, args, 'uuid-collision')
        with self.a.runtime.read_db() as db:
            operation = service._get(db, reply['requestId'])
        descriptor = service._export(operation)
        descriptor['nativeThread'] = victim['threadId']
        descriptor['agent']['threadId'] = victim['threadId']
        descriptor['preflight']['nativeThread'] = victim['threadId']
        target = self.b.runtime.multi_server().moves()
        with self.assertRaisesRegex((ValueError, PermissionError), 'native.*identity|native.*owned'):
            target.receive(self.a.server_id, 'move_begin', descriptor, 'attack-begin')
        self.assertFalse((Path(self.b.runtime.root) / 'agent-moves' / reply['requestId']).exists())
        self.assertFalse([1 for method, _ in self.b.runtime.server.calls if method == 'thread/resume'])

    def test_review_redirect_before_activation_keeps_child_result_pending(self):
        from codex_multi_server_orchestration import identity
        service = self.a.runtime.multi_server().moves()
        original = service._exchange
        observed = []
        def deliver_child_first(server, action, payload, key):
            if action == 'move_activate':
                with self.a.runtime.db() as db:
                    actor = self.a.runtime.agent(self.lead['id'], db)
                    self.a.runtime.enqueue(db, actor, 'child_result', 'Result during activation', 'activation-child')
                receipt = self.a.runtime.multi_server().deliver(identity('input', 'activation-child'))
                with self.b.runtime.read_db() as db:
                    rows = [dict(row) for row in db.execute("SELECT * FROM runtime_events WHERE agent=? AND kind='child_result' AND text=?", (self.lead['id'], 'Result during activation'))]
                observed.append((receipt, rows))
            return original(server, action, payload, key)
        with patch.object(service, '_exchange', side_effect=deliver_child_first):
            moved = self.finish_move(self.lead)
        self.assertEqual(observed[0][0]['outcome'], 'applied', observed)
        self.assertEqual(len(observed[0][1]), 1, observed)
        self.assertEqual(observed[0][1][0]['status'], 'pending', observed)
        self.assertEqual(len(self.events(self.b, moved['id'], 'child_result', 'Result during activation')), 1)
        self.assertTrue(moved['autoWake'])

    def test_review_lost_begin_and_chunk_replies_recover_the_saved_transfer(self):
        service = self.a.runtime.multi_server().moves()
        original = service._exchange
        lost = {'move_begin', 'move_chunk'}
        def lost_reply(server, action, payload, key):
            result = original(server, action, payload, key)
            if action in lost:
                lost.remove(action)
                raise RuntimeError('Lost saved transfer receipt')
            return result
        reply = self.tool(self.a, self.lead, 'orchestration_move',
            {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'lost-transfer'}, 'lost-transfer')
        with patch.object(service, '_exchange', side_effect=lost_reply):
            for _ in range(3):
                service.tick()
                f.fixture.f.eventually(lambda: not service.running, timeout=DEADLINE)
        actor = self.a.runtime.agent(self.lead['id'])
        self.assertFalse(actor.get('executionMove'), actor.get('error'))
        self.assertEqual(actor['movedTo']['server'], self.b.server_id)
        self.assertEqual(len([1 for method, _ in self.b.runtime.server.calls if method == 'thread/resume']), 1)

    def test_review_unknown_codex_account_identity_requires_explicit_approval(self):
        source = self.a.runtime.accounts.get('default')
        target = self.b.runtime.accounts.get('default')
        source.update(email=None, accountId=None)
        target.update(email=None, accountId=None)
        service = self.a.runtime.multi_server().moves()
        with patch.object(self.a.runtime.accounts, 'get', return_value=source), \
             patch.object(self.b.runtime.accounts, 'get', return_value=target), \
             patch.object(self.b.runtime.accounts, 'list', return_value=[target]):
            with self.assertRaisesRegex(ValueError, 'account identity.*cache reset'):
                service.start(self.lead, {'server': self.b.server_id, 'cwd': str(self.b.folder),
                              'request_id': 'unknown-account'}, 'unknown-account')
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_review_different_codex_identity_refuses_without_explicit_approval(self):
        row = self.b.runtime.accounts.get('default')
        row['accountId'] = 'different-organization'
        with patch.object(self.b.runtime.accounts, 'list', return_value=[row]), patch.object(self.b.runtime.accounts, 'get', return_value=row):
            with self.assertRaisesRegex(ValueError, 'cache|account identity'):
                self.a.runtime.multi_server().moves().start(self.lead,
                    {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'cache-refuse'}, 'cache-refuse')
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_children_stay_and_report_to_the_moved_lead(self):
        child = self.a.runtime.create({'name': 'Child', 'prompt': 'Check', 'role': 'reviewer'}, parent=self.lead['id'], defer=True)
        child['autoWake'] = True
        with self.a.runtime.db() as db:
            self.a.runtime.put(db, 'agents', child)
        child = self.a.runtime.prepare(child)
        moved = self.finish_move(self.lead)
        self.assertIsNone(self.a.runtime.agent(child['id']).get('movedTo'))
        with self.a.runtime.db() as db:
            parent = self.a.runtime.agent(self.lead['id'], db)
            self.a.runtime.enqueue_recovery_event(db, parent, 'child_result', json.dumps({'agent_id': child['id'], 'result': 'Original child result'}), 'original-child-result')
        self.drain()
        self.assertEqual(len(self.events(self.b, moved['id'], 'child_result')), 1)

    def test_lost_prepare_reply_recovers_the_committed_import(self):
        self.drop_action = 'move_prepare'
        reply = self.tool(self.a, self.lead, 'orchestration_move', {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'lost-import'}, 'lost-import')
        service = self.a.runtime.multi_server().moves()
        service.tick()
        f.fixture.f.eventually(lambda: not service.running)
        self.assertEqual(self.a.runtime.agent(self.lead['id'])['executionMove']['phase'], 'unknown')
        service.tick()
        f.fixture.f.eventually(lambda: not service.running)
        self.assertEqual(self.a.runtime.agent(self.lead['id'])['movedTo']['server'], self.b.server_id)
        self.assertEqual(len([1 for m, p in self.b.runtime.server.calls if m == 'thread/resume' and p.get('path')]), 1)
        self.drive(lambda: bool(self.b.runtime.agent(self.lead['id']).get('turnId')))
        self.assertEqual(len([1 for m, p in self.b.runtime.server.calls if m == 'turn/start']), 1)

    def test_offline_target_never_freezes_or_starts_the_agent(self):
        with patch.object(self.a.runtime.multi_server().transport, 'request', side_effect=TimeoutError):
            with self.assertRaisesRegex(RuntimeError, 'unknown'):
                self.a.runtime.multi_server().moves().start(self.lead,
                    {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'offline'}, 'offline')
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))
        self.assertFalse([1 for m, p in self.b.runtime.server.calls if m == 'turn/start'])

    def test_background_commands_other_tools_and_missing_folder_refuse(self):
        service = self.a.runtime.multi_server().moves()
        args = {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'refuse'}
        with patch.object(self.a.runtime.server, 'call', side_effect=lambda m, p, **kw: {'data': [{'id': 'background'}]}):
            with self.assertRaisesRegex(ValueError, 'background'):
                service.start(self.lead, args, 'refuse')
        with self.a.runtime.db() as db:
            current = self.a.runtime.agent(self.lead['id'], db)
            current['activeTools'] = [{'id': 'other', 'name': 'exec_command'}]
            self.a.runtime.put(db, 'agents', current)
        with self.assertRaisesRegex(ValueError, 'other active tools'):
            service.start(self.lead, args, 'refuse')
        with self.a.runtime.db() as db:
            current['activeTools'] = []
            current['pendingSettings'] = {'model': 'next-model'}
            self.a.runtime.put(db, 'agents', current)
        with self.assertRaisesRegex(ValueError, 'settings'):
            service.start(self.lead, args, 'refuse')
        with self.a.runtime.db() as db:
            current.pop('pendingSettings')
            self.a.runtime.put(db, 'agents', current)
            self.a.runtime.multi_server().queue(db, self.b.server_id, 'move_spawn', {'worker': current['id']}, 'pending-move-spawn')
        with self.assertRaisesRegex(ValueError, 'pending remote child spawn'):
            service.start(self.lead, args, 'refuse')
        with self.a.runtime.db() as db:
            db.execute("UPDATE runtime_server_outbox SET state='complete' WHERE id='pending-move-spawn'")
        with self.assertRaisesRegex(ValueError, 'existing absolute folder'):
            service.start(self.lead, {**args, 'cwd': str(self.b.folder / 'missing')}, 'refuse')

    def test_uncertain_native_import_never_repeats_resume_or_starts_input(self):
        original = self.b.runtime.server.call
        def crash_after_resume(method, params, timeout=60):
            result = original(method, params, timeout)
            if method == 'thread/resume' and params.get('path'):
                raise RuntimeError('Crash after native import')
            return result
        with patch.object(self.b.runtime.server, 'call', side_effect=crash_after_resume):
            reply = self.tool(self.a, self.lead, 'orchestration_move',
                {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'uncertain-import'}, 'uncertain-import')
            service = self.a.runtime.multi_server().moves()
            service.tick()
            f.fixture.f.eventually(lambda: not service.running)
        self.assertEqual(self.a.runtime.agent(self.lead['id'])['executionMove']['phase'], 'unknown')
        service.tick()
        f.fixture.f.eventually(lambda: not service.running)
        self.assertEqual(len([1 for method, params in self.b.runtime.server.calls if method == 'thread/resume' and params.get('path')]), 1)
        self.assertFalse([1 for method, _ in self.b.runtime.server.calls if method == 'turn/start'])
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('movedTo'))

    def test_uncertain_archive_never_repeats_or_starts_input(self):
        native = self.a.runtime.server
        original = native.call
        def lost_archive_reply(method, params, timeout=60):
            result = original(method, params, timeout)
            if method == 'thread/archive':
                raise RuntimeError('Lost native archive reply')
            return result
        service = self.a.runtime.multi_server().moves()
        with patch.object(native, 'call', side_effect=lost_archive_reply):
            self.tool(self.a, self.lead, 'orchestration_move',
                {'server': 'local', 'cwd': str(self.a.folder), 'request_id': 'archive-unknown'}, 'archive-unknown')
            service.tick()
            f.fixture.f.eventually(lambda: not service.running)
        self.assertEqual(self.a.runtime.agent(self.lead['id'])['executionMove']['phase'], 'unknown')
        service.tick()
        f.fixture.f.eventually(lambda: not service.running)
        self.assertEqual(len([1 for method, _ in native.calls if method == 'thread/archive']), 1)
        self.assertFalse([1 for method, _ in native.calls if method == 'turn/start'])
        self.assertFalse(self.events(self.a, self.lead['id'], 'followup'))

    def test_current_turn_finishes_before_the_move_exports(self):
        source = self.a.runtime.server
        turn = source.call('turn/start', {'threadId': self.lead['threadId'], 'input': []})['turn']
        with self.a.runtime.db() as db:
            actor = self.a.runtime.agent(self.lead['id'], db)
            actor.update(inFlight=True, turnId=turn['id'], status='running', turnEpoch=actor['epoch'])
            self.a.runtime.put(db, 'agents', actor)
        self.tool(self.a, actor, 'orchestration_move',
            {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'turn-boundary'}, 'turn-boundary')
        service = self.a.runtime.multi_server().moves()
        service.tick()
        self.assertFalse(service.running)
        self.assertFalse([1 for method, params in source.calls if method == 'thread/unsubscribe'])
        source.complete(actor['threadId'], turn['id'], 'Finish source turn')
        service.tick()
        f.fixture.f.eventually(lambda: not service.running)
        self.assertEqual(self.a.runtime.agent(actor['id'])['movedTo']['server'], self.b.server_id)

    def test_review_every_uncertain_phase_stops_at_the_recovery_deadline(self):
        service = self.a.runtime.multi_server().moves()
        accepted = service.start(self.lead, {'server': self.b.server_id, 'cwd': str(self.b.folder),
            'request_id': 'recovery-deadline'}, 'recovery-deadline')
        with self.a.runtime.db() as db:
            operation = service._get(db, accepted['requestId'])
        for phase in ('waiting', 'exported', 'importing', 'prepared', 'redirected'):
            with self.subTest(phase=phase):
                operation.update(phase='unknown', uncertainPhase=phase, deadline=time.time() - 1)
                with self.a.runtime.db() as db:
                    service._save(db, operation)
                with patch.object(service, '_exchange', return_value={'phase': 'ready'}) as exchange, \
                     patch.object(service, '_run') as run:
                    service._reconcile(operation)
                    exchange.assert_not_called()
                    run.assert_not_called()
                with self.a.runtime.read_db() as db:
                    held = service._get(db, operation['id'])
                self.assertTrue(held.get('recoveryExpiredAt'))
                self.assertEqual(held['phase'], 'unknown')
                self.assertIn('deadline', held['error'])

    def test_review_expired_target_cannot_accept_late_activation(self):
        service = self.a.runtime.multi_server().moves()
        original = service._exchange
        def before_activation(server, action, payload, key):
            if action == 'move_activate':
                raise TimeoutError('Hold the activation receipt')
            return original(server, action, payload, key)
        accepted = service.start(self.lead, {'server': self.b.server_id, 'cwd': str(self.b.folder),
            'request_id': 'late-activation'}, 'late-activation')
        with patch.object(service, '_exchange', side_effect=before_activation):
            service.tick()
            f.fixture.f.eventually(lambda: not service.running, timeout=DEADLINE)
        target = self.b.runtime.multi_server().moves()
        from codex_multi_server_orchestration import identity
        with self.b.runtime.db() as db:
            operation = target._get(db, identity(accepted['requestId'], 'target'))
            self.assertEqual(operation['phase'], 'ready')
            operation['deadline'] = time.time() - 1
            target._save(db, operation)
        target.tick()
        with self.assertRaisesRegex(ValueError, 'deadline'):
            original(self.b.server_id, 'move_activate', {'move': accepted['requestId']},
                     identity(accepted['requestId'], 'activate'))
        self.assertEqual(self.b.runtime.agent(self.lead['id'])['executionMove']['phase'], 'unknown')
        self.assertFalse(self.b.runtime.agent(self.lead['id'])['autoWake'])
        self.assertFalse([1 for method, _ in self.b.runtime.server.calls if method == 'turn/start'])

    def test_review_expired_source_never_resumes_any_transfer_phase(self):
        service = self.a.runtime.multi_server().moves()
        accepted = service.start(self.lead, {'server': self.b.server_id, 'cwd': str(self.b.folder),
            'request_id': 'expired-source'}, 'expired-source')
        with self.a.runtime.read_db() as db:
            operation = service._get(db, accepted['requestId'])
        for phase in ('waiting', 'exported', 'importing', 'prepared', 'redirected'):
            with self.subTest(phase=phase):
                operation.update(phase=phase, deadline=time.time() - 1)
                with self.a.runtime.db() as db:
                    service._save(db, operation)
                with patch.object(service, '_export') as export, patch.object(service, '_exchange') as exchange:
                    service._run(operation['id'])
                    export.assert_not_called()
                    exchange.assert_not_called()
                self.assertEqual(self.a.runtime.agent(self.lead['id'])['executionMove']['phase'], 'unknown')

    def test_source_turn_deadline_holds_the_move_without_target_input(self):
        source = self.a.runtime.server
        turn = source.call('turn/start', {'threadId': self.lead['threadId'], 'input': []})['turn']
        with self.a.runtime.db() as db:
            actor = self.a.runtime.agent(self.lead['id'], db)
            actor.update(inFlight=True, turnId=turn['id'], status='running', turnEpoch=actor['epoch'])
            self.a.runtime.put(db, 'agents', actor)
        reply = self.tool(self.a, actor, 'orchestration_move',
            {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'source-deadline'}, 'source-deadline')
        service = self.a.runtime.multi_server().moves()
        with self.a.runtime.db() as db:
            operation = service._get(db, reply['requestId'])
            self.assertAlmostEqual(operation['deadline'] - operation['created'], DEADLINE, delta=1)
            operation['deadline'] = time.time() - 1
            service._save(db, operation)
        service.tick()
        held = self.a.runtime.agent(actor['id'])
        self.assertEqual(held['executionMove']['phase'], 'unknown')
        self.assertIn('300 seconds', held['error'])
        self.assertFalse(service.running)
        self.assertFalse(held.get('movedTo'))
        self.assertFalse([1 for method, _ in source.calls if method == 'thread/unsubscribe'])
        self.assertFalse([1 for method, _ in self.b.runtime.server.calls if method in {'thread/resume', 'turn/start'}])
        service.tick()
        f.fixture.f.eventually(lambda: not service.running, timeout=DEADLINE)
        self.assertFalse(service.running)
        self.assertFalse([1 for method, _ in self.b.runtime.server.calls if method in {'thread/resume', 'turn/start'}])

    def test_crash_after_export_resumes_only_the_saved_transfer(self):
        service = self.a.runtime.multi_server().moves()
        args = {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'export-crash'}
        reply = self.tool(self.a, self.lead, 'orchestration_move', args, 'export-crash')
        with self.a.runtime.read_db() as db:
            operation = service._get(db, reply['requestId'])
        descriptor = service._export(operation)
        operation.update(phase='exported', descriptor=descriptor)
        with self.a.runtime.db() as db:
            service._save(db, operation)
        # Recreate the move service from durable rows at the export/import boundary.
        from codex_agent_move import AgentMoves
        restored = AgentMoves(self.a.runtime.multi_server())
        self.a.runtime.multi_server()._move_service = restored
        with patch.object(restored, '_export', side_effect=AssertionError('Must use the saved export')):
            restored.tick()
            f.fixture.f.eventually(lambda: not restored.running)
        self.assertEqual(self.a.runtime.agent(self.lead['id'])['movedTo']['server'], self.b.server_id)
        self.assertEqual(len([1 for m, p in self.b.runtime.server.calls if m == 'thread/resume' and p.get('path')]), 1)

    def test_moved_lead_can_send_to_original_child_and_read_task_board(self):
        task = self.tool(self.a, self.lead, 'orchestration_task', {'action': 'create', 'title': 'Original board'}, 'original-board')
        child = self.a.runtime.create({'name': 'Child', 'prompt': 'Check', 'role': 'reviewer'}, parent=self.lead['id'], defer=True)
        child['autoWake'] = True
        with self.a.runtime.db() as db:
            self.a.runtime.put(db, 'agents', child)
        child = self.a.runtime.prepare(child)
        moved = self.finish_move(self.lead)
        self.tool(self.b, moved, 'orchestration_send', {'agent_id': child['id'][:8], 'text': 'Continue original child', 'request_id': 'old-child-send'}, 'old-child-send')
        self.assertEqual(len(self.events(self.a, child['id'], text='Continue original child')), 1)
        fetched = self.tool(self.b, moved, 'orchestration_task', {'action': 'get', 'task_id': task['id']}, 'old-task-get')
        self.assertEqual(fetched['title'], 'Original board')
        inspected = self.tool(self.b, moved, 'orchestration_agent_manage', {'action': 'inspect', 'agent_id': child['id'][:8]}, 'old-child-inspect')
        self.assertEqual(inspected['agent']['id'], child['id'])

    def test_spawn_after_move_uses_the_same_visible_parent(self):
        moved = self.finish_move(self.lead)
        reply = self.tool(self.b, moved, 'orchestration_spawn', {'agents': [
            {'name': 'New child', 'prompt': 'Check target', 'role': 'reviewer', 'cwd': str(self.b.folder)}]}, 'spawn-after-move')
        child = self.b.runtime.agent(reply['agents'][0]['id'])
        self.assertEqual(child['parentId'], moved['id'])
        self.assertEqual(child['rootId'], moved['rootId'])
        self.assertEqual(self.a.runtime.agent(child['id'])['parentId'], moved['id'])

    def test_native_review_after_move_keeps_the_home_parent_link(self):
        self.git(self.b.folder, 'init', '-q', '-b', 'main')
        moved = self.finish_move(self.lead)
        review = self.tool(self.b, moved, 'orchestration_review', {'cwd': str(self.b.folder)}, 'moved-review')
        child = self.b.runtime.agent(review['agentId'])
        proxy = self.a.runtime.agent(child['id'])
        self.assertEqual(proxy['parentId'], self.lead['id'])
        self.assertEqual(proxy['remoteWorker']['server'], self.b.server_id)
        self.assertEqual(child['remoteOrigin']['home'], self.a.server_id)
        self.assertTrue(child['autoWake'])
        self.assertFalse(child.get('moveReviewPending'))
        retry = self.tool(self.b, moved, 'orchestration_review', {'cwd': str(self.b.folder)}, 'moved-review')
        self.assertEqual(retry['agentId'], child['id'])

    def claude_source(self):
        for endpoint in (self.a, self.b):
            rt = endpoint.runtime
            row = rt.accounts.get('default')
            row['provider'] = 'claude'
            rt.accounts.data['accounts']['default'] = copy.deepcopy(row)
            self.stack.enter_context(patch.object(rt.accounts, 'get', side_effect=lambda _key, row=row: copy.deepcopy(row)))
            self.stack.enter_context(patch.object(rt.accounts, 'list', side_effect=lambda row=row: [copy.deepcopy(row)]))
        with self.a.runtime.db() as db:
            self.lead['provider'] = 'claude'
            self.a.runtime.put(db, 'agents', self.lead)

    def test_claude_move_resumes_the_same_native_session(self):
        self.claude_source()
        original = self.a.runtime.server.histories[self.lead['threadId']].read_bytes()
        moved = self.finish_move(self.lead)
        self.assertEqual(moved['provider'], 'claude')
        self.assertEqual(self.b.runtime.server.histories[moved['threadId']].read_bytes(), original)
        self.assertEqual(self.b.runtime.server.sessions[moved['threadId']]['nativeId'], self.lead['threadId'])
        self.assertEqual(len([1 for m, _ in self.b.runtime.server.calls if m == 'claude/moveImport']), 1)

    def test_claude_version_mismatch_names_versions_and_refuses(self):
        self.claude_source()
        self.b.runtime.server.cli_version = '2.1.177'
        with self.assertRaisesRegex(ValueError, r'Source CLI 2.1.291.*target CLI 2.1.177.*Update'):
            self.a.runtime.multi_server().moves().start(self.lead,
                {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'version-refuse'}, 'version-refuse')
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_refusal_names_all_codex_fields_without_private_catalog_values(self):
        source = self.a.runtime.server
        target = self.b.runtime.server
        source.native_binary['version'] = '0.160.1'
        target.native_binary['version'] = '0.161.0'
        source_call, target_call = source.call, target.call
        def status(original, names, method, params, timeout=60):
            if method == 'mcpServerStatus/list':
                return {'data': [{'name': name, 'tools': {'inspect': {'name': 'inspect',
                    'description': 'private-description', 'inputSchema': {'private': 'schema-secret'}}},
                    'url': 'https://private.example/?token=url-secret', 'env': {'TOKEN': 'env-secret'}} for name in names]}
            return original(method, params, timeout)
        service = self.b.runtime.multi_server().moves()
        original_caps = service._capabilities
        source_service = self.a.runtime.multi_server().moves()
        source_caps = source_service._capabilities
        with patch.object(source, 'call', side_effect=lambda m, p, timeout=60: status(source_call, ['anarlog', 'computer-use'], m, p, timeout)), \
             patch.object(target, 'call', side_effect=lambda m, p, timeout=60: status(target_call, ['linear'], m, p, timeout)), \
             patch.object(source_service, '_capabilities', side_effect=lambda actor: {**source_caps(actor), 'platform': 'Darwin'}), \
             patch.object(service, '_capabilities', side_effect=lambda actor: {**original_caps(actor), 'platform': 'Linux'}):
            with self.assertRaises(ValueError) as refused:
                self.a.runtime.multi_server().moves().start(self.lead,
                    {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'detail'}, 'detail')
        message = str(refused.exception)
        for field in ('0.160.1', '0.161.0', 'Darwin', 'Linux', 'anarlog', 'computer-use', 'linear', 'inspect'):
            self.assertIn(field, message)
        for secret in ('private-description', 'schema-secret', 'private.example', 'url-secret', 'env-secret'):
            self.assertNotIn(secret, message)
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_refusal_names_all_claude_version_and_platform_fields(self):
        self.claude_source()
        self.b.runtime.server.cli_version = '2.1.177'
        self.b.runtime.server.sdk_version = '0.3.284'
        service = self.b.runtime.multi_server().moves()
        original = service._capabilities
        source_service = self.a.runtime.multi_server().moves()
        source_caps = source_service._capabilities
        with patch.object(source_service, '_capabilities', side_effect=lambda actor: {**source_caps(actor), 'platform': 'Darwin'}), \
             patch.object(service, '_capabilities', side_effect=lambda actor: {**original(actor), 'platform': 'Linux'}):
            with self.assertRaises(ValueError) as refused:
                self.a.runtime.multi_server().moves().start(self.lead,
                    {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'claude-detail'}, 'claude-detail')
        for field in ('2.1.291', '2.1.177', '0.3.285', '0.3.284', 'Darwin', 'Linux'):
            self.assertIn(field, str(refused.exception))
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_refusal_preserves_claude_native_tool_details_without_raw_errors(self):
        self.claude_source()
        target = self.b.runtime.server
        original = target.call
        def refuse(method, params, timeout=60):
            if method == 'claude/movePreflight':
                raise NativeRpcError({'code': -32000, 'message': 'raw-error-secret', 'data': {
                    'moveRefusal': {'kind': 'studio_tools', 'sourceNames': ['mcp__studio__old', 'mcp__studio__same'],
                        'targetNames': ['mcp__studio__new', 'mcp__studio__same'], 'changedNames': ['mcp__studio__same']}}})
            return original(method, params, timeout)
        with patch.object(target, 'call', side_effect=refuse):
            with self.assertRaises(ValueError) as refused:
                self.a.runtime.multi_server().moves().start(self.lead,
                    {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'native-tool-detail'}, 'native-tool-detail')
        message = str(refused.exception)
        for name in ('mcp__studio__old', 'mcp__studio__new', 'changed definitions [mcp__studio__same]'):
            self.assertIn(name, message)
        self.assertNotIn('raw-error-secret', message)
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_refusal_names_changed_tools_when_versions_and_servers_match(self):
        source, target = self.a.runtime.server, self.b.runtime.server
        source_call, target_call = source.call, target.call
        def status(original, side, method, params, timeout=60):
            if method == 'mcpServerStatus/list':
                return {'data': [{'name': 'same-server', 'tools': {
                    'read': {'name': 'read', 'inputSchema': {'private': side + '-secret'}},
                    side: {'name': side, 'inputSchema': {'private': side + '-secret'}}}}]}
            return original(method, params, timeout)
        with patch.object(source, 'call', side_effect=lambda m, p, timeout=60: status(source_call, 'source-only', m, p, timeout)), \
             patch.object(target, 'call', side_effect=lambda m, p, timeout=60: status(target_call, 'target-only', m, p, timeout)):
            with self.assertRaises(ValueError) as refused:
                self.a.runtime.multi_server().moves().start(self.lead,
                    {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'tool-detail'}, 'tool-detail')
        message = str(refused.exception)
        for name in ('same-server', 'source-only', 'target-only', 'MCP tool definitions differ: same-server/read'):
            self.assertIn(name, message)
        self.assertNotIn('source-only-secret', message)
        self.assertNotIn('target-only-secret', message)
        self.assertNotIn('CLI version differs', message)
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_external_catalog_refusal_names_missing_target_server_through_signed_caller(self):
        self.claude_source()
        target = self.b.runtime.server
        original = target.call
        def refuse(method, params, timeout=60):
            if method == 'claude/movePreflight':
                raise NativeRpcError({'code': -32000, 'message': 'raw-credential-secret', 'data': {
                    'moveRefusal': {'kind': 'external_tools', 'sourceNames': ['anarlog', 'claude.ai Claude Docs'],
                        'targetNames': ['anarlog'], 'changedNames': ['claude.ai Claude Docs']}}})
            return original(method, params, timeout)
        with patch.object(target, 'call', side_effect=refuse):
            with self.assertRaisesRegex(ValueError, 'external MCP catalog is unavailable or differs') as refused:
                self.a.runtime.multi_server().moves().start(self.lead,
                    {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'external-catalog-detail'}, 'external-catalog-detail')
        self.assertIn('target names [anarlog]', str(refused.exception))
        self.assertIn('changed definitions [claude.ai Claude Docs]', str(refused.exception))
        self.assertNotIn('raw-credential-secret', str(refused.exception))
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_resource_data_and_auth_status_do_not_change_teleport_capabilities(self):
        source, target = self.a.runtime.server, self.b.runtime.server
        source_call, target_call = source.call, target.call
        def status(original, side, method, params, timeout=60):
            if method == 'mcpServerStatus/list':
                return {'data': [{'name': 'anarlog', 'serverInfo': {'name': 'anarlog', 'version': '1'},
                    'tools': {'read': {'name': 'read', 'description': 'Read', 'inputSchema': {'type': 'object'}}},
                    'resources': [{'uri': 'private-meeting'}] if side == 'source' else [],
                    'resourceTemplates': [{'uriTemplate': 'private/{id}'}] if side == 'source' else [],
                    'authStatus': side, 'runtimeStatus': side}]}
            return original(method, params, timeout)
        with patch.object(source, 'call', side_effect=lambda m, p, timeout=60: status(source_call, 'source', m, p, timeout)), \
             patch.object(target, 'call', side_effect=lambda m, p, timeout=60: status(target_call, 'target', m, p, timeout)):
            result = self.a.runtime.multi_server().moves().start(self.lead,
                {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'resource-only'}, 'resource-only')
        self.assertEqual(result['status'], 'accepted')
        self.assertIsNone(result['warning'])
        self.assertNotIn('private-meeting', json.dumps(result))
        self.assertEqual(result['mcpProofLevels'], [{'name': 'anarlog', 'proofLevel': 'server_info_fallback'}])

    def instruction_preflight(self, different=False, fallback=False):
        script = self.a.folder / 'instruction-server.py'
        log = self.a.folder / 'instruction-requests'
        script.write_text("import json,sys,os,pathlib\nassert pathlib.Path.cwd()==pathlib.Path(os.environ['EXPECTED_CWD']).resolve()\nfor line in sys.stdin:\n r=json.loads(line)\n"
            " with open(" + repr(str(log)) + ", 'a') as f:f.write(r['method']+'\\n')\n"
            " if 'id' not in r:continue\n"
            " print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':{'protocolVersion':r['params']['protocolVersion'],"
            "'capabilities':{},'serverInfo':{'name':'fixture','version':'1'},'instructions':os.environ['PROOF_TEXT']}}),flush=True)\n")
        source, target = self.a.runtime.server, self.b.runtime.server
        source_call, target_call = source.call, target.call
        def reply(original, is_target, method, params, timeout=60):
            if method == 'config/read':
                if is_target and fallback:
                    return {'config': {'mcp_servers': {}}}
                return {'config': {'mcp_servers': {'fixture': {'command': sys.executable, 'args': [str(script)], 'cwd': str(self.a.folder),
                    'env': {'EXPECTED_CWD': str(self.a.folder), 'PROOF_TEXT': 'target-private-instructions' if is_target and different else 'source-private-instructions'}}}}}
            if method == 'mcpServerStatus/list':
                return {'data': [{'name': 'fixture', 'serverInfo': {'name': 'fixture', 'version': '1'},
                    'tools': {'read': {'name': 'read', 'description': 'Read', 'inputSchema': {'type': 'object'}}}}]}
            return original(method, params, timeout)
        with patch.object(source, 'call', side_effect=lambda m, p, timeout=60: reply(source_call, False, m, p, timeout)), \
             patch.object(target, 'call', side_effect=lambda m, p, timeout=60: reply(target_call, True, m, p, timeout)):
            result = self.a.runtime.multi_server().moves().start(self.lead,
                {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'instruction-proof'}, 'instruction-proof')
        self.assertEqual(set(log.read_text().splitlines()), {'initialize', 'notifications/initialized'})
        return result

    def test_verified_mcp_initialization_instruction_difference_refuses(self):
        with self.assertRaisesRegex(ValueError, 'MCP server fixture initialization instructions differ') as refused:
            self.instruction_preflight(different=True)
        self.assertNotIn('private-instructions', str(refused.exception))
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_matching_mcp_initialization_instructions_record_strong_proof(self):
        result = self.instruction_preflight()
        self.assertEqual(result['mcpProofLevels'], [{'name': 'fixture', 'proofLevel': 'initialize_instructions'}])
        self.assertNotIn('private-instructions', json.dumps(result))

    def test_unavailable_target_mcp_instructions_use_visible_server_info_fallback(self):
        result = self.instruction_preflight(fallback=True)
        self.assertEqual(result['mcpProofLevels'], [{'name': 'fixture', 'proofLevel': 'server_info_fallback'}])

    def test_refusal_keeps_unstructured_claude_native_errors_unknown(self):
        self.claude_source()
        target = self.b.runtime.server
        original = target.call
        def refuse(method, params, timeout=60):
            if method == 'claude/movePreflight':
                raise NativeRpcError({'code': -32000, 'message': 'raw-error-secret',
                    'data': {'moveRefusal': {'kind': 'unknown', 'sourceNames': ['private-name']}}})
            return original(method, params, timeout)
        with patch.object(target, 'call', side_effect=refuse):
            with self.assertRaisesRegex(RuntimeError, 'receipt is unknown') as refused:
                self.a.runtime.multi_server().moves().start(self.lead,
                    {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'unknown-native-error'}, 'unknown-native-error')
        self.assertNotIn('raw-error-secret', str(refused.exception))
        self.assertNotIn('private-name', str(refused.exception))
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_claude_same_email_with_a_different_organization_refuses(self):
        self.claude_source()
        self.b.runtime.server.organization = 'another-organization'
        with self.assertRaisesRegex(ValueError, 'same provider account and organization'):
            self.a.runtime.multi_server().moves().start(self.lead,
                {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'organization-refuse'}, 'organization-refuse')
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_review_claude_cross_os_refuses_the_builtin_catalog(self):
        self.claude_source()
        target = self.b.runtime.multi_server().moves()
        original = target._capabilities
        with patch.object(target, '_capabilities', side_effect=lambda actor: {**original(actor), 'platform': 'Another OS'}):
            with self.assertRaisesRegex(ValueError, 'Platform differs.*builtin tool catalog'):
                self.a.runtime.multi_server().moves().start(self.lead,
                    {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'os-refuse'}, 'os-refuse')
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_review_claude_requires_saved_snapshot_and_matching_target_tools(self):
        self.claude_source()
        self.a.runtime.server.move_snapshot = False
        service = self.a.runtime.multi_server().moves()
        args = {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'snapshot-refuse'}
        with self.assertRaisesRegex(ValueError, 'saved prompt snapshot'):
            service.start(self.lead, args, 'snapshot-refuse')
        self.a.runtime.server.move_snapshot = True
        self.b.runtime.server.move_catalog = 'different-schema-or-order'
        with self.assertRaisesRegex(ValueError, 'tool definitions differ'):
            service.start(self.lead, {**args, 'request_id': 'tool-refuse'}, 'tool-refuse')
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))

    def test_provider_switch_refuses_and_another_codex_account_warns(self):
        row = self.b.runtime.accounts.get('default')
        row.update(provider='claude', email='different@example.com', accountId='different-account')
        with patch.object(self.b.runtime.accounts, 'list', return_value=[row]):
            with self.assertRaisesRegex(ValueError, 'cannot change providers'):
                self.a.runtime.multi_server().moves().start(self.lead,
                    {'server': self.b.server_id, 'cwd': str(self.b.folder), 'account_key': 'default', 'request_id': 'wrong-provider'}, 'wrong-provider')
        self.assertFalse(self.a.runtime.agent(self.lead['id']).get('executionMove'))
        row['provider'] = 'codex'
        with patch.object(self.b.runtime.accounts, 'list', return_value=[row]), patch.object(self.b.runtime.accounts, 'get', return_value=row):
            reply = self.tool(self.a, self.lead, 'orchestration_move',
                {'server': self.b.server_id, 'cwd': str(self.b.folder), 'request_id': 'account-warning', 'accept_cache_loss': True}, 'account-warning')
        self.assertEqual(reply['status'], 'accepted')
        self.assertIn('prompt cache will be lost', reply['warning'])
        self.assertTrue(reply['cacheLossApproved'])

    def test_local_move_preserves_history_and_the_old_workspace(self):
        service = self.a.runtime.multi_server().moves()
        destination = self.a.folder / 'prepared-local'
        destination.mkdir()
        reply = self.tool(self.a, self.lead, 'orchestration_move',
            {'server': 'local', 'cwd': str(destination), 'request_id': 'local-move'}, 'local-move')
        service.tick()
        f.fixture.f.eventually(lambda: not service.running)
        moved = self.a.runtime.agent(self.lead['id'])
        self.assertFalse(moved.get('executionMove'), moved.get('error'))
        self.assertEqual(moved['cwd'], str(destination))
        self.assertEqual(moved['executionArchives'][-1]['workspace']['cwd'], self.lead['cwd'])
        self.assertFalse(moved.get('movedTo'))
        self.assertEqual(len(self.events(self.a, moved['id'], 'followup')), 1)

    def test_return_to_the_original_home_keeps_the_identity_and_task_board(self):
        moved = self.finish_move(self.lead)
        self.b.runtime.server.complete(moved['threadId'], moved['turnId'], 'Ready to return')
        self.drain()
        moved = self.b.runtime.agent(moved['id'])
        reply = self.tool(self.b, moved, 'orchestration_move',
            {'server': self.a.server_id, 'cwd': str(self.a.folder), 'request_id': 'return-home'}, 'return-home')
        self.assertEqual(reply['status'], 'accepted')
        service = self.b.runtime.multi_server().moves()
        service.tick()
        f.fixture.f.eventually(lambda: not service.running)
        restored = self.a.runtime.agent(moved['id'])
        self.assertFalse(restored.get('remoteOrigin'), restored.get('error'))
        self.assertFalse(restored.get('movedTo'))
        self.assertEqual(restored['rootId'], self.lead['id'])
        self.assertEqual(len(restored['executionArchives']), 2)
        self.assertEqual(self.b.runtime.agent(moved['id'])['movedTo']['server'], self.a.server_id)
        self.a.runtime.dispatch()
        f.fixture.f.eventually(lambda: bool(self.a.runtime.agent(moved['id']).get('turnId')))

    def test_worker_move_reports_to_original_parent(self):
        worker = self.a.runtime.create({'name': 'Worker', 'prompt': 'Check', 'role': 'reviewer'}, parent=self.lead['id'], defer=True)
        with self.a.runtime.db() as db:
            worker['autoWake'] = True
            self.a.runtime.put(db, 'agents', worker)
        worker = self.a.runtime.prepare(worker)
        moved = self.finish_move(worker)
        self.b.runtime.server.complete(moved['threadId'], moved['turnId'], 'Moved result')
        self.drive(lambda: bool(self.events(self.a, self.lead['id'], 'child_result')))
        result = json.loads(self.events(self.a, self.lead['id'], 'child_result')[0]['text'])
        self.assertEqual(result['agent_id'], worker['id'])
        self.assertEqual(result['result'], 'Moved result')


if __name__ == '__main__':
    unittest.main(verbosity=2)

#!/usr/bin/env python3
"""Model projections preserve durable data, delivery identities, and permissions."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import time
import unittest
import uuid
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('efficiency_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_efficiency import EfficiencyMixin, packed, remember_context_manifest
from codex_payloads import resolve_result, state_root


class EfficiencyContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update
    start = f.WorkspaceContract.start
    work = f.WorkspaceContract.work
    events = f.WorkspaceContract.events
    tool = f.WorkspaceContract.tool

    def value(self, result):
        self.assertTrue(result['success'], result)
        return json.loads(result['contentItems'][0]['text'])

    def test_task_pages_detail_history_and_unchanged_ui(self):
        lead = self.lead()
        for i in range(77):
            task = self.work(lead, title=f'Task {i}', description='Long description ' * 200)
            with self.runtime.db() as db:
                task['results'] = [{'id': f'r{j}', 'created': j, 'text': 'Evidence ' * 200} for j in range(9)]
                self.runtime.put(db, 'work', task)
        full = self.runtime.work_action(lead['id'], {'action': 'list'}, actor=lead['id'])
        brief = self.runtime.model_work(lead['id'], {'action': 'list'})
        self.assertEqual(full['items'], full['tasks'])
        self.assertEqual(brief['total'], 77)
        self.assertEqual(len(brief['items']), 20)
        self.assertLess(len(packed(brief)), len(packed(full)) / 50)
        self.assertNotIn('tasks', brief)
        ids = [r['id'] for r in brief['items']]
        while brief['nextCursor']:
            brief = self.runtime.model_work(lead['id'], {'cursor': brief['nextCursor']})
            ids.extend(r['id'] for r in brief['items'])
        self.assertEqual(len(set(ids)), 77)
        detail = self.runtime.model_work(lead['id'], {'action': 'get', 'task_id': task['id']})
        self.assertEqual(detail['latestResult']['id'], 'r8')
        self.assertEqual(detail['description'], task['description'])
        history = self.runtime.model_work(lead['id'], {'action': 'history', 'task_id': task['id'], 'limit': 2})
        self.assertEqual([r['id'] for r in history['items']], ['r0', 'r1'])
        self.assertIsNotNone(history['nextCursor'])
        self.assertEqual(self.runtime.model_work(lead['id'], {'state': 'accepted'})['items'], [])
        other = self.lead('Other')
        with self.assertRaisesRegex(ValueError, 'Unknown task'):
            self.runtime.model_work(other['id'], {'action': 'get', 'task_id': task['id']})

    def test_stale_cursor_and_invalid_limits_are_explicit(self):
        lead = self.lead()
        self.work(lead); self.work(lead)
        page = self.runtime.model_work(lead['id'], {'limit': 1})
        self.work(lead)
        with self.assertRaisesRegex(ValueError, 'List changed'):
            self.runtime.model_work(lead['id'], {'cursor': page['nextCursor']})
        for args in [{'limit': True}, {'limit': 51}, {'limit': 0}, {'cursor': 'bad!'}]:
            with self.assertRaises(ValueError):
                self.runtime.model_work(lead['id'], args)

    def test_peer_cursor_survives_volatile_changes_and_returns_current_status(self):
        lead = self.lead()
        agents = [lead, *(self.worker(lead, f'Worker {i}') for i in range(20))]
        agents.sort(key=lambda agent: agent['id'])
        first = self.runtime.model_directory(lead['id'], 'orchestration_peers', {'limit': 20})
        self.assertEqual([agent['id'] for agent in first['items']], [agent['id'] for agent in agents[:20]])
        target = agents[-1]
        expected = {key: target.get(key) for key in ('id', 'name', 'role', 'rootId', 'parentId')}
        for agent in agents:
            # Runtime.notification timestamps lastEvent as an ISO UTC string.
            self.agent_update(agent, status='running', inFlight=True, error='Current state',
                              lastEvent='2026-10-06T00:00:00Z')
        for status in ('running', 'approval', 'waiting'):
            with self.subTest(status=status):
                self.agent_update(target, status=status)
                page = self.runtime.model_directory(lead['id'], 'orchestration_peers',
                    {'limit': 20, 'cursor': first['nextCursor']})
                self.assertEqual(page['items'], [{**expected, 'status': status}])
                self.assertEqual(page['revision'], first['revision'])
                self.assertEqual(page['nextCursor'], None)
                self.assertEqual(page['total'], 21)

    def test_peer_cursor_rejects_identity_and_membership_changes(self):
        lead = self.lead()
        worker = self.worker(lead)
        for field, value in (('name', 'New display name'), ('role', 'implementer'), ('parentId', None)):
            with self.subTest(field=field):
                page = self.runtime.model_directory(lead['id'], 'orchestration_peers', {'limit': 1})
                self.agent_update(worker, **{field: value})
                with self.assertRaisesRegex(ValueError, 'List changed'):
                    self.runtime.model_directory(lead['id'], 'orchestration_peers', {'cursor': page['nextCursor']})
                self.agent_update(worker, **{field: worker.get(field)})
        page = self.runtime.model_directory(lead['id'], 'orchestration_peers', {'limit': 1})
        added = self.worker(lead, 'Added')
        with self.assertRaisesRegex(ValueError, 'List changed'):
            self.runtime.model_directory(lead['id'], 'orchestration_peers', {'cursor': page['nextCursor']})
        page = self.runtime.model_directory(lead['id'], 'orchestration_peers', {'limit': 1})
        self.agent_update(added, deletedAt=time.time())
        with self.assertRaisesRegex(ValueError, 'List changed'):
            self.runtime.model_directory(lead['id'], 'orchestration_peers', {'cursor': page['nextCursor']})

    def test_peer_cursor_preserves_actor_scope_and_room_permissions(self):
        from codex_peer_teams import manage
        lead, peer = self.lead(), self.lead('Peer')
        worker = self.worker(lead)
        team_id = str(uuid.uuid4())
        group = {'action': 'save', 'path': str(self.project), 'team_id': team_id,
                 'name': 'Peers', 'members': [lead['id'], peer['id']]}
        manage(self.runtime, {**group, 'expected_revision': 0, 'request_id': str(uuid.uuid4())})
        receipt = self.runtime.chat_message(lead['id'], peer['id'], 'Private peer question', 'peer-cursor-room')
        page = self.runtime.model_directory(lead['id'], 'orchestration_peers', {'limit': 1})
        for actor in (worker, peer):
            with self.subTest(actor=actor['id']), self.assertRaisesRegex(ValueError, 'List changed'):
                self.runtime.model_directory(actor['id'], 'orchestration_peers', {'cursor': page['nextCursor']})
        with self.assertRaisesRegex(ValueError, 'limited to your team'):
            self.runtime.model_directory(lead['id'], 'orchestration_peers', {'scope': 'all', 'cursor': page['nextCursor']})
        manage(self.runtime, {**group, 'name': 'Renamed peers', 'expected_revision': 1,
                              'request_id': str(uuid.uuid4())})
        with self.assertRaisesRegex(ValueError, 'List changed'):
            self.runtime.model_directory(lead['id'], 'orchestration_peers', {'cursor': page['nextCursor']})
        page = self.runtime.model_directory(lead['id'], 'orchestration_peers', {'limit': 1})
        manage(self.runtime, {'action': 'delete', 'path': str(self.project), 'team_id': team_id,
                              'expected_revision': 2, 'request_id': str(uuid.uuid4())})
        with self.assertRaisesRegex(ValueError, 'List changed'):
            self.runtime.model_directory(lead['id'], 'orchestration_peers', {'cursor': page['nextCursor']})
        with self.assertRaises(ValueError):
            self.runtime.chat_read(receipt['room'], peer['id'])

    def test_default_cursor_still_tracks_status_changes(self):
        rows = [{'id': 'first', 'status': 'pending'}, {'id': 'second', 'status': 'pending'}]
        page = self.runtime.model_page(rows, {'limit': 1}, ['other-directory'])
        rows[-1]['status'] = 'completed'
        with self.assertRaisesRegex(ValueError, 'List changed'):
            self.runtime.model_page(rows, {'cursor': page['nextCursor']}, ['other-directory'])

    def test_mutation_receipt_is_compact_but_full_evidence_is_durable(self):
        lead = self.lead()
        args = {'action': 'create', 'title': 'Task', 'description': 'Full task content ' * 1000}
        first = self.runtime.model_work(lead['id'], args, 'create-once', lead['epoch'])
        self.assertNotIn('description', first)
        self.assertEqual(self.runtime.model_work(lead['id'], args, 'create-once', lead['epoch']), first)
        with self.runtime.db() as db:
            saved = json.loads(db.execute('SELECT result FROM runtime_operation_receipts WHERE id=?', ('create-once',)).fetchone()[0])
        self.assertEqual(saved['description'], args['description'].strip())
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.runtime.model_work(lead['id'], {**args, 'title': 'Different'}, 'create-once', lead['epoch'])
        self.assertEqual(self.runtime.model_work(lead['id'], {})['total'], 1)

    def test_status_delta_and_peer_privacy(self):
        lead = self.lead(); worker = self.worker(lead); other = self.lead('Other')
        self.runtime.chat_message(worker['id'], lead['id'], 'Private evidence', 'chat1')
        with self.runtime.db() as db:
            self.runtime.put(db, 'monitors', {'id': 'm', 'agent': worker['id'], 'status': 'running', 'tail': 'SECRET ' * 20000, 'created': time.time()})
        status = self.runtime.model_directory(lead['id'], 'orchestration_status', {})
        self.assertNotIn('SECRET', packed(status))
        self.assertNotIn('Private evidence', packed(status))
        self.assertEqual(len(status['changes']), 3)
        with self.runtime.db() as db:
            self.runtime.put(db, 'monitors', {'id': 'done', 'agent': worker['id'], 'status': 'completed', 'created': time.time()})
        status = self.runtime.model_directory(lead['id'], 'orchestration_status', {})
        self.assertNotIn('monitor:done', [item['kind'] + ':' + item['id'] for item in status['changes']])
        self.assertEqual(status['finishedCounts']['monitors'], 1)
        page = self.runtime.model_directory(lead['id'], 'orchestration_status', {'include_finished': True, 'limit': 1})['finished']
        self.assertEqual(page['total'], 1)
        self.assertEqual(page['items'][0]['id'], 'done')
        same = self.runtime.model_directory(lead['id'], 'orchestration_status', {'since_revision': status['revision']})
        self.assertTrue(same['unchanged']); self.assertEqual(same['changes'], [])
        self.agent_update(worker, status='failed', error='Specific cause')
        changed = self.runtime.model_directory(lead['id'], 'orchestration_status', {'since_revision': status['revision']})
        self.assertEqual(changed['changes'], [])
        self.assertIn('agent:' + worker['id'], changed['removed'])
        finished = self.runtime.model_directory(lead['id'], 'orchestration_status', {'include_finished': True, 'limit': 1})['finished']
        self.assertTrue(finished['nextCursor'])
        next_page = self.runtime.model_directory(lead['id'], 'orchestration_status',
            {'include_finished': True, 'limit': 1, 'cursor': finished['nextCursor']})['finished']
        self.assertEqual(next_page['items'][0]['id'], 'done')
        self.agent_update(worker, deletedAt=time.time())
        removed = self.runtime.model_directory(lead['id'], 'orchestration_status', {'since_revision': changed['revision']})
        self.assertNotIn('agent:' + worker['id'], [item['kind'] + ':' + item['id'] for item in removed['changes']])
        peers = self.runtime.model_directory(other['id'], 'orchestration_peers', {})
        self.assertEqual([a['id'] for a in peers['items']], [other['id']])
        self.assertNotIn('private:', packed(peers))
        with self.assertRaisesRegex(ValueError, 'limited to your team'):
            self.runtime.model_directory(other['id'], 'orchestration_peers', {'scope': 'all'})
        with self.assertRaisesRegex(ValueError, 'Unknown monitor'):
            self.runtime.model_context(other['id'], {'topic': 'monitor', 'id': 'm'})

    def test_status_reads_only_team_agents_and_monitors(self):
        lead = self.lead(); self.worker(lead)
        with patch.object(self.runtime, 'records', side_effect=AssertionError('global records decoded')):
            status = self.runtime.model_directory(lead['id'], 'orchestration_status', {})
        self.assertEqual(len(status['changes']), 2)
        with self.runtime.db() as db:
            plan = ' '.join(str(tuple(row)) for row in db.execute("EXPLAIN QUERY PLAN SELECT record FROM runtime_monitors "
                "WHERE json_extract(record,'$.agent')=? AND json_extract(record,'$.status') IN (?,?,?)",
                (lead['id'], 'running', 'starting', 'approval')))
            self.assertIn('runtime_monitor_agent_status', plan)
            capacity_plan = ' '.join(str(tuple(row)) for row in db.execute(
                "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM runtime_agents INDEXED BY runtime_agent_global_active WHERE "
                "(json_extract(record,'$.deletedAt') IS NULL OR json_extract(record,'$.deletedAt') IN (0,'')) "
                "AND json_extract(record,'$.status') IN ('running','starting','approval')"))
            self.assertIn('runtime_agent_global_active', capacity_plan)
            flight_plan = ' '.join(str(tuple(row)) for row in db.execute(
                "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM runtime_agents INDEXED BY runtime_agent_inflight WHERE "
                "(json_extract(record,'$.deletedAt') IS NULL OR json_extract(record,'$.deletedAt') IN (0,'')) "
                "AND json_extract(record,'$.inFlight')=1 AND json_extract(record,'$.status') NOT IN (?,?,?)",
                ('running','starting','approval')))
            self.assertIn('runtime_agent_inflight', flight_plan)
            reservation_plan = ' '.join(str(tuple(row)) for row in db.execute(
                "EXPLAIN QUERY PLAN SELECT id,json_extract(record,'$.cwd') FROM runtime_agents "
                "INDEXED BY runtime_agent_reservation_cwd WHERE json_type(record,'$.workspaceOperation')='text' "
                "AND json_extract(record,'$.workspaceOperation')!=''"))
            self.assertIn('runtime_agent_reservation_cwd', reservation_plan)

    def test_last_delivered_context_manifest_uses_persisted_agent_epoch_row(self):
        lead = self.lead()
        event = {'id':'manifest-event','agent':lead['id'],'kind':'user','text':'hello',
                 'status':'delivered','created':1,'epoch':lead['epoch'],'turn_id':'turn', 'error':None}
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)', tuple(event.values()))
            db.execute('INSERT INTO runtime_event_meta VALUES (?,?)', ('manifest-event', packed({
                'contextManifest':{'epoch':[lead['threadId'],lead.get('compactions',0)],
                                   'versions':{'roleSkill':'v1','worktreeReminder':'workers-v1'},'sequence':8}})))
            self.assertTrue(remember_context_manifest(db, lead['id'], 'manifest-event'))
            self.assertTrue(db.execute('SELECT 1 FROM runtime_context_reminders WHERE agent=? AND version=?',
                                       (lead['id'], 'workers-v1')).fetchone())
            query = []
            db.set_trace_callback(query.append)
            known = self.runtime.model_known_context(db, lead)[2]
            db.set_trace_callback(None)
        self.assertEqual(known['roleSkill'], 'v1')
        self.assertFalse(any('JOIN runtime_events' in statement for statement in query))
        with self.runtime.db() as db:
            db.execute('DELETE FROM runtime_context_manifests WHERE agent=?', (lead['id'],))
            query = []
            db.set_trace_callback(query.append)
            cold_known = self.runtime.model_known_context(db, lead)[2]
            db.set_trace_callback(None)
        self.assertEqual(cold_known, {})
        self.assertFalse(any('runtime_events' in statement for statement in query))
        with self.runtime.db() as db:
            db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?',
                       (packed({'contextManifest':{'epoch':[lead['threadId'],'invalid']}}), 'manifest-event'))
            self.assertFalse(remember_context_manifest(db, lead['id'], 'manifest-event'))

    def test_large_dynamic_result_is_readable_without_reexecution(self):
        lead = self.runtime.prepare(self.lead())
        huge = {'body': 'Доказательство 🚀 ' * 3000, 'id': 'exact-result-id'}
        message = {'id': 'large', 'params': {'threadId': lead['threadId'], 'callId': 'large', 'tool': 'orchestration_search', 'arguments': {'query': 'evidence'}}}
        with patch.object(self.runtime, 'search_work', return_value=huge) as search:
            self.runtime.dynamic(message)
            first = self.runtime.server.responses[-1]['result']
            self.runtime.dynamic(message)
            self.assertEqual(self.runtime.server.responses[-1]['result'], first)
            self.assertEqual(search.call_count, 1)
        preview = self.value(first)
        self.assertTrue(preview['truncated'])
        self.assertEqual(preview['summary']['id'], 'exact-result-id')
        self.assertLess(len(packed(first).encode()), 6000)
        key = preview['outputRef']
        with self.runtime.db() as db:
            saved = json.loads(db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (key,)).fetchone()[0])
        raw_result = resolve_result(state_root(self.runtime), saved)
        raw = '\n'.join(c['text'] for c in raw_result['contentItems'] if c['type'] == 'inputText')
        chunks, offset = [], 0
        while True:
            page = self.runtime.model_read(lead['id'], {'output_ref': key, 'offset': offset})
            if not chunks:
                self.assertEqual(len(page['text']), 30000)
            chunks.append(page['text'])
            if page['nextOffset'] is None:
                break
            offset = page['nextOffset']
        self.assertEqual(''.join(chunks), raw)
        found = self.runtime.model_read(lead['id'], {'output_ref': key, 'contains': 'exact-result-id'})
        self.assertIn('exact-result-id', found['text'])
        other = self.lead('Other')
        with self.assertRaisesRegex(ValueError, 'not owned'):
            self.runtime.model_read(other['id'], {'output_ref': key})
        with self.assertRaises(ValueError):
            self.runtime.model_read(lead['id'], {'output_ref': key, 'offset': -1})

    def test_read_pages_keep_thirty_thousand_characters_inline_once(self):
        lead = self.lead()
        page = {'outputRef': 'event:large', 'offset': 0, 'totalChars': 30000,
                'nextOffset': None, 'text': 'x' * 30000}
        result = {'success': True, 'contentItems': [{'type': 'inputText', 'text': packed(page)}]}
        projected = self.runtime.model_tool_result(lead['id'], 'read-page', result)
        self.assertEqual(projected, result)
        self.assertEqual(len(projected['contentItems']), 1)
        self.assertIn('x' * 30000, projected['contentItems'][0]['text'])

    def test_projection_retains_images_clocks_failure_and_spawn_ids(self):
        lead = self.lead()
        image = {'type': 'inputImage', 'imageUrl': 'data:image/png;base64,example'}
        clock = {'type': 'inputText', 'text': '[Time awareness] Fixed time'}
        result = {'success': False, 'contentItems': [{'type': 'inputText', 'text': packed({'agents': [{'id': 'child', 'large': 'x' * 20000}]})}, image, clock]}
        projected = self.runtime.model_tool_result(lead['id'], 'missing', result)
        self.assertFalse(projected['success'])
        self.assertIn(image, projected['contentItems']); self.assertIn(clock, projected['contentItems'])
        self.assertEqual(json.loads(projected['contentItems'][0]['text'])['outcome'], 'unknown')
        self.assertEqual(json.loads(projected['contentItems'][0]['text'])['summary']['agents'], [{'id': 'child'}])

    def test_progress_batches_and_keeps_all_durable_messages(self):
        lead = self.start(self.lead()); worker = self.worker(lead)
        self.runtime.server.complete(lead['threadId'], lead['turnId'])
        f.eventually(lambda: not self.runtime.agent(lead['id'])['inFlight'])
        for n in range(3):
            self.runtime.chat_message(worker['id'], lead['id'], f'Routine {n}', f'p{n}', importance='progress', progress_key='task-1', progress_version=n)
        rows = [r for r in self.events(lead, 'agent_message') if r['status'] == 'pending']
        self.assertFalse(self.runtime.progress_batch_ready(rows, min(r['created'] for r in rows) + .2))
        self.assertTrue(self.runtime.progress_batch_ready(rows, min(r['created'] for r in rows) + 1.1))
        with patch.object(self.runtime, 'progress_batch_ready', return_value=False):
            self.runtime.dispatch()
            self.assertFalse(self.runtime.agent(lead['id'])['inFlight'])
        with patch.object(self.runtime, 'progress_batch_ready', return_value=True):
            self.runtime.dispatch()
        f.eventually(lambda: any(r['status'] == 'delivered' for r in self.events(lead, 'agent_message')))
        text = [p['input'][0]['text'] for method, p in self.runtime.server.calls if method == 'turn/start'][-1]
        self.assertNotIn('Routine 0', text); self.assertIn('Routine 2', text)
        self.assertIn('earlierProgressUpdates', text)
        self.assertEqual(len([r for r in self.events(lead, 'agent_message') if r['status'] == 'delivered']), 1)
        self.assertEqual(len([r for r in self.events(lead, 'agent_message') if r['status'] == 'stored_only']), 2)
        room = 'private:' + ':'.join(sorted([lead['id'], worker['id']]))
        self.assertEqual(len(self.runtime.chat_read(room, lead['id'])['messages']), 3)
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.runtime.chat_message(worker['id'], lead['id'], 'Routine 0', 'p0', importance='blocker')

    def test_progress_does_not_hide_distinct_tasks_or_ambiguous_versions(self):
        def row(key, version, text):
            return {'id': text, 'kind': 'agent_message', 'text': packed({'sender': 's', 'room': 'r', 'importance': 'progress',
                    'progress_key': key, 'progress_version': version, 'text': text})}
        rows = [row('one', 2, 'Latest one'), row('two', 1, 'Latest two'), row('one', 1, 'Older one')]
        text = self.runtime.model_event_text(rows)
        self.assertIn('Latest one', text); self.assertIn('Latest two', text); self.assertNotIn('Older one', text)
        rows += [row('one', 2, 'Same version with different evidence'), row(None, None, 'Unversioned')]
        text = self.runtime.model_event_text(rows)
        for record in rows:
            self.assertIn(record['id'], text)

    def test_urgent_behind_32_progress_messages_bypasses_batch(self):
        lead = self.start(self.lead()); worker = self.worker(lead)
        self.runtime.server.complete(lead['threadId'], lead['turnId'])
        f.eventually(lambda: not self.runtime.agent(lead['id'])['inFlight'])
        for n in range(33):
            self.runtime.chat_message(worker['id'], lead['id'], str(n), f'p{n}', importance='progress', progress_key='task-1', progress_version=n)
        self.runtime.send(lead['id'], 'Urgent owner instruction')
        with patch.object(self.runtime, 'progress_batch_ready', return_value=True):
            self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(lead['id']).get('inFlight'))
        f.eventually(lambda: len([p for method, p in self.runtime.server.calls if method == 'turn/start']) == 2)
        text = [p['input'][0]['text'] for method, p in self.runtime.server.calls if method == 'turn/start'][-1]
        self.assertIn('Urgent owner instruction', text)
        for kind in ['user', 'child_result', 'monitor_exit']:
            self.assertTrue(EfficiencyMixin.progress_batch_ready([{'kind': kind, 'text': '', 'created': time.time()}]))
        for importance in ['question', 'blocker', 'message', 'result']:
            row = {'kind': 'agent_message', 'text': packed({'importance': importance}), 'created': time.time()}
            self.assertTrue(EfficiencyMixin.progress_batch_ready([row]))

    def report_plan(self, lead, text, status='pending', explanation=''):
        with self.runtime.lock, self.runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_plans WHERE id=?', (lead['id'],)).fetchone()
            plan = json.loads(row[0]) if row else {'id': lead['id'], 'rootId': lead['rootId'], 'text': '', 'version': 0}
            steps = [{'step': text, 'status': status}] if text else []
            plan.update(native={'plan': steps, 'explanation': explanation}, steps=steps, updated=time.time())
            self.runtime.put(db, 'plans', plan)

    def test_context_versions_need_confirmed_delivery_and_reset_after_compaction(self):
        lead = self.lead(); worker = self.worker(lead)
        self.report_plan(lead, 'Exact plan content')
        self.runtime.complaint(worker['id'], {'action': 'submit', 'text': 'Exact complaint content'}, 'complaint')
        def context(event, delivered=False, **changes):
            with self.runtime.lock, self.runtime.db() as db:
                actor = self.runtime.agent(lead['id'], db); actor.update(changes)
                self.runtime.enqueue(db, actor, 'user', 'Continue', event)
                text = self.runtime.model_turn_context(db, actor, event)
                if delivered:
                    db.execute("UPDATE runtime_events SET status='delivered' WHERE id=?", (event,))
                    remember_context_manifest(db, actor['id'], event)
                return text
        initial = context('first')
        self.assertIn('Exact plan content', initial); self.assertIn('Exact complaint content', initial)
        uncertain = context('second', delivered=True)
        self.assertIn('Exact plan content', uncertain)
        unchanged = context('third', delivered=True)
        self.assertNotIn('Exact plan content', unchanged); self.assertNotIn('Exact complaint content', unchanged)
        self.assertIn('still requiring a response', unchanged)
        compacted = context('fourth', compactions=1)
        self.assertIn('Exact plan content', compacted); self.assertIn('Exact complaint content', compacted)
        self.report_plan(lead, 'Changed plan')
        self.assertIn('Changed plan', context('fifth'))
        on_demand = self.runtime.model_context(lead['id'], {'topic': 'plan'})
        self.assertEqual(on_demand['content']['steps'][0]['step'], 'Changed plan')
        params = self.runtime.new_thread_params(self.runtime.agent(lead['id']))
        self.assertNotIn('[Studio panel guidance:', params['developerInstructions'])
        self.assertIn(str(self.runtime.progress_file(lead)), params['developerInstructions'])
        self.assertIn('clipped', self.runtime.model_context(lead['id'], {'topic': 'panel'})['content'])

    def test_context_uses_delivery_order_when_urgent_events_overtake_progress(self):
        lead = self.lead(); worker = self.worker(lead)
        def delivered(event, created, plan, expected):
            self.report_plan(lead, plan)
            with self.runtime.lock, self.runtime.db() as db:
                actor = self.runtime.agent(worker['id'], db)
                self.runtime.enqueue(db, actor, 'user', 'Continue', event)
                db.execute('UPDATE runtime_events SET created=? WHERE id=?', (created, event))
                text = self.runtime.model_turn_context(db, actor, event)
                self.assertIn(expected, text)
                db.execute("UPDATE runtime_events SET status='delivered' WHERE id=?", (event,))
                remember_context_manifest(db, worker['id'], event)
        delivered('urgent', 200, 'Plan A', 'Plan A')
        delivered('older-progress', 100, 'Plan B', 'Plan B')
        delivered('next', 300, 'Plan A', 'Plan A')
        delivered('clear', 400, '', 'Discard the previous agent plan')

    def test_plan_context_excludes_legacy_text_and_tracks_native_steps(self):
        lead = self.lead(); worker = self.worker(lead); other = self.lead('Other')
        legacy = self.runtime.plan_action(lead['id'], {'text': 'Old editable plan', 'version': 0})
        self.assertIsNone(self.runtime.model_context(worker['id'], {'topic': 'plan'})['content'])
        self.report_plan(other, 'Other team plan')
        self.report_plan(lead, 'Current step', explanation='Current reason')
        first = self.runtime.model_context(worker['id'], {'topic': 'plan'})
        self.assertNotIn('Old editable plan', packed(first))
        self.assertNotIn('Other team plan', packed(first))
        self.assertEqual(first['content']['steps'][0]['status'], 'pending')
        self.assertEqual(first['content']['explanation'], 'Current reason')
        self.assertEqual(self.runtime.plan_action(lead['id'])['text'], legacy['text'])
        self.report_plan(lead, 'Current step', status='completed', explanation='Current reason')
        changed = self.runtime.model_context(worker['id'], {'topic': 'plan'})
        self.assertNotEqual(first['version'], changed['version'])
        self.report_plan(lead, 'Current step', status='completed', explanation='New reason')
        explained = self.runtime.model_context(worker['id'], {'topic': 'plan'})
        self.assertNotEqual(changed['version'], explained['version'])
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(worker['id'], db)
            for event in ('plan-first', 'plan-second'):
                self.runtime.enqueue(db, actor, 'user', 'Continue', event)
                context = self.runtime.model_turn_context(db, actor, event)
                if event == 'plan-first':
                    self.assertIn('New reason', context)
                    self.assertNotIn('Old editable plan', context)
                    self.assertNotIn('Other team plan', context)
                    db.execute("UPDATE runtime_events SET status='delivered' WHERE id=?", (event,))
                    remember_context_manifest(db, worker['id'], event)
                else:
                    self.assertNotIn('Current step', context)

    def test_completed_tool_transcript_keeps_existing_ui_content_allowance(self):
        lead = self.runtime.prepare(self.lead())
        full = {'body': 'x' * 17000, 'end': 'VISIBLE_FULL_RESULT_MARKER'}
        with patch.object(self.runtime, 'search_work', return_value=full):
            response = self.tool(lead, 'orchestration_search', {'query': 'evidence'})
        projection = self.value(response)
        key = projection['outputRef']
        receipt = self.runtime.tool_request(key)
        item = {'id': receipt['callId'], 'type': 'dynamicToolCall', 'status': 'completed',
                'success': True, 'contentItems': response['contentItems']}
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(db, lead['id'], receipt['callId'], 'tool', packed(item), 'dynamicToolCall', toolStatus='completed')
        record = self.runtime.transcript(lead['id'])['items'][-1]
        self.assertIn('VISIBLE_FULL_RESULT_MARKER', record['text'])
        self.assertNotIn('outputRef', record['text'])

    def test_every_monitor_exit_wakes_with_errors_only_filter(self):
        lead = self.lead()
        for n, wake, code, error in [('ok', 'failure', 0, None), ('fail', 'failure', 7, None), ('unknown', 'failure', None, 'Lost response'), ('default', 'exit', 0, None)]:
            m = self.runtime.monitor(lead['id'], {'command': 'true', 'wake_on': wake}, key=n)
            self.runtime.finish_monitor(m['id'], code, error)
            events = [r for r in self.events(lead, 'monitor_exit') if json.loads(r['text'])['id'] == m['id']]
            self.assertEqual(len(events), 1)
            with self.runtime.db() as db:
                saved = json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?', (m['id'],)).fetchone()[0])
            self.assertEqual(saved['exitCode'], code)
            self.assertEqual(saved['status'], 'completed' if code == 0 else 'failed')
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.runtime.monitor(lead['id'], {'command': 'true', 'wake_on': 'exit'}, key='ok')
        with self.assertRaises(ValueError):
            self.runtime.monitor(lead['id'], {'command': 'true', 'wake_on': 'never'})


class RoleSnapshotWrites(unittest.TestCase):
    def test_parallel_writers_of_one_snapshot_do_not_fail(self):
        import tempfile
        import threading
        with tempfile.TemporaryDirectory() as root:
            store = EfficiencyMixin.__new__(EfficiencyMixin)
            store.root = Path(root)
            text, errors, start = 'Role text ' * 5000, [], threading.Barrier(16)

            def write():
                start.wait()
                try:
                    store.remember_role_text(text)
                except Exception as error:
                    errors.append(error)
            # The writers pass the exists() check together, then race on rename.
            with patch.object(Path, 'exists', return_value=False):
                threads = [threading.Thread(target=write) for _ in range(16)]
                for thread in threads:
                    thread.start()
                for thread in threads:
                    thread.join()
            self.assertEqual(errors, [])
            files = list((Path(root) / 'context-snapshots').iterdir())
            self.assertEqual([p.suffix for p in files], ['.txt'])
            self.assertEqual(files[0].read_text(encoding='utf-8'), text)


if __name__ == '__main__':
    unittest.main(verbosity=2)

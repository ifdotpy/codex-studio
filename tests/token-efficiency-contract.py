#!/usr/bin/env python3
"""Model projections preserve durable data, delivery identities, and permissions."""
import importlib.util
import json
from pathlib import Path
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('efficiency_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_efficiency import EfficiencyMixin, packed


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
        same = self.runtime.model_directory(lead['id'], 'orchestration_status', {'since_revision': status['revision']})
        self.assertTrue(same['unchanged']); self.assertEqual(same['changes'], [])
        self.agent_update(worker, status='failed', error='Specific cause')
        changed = self.runtime.model_directory(lead['id'], 'orchestration_status', {'since_revision': status['revision']})
        self.assertEqual(len(changed['changes']), 1)
        self.assertEqual(changed['changes'][0]['error'], 'Specific cause')
        self.agent_update(worker, deletedAt=time.time())
        removed = self.runtime.model_directory(lead['id'], 'orchestration_status', {'since_revision': changed['revision']})
        self.assertIn('agent:' + worker['id'], removed['removed'])
        peers = self.runtime.model_directory(other['id'], 'orchestration_peers', {})
        self.assertEqual([a['id'] for a in peers['items']], [other['id']])
        self.assertNotIn('private:', packed(peers))
        cross = self.runtime.model_directory(other['id'], 'orchestration_peers', {'scope': 'all'})
        self.assertIn(lead['id'], [a['id'] for a in cross['items']])
        with self.assertRaisesRegex(ValueError, 'Unknown monitor'):
            self.runtime.model_context(other['id'], {'topic': 'monitor', 'id': 'm'})

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
        raw = '\n'.join(c['text'] for c in saved['contentItems'] if c['type'] == 'inputText')
        chunks, offset = [], 0
        while True:
            page = self.runtime.model_read(lead['id'], {'output_ref': key, 'offset': offset})
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
        legacy = self.value(self.tool(lead, 'orchestration_send', {'agent_id': 'workspace', 'text': packed({'tool': 'orchestration_read', 'arguments': {'output_ref': key}})}))
        self.assertEqual(legacy['offset'], 0)

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
        self.assertEqual(len([r for r in self.events(lead, 'agent_message') if r['status'] == 'delivered']), 3)
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
        self.assertIn('150px', params['developerInstructions'])
        self.assertIn('150px', self.runtime.model_context(lead['id'], {'topic': 'panel'})['content'])

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

    def test_monitor_failure_only_keeps_success_in_ui_and_wakes_for_failure(self):
        lead = self.lead()
        for n, wake, code, error in [('ok', 'failure', 0, None), ('fail', 'failure', 7, None), ('unknown', 'failure', None, 'Lost response'), ('default', 'exit', 0, None)]:
            m = self.runtime.monitor(lead['id'], {'command': 'true', 'wake_on': wake}, key=n)
            self.runtime.finish_monitor(m['id'], code, error)
            events = [r for r in self.events(lead, 'monitor_exit') if json.loads(r['text'])['id'] == m['id']]
            self.assertEqual(len(events), 0 if n == 'ok' else 1)
            with self.runtime.db() as db:
                saved = json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?', (m['id'],)).fetchone()[0])
            self.assertEqual(saved['exitCode'], code)
            self.assertEqual(saved['status'], 'completed' if code == 0 else 'failed')
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.runtime.monitor(lead['id'], {'command': 'true', 'wake_on': 'exit'}, key='ok')
        with self.assertRaises(ValueError):
            self.runtime.monitor(lead['id'], {'command': 'true', 'wake_on': 'never'})


if __name__ == '__main__':
    unittest.main(verbosity=2)

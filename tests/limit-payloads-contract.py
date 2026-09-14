#!/usr/bin/env python3
"""Bound model payloads without losing durable history, identity, or hot policy."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('payload_fixture', Path(__file__).with_name('token-efficiency-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_efficiency import EfficiencyMixin, packed
from codex_progress import progress_context


class PayloadContract(unittest.TestCase):
    setUp = f.EfficiencyContract.setUp
    tearDown = f.EfficiencyContract.tearDown
    lead = f.EfficiencyContract.lead
    worker = f.EfficiencyContract.worker
    agent_update = f.EfficiencyContract.agent_update
    start = f.EfficiencyContract.start
    tool = f.EfficiencyContract.tool
    value = f.EfficiencyContract.value

    def mode(self, lead, mode):
        latest = self.runtime.agent(lead['id'])
        return self.runtime.conversation_settings(lead['id'], {
            'agent_mode': mode, 'expected_mode_revision': latest.get('agentModeRevision', 0),
            'request_id': 'mode-' + str(latest.get('agentModeRevision', 0))})

    def has_mode(self, result):
        return any(c.get('text', '').startswith('[Studio agent mode,') for c in result.get('contentItems', []))

    def test_confirmed_thread_preparation_does_not_repeat_static_guidance(self):
        lead = self.start(self.lead())
        params = next(p for method, p in self.runtime.server.calls if method == 'thread/start')
        text = next(p['input'][0]['text'] for method, p in self.runtime.server.calls if method == 'turn/start')
        for block in (self.runtime.role_guidance(lead), progress_context(self.runtime.root, lead['id'])):
            self.assertIn(block, params['developerInstructions'])
            self.assertNotIn(block, text)
        self.assertFalse(any(k.startswith('_studio') for k in params))
        with self.runtime.db() as db:
            actor = self.runtime.agent(lead['id'], db)
            actor['compactions'] = actor.get('compactions', 0) + 1
            refresh = self.runtime.model_turn_context(db, actor, 'compacted')
        self.assertIn(self.runtime.role_guidance(lead), refresh)

    def test_unconfirmed_preparation_and_changed_guidance_refresh(self):
        lead = self.lead()
        params = self.runtime.new_thread_params(lead)
        self.assertNotIn('preparedContext', self.runtime.agent(lead['id']))
        with self.runtime.db() as db:
            self.assertIn(self.runtime.role_guidance(lead), self.runtime.model_turn_context(db, lead, 'before'))
        with patch.object(self.runtime, 'role_guidance', return_value='Changed role content'):
            versions = self.runtime.preparation_context_versions(lead, params)
            self.assertNotIn('roleSkill', versions)
        lead = self.start(lead)
        with patch.object(self.runtime, 'role_guidance', return_value='Changed role content'):
            with self.runtime.db() as db:
                actor = self.runtime.agent(lead['id'], db)
                self.runtime.enqueue(db, actor, 'user', 'continue', 'changed-role')
                self.assertIn('Changed role content', self.runtime.model_turn_context(db, actor, 'changed-role'))
                db.execute("UPDATE runtime_events SET status='delivered' WHERE id='changed-role'")
                self.assertNotIn('Changed role content', self.runtime.model_turn_context(db, actor, 'after-role'))

    def test_mode_projection_requires_delivery_and_replay_keeps_same_result(self):
        lead = self.runtime.prepare(self.lead())
        self.mode(lead, 'single')
        raw = {'success': True, 'contentItems': []}
        first = self.runtime.model_tool_result(lead['id'], 'first', raw)
        self.assertTrue(self.has_mode(first))
        self.assertTrue(self.has_mode(self.runtime.model_tool_result(lead['id'], 'unconfirmed', raw)))
        self.runtime.confirm_model_tool_result(lead['id'], 'first', first)
        self.assertEqual(first, self.runtime.model_tool_result(lead['id'], 'first', raw))
        for i in range(100):
            self.assertFalse(self.has_mode(self.runtime.model_tool_result(lead['id'], 'next-' + str(i), raw)))
        with self.runtime.db() as db:
            text = self.runtime.model_turn_context(db, self.runtime.agent(lead['id'], db), 'next-turn')
        self.assertNotIn('[Studio agent mode,', text)
        self.mode(lead, 'multi')
        changed = self.runtime.model_tool_result(lead['id'], 'first', raw)
        self.assertIn('Multi agent mode', changed['contentItems'][-1]['text'])
        # A delayed old result must not acknowledge the new revision.
        self.runtime.confirm_model_tool_result(lead['id'], 'first', first)
        self.assertTrue(self.has_mode(self.runtime.model_tool_result(lead['id'], 'after-change', raw)))

    def test_mode_refresh_after_compaction_and_late_old_epoch(self):
        lead = self.runtime.prepare(self.lead())
        self.mode(lead, 'single')
        raw = {'success': True, 'contentItems': []}
        first = self.runtime.model_tool_result(lead['id'], 'first', raw)
        epoch = [lead['threadId'], lead.get('compactions', 0)]
        self.agent_update(lead, compactions=1)
        self.runtime.confirm_model_tool_result(lead['id'], 'first', first, epoch)
        refreshed = self.runtime.model_tool_result(lead['id'], 'first', raw)
        self.assertTrue(self.has_mode(refreshed))
        with self.runtime.db() as db:
            self.assertIn('Single agent mode', self.runtime.model_turn_context(db, self.runtime.agent(lead['id'], db), 'refresh'))

    def test_dynamic_reply_failure_does_not_acknowledge_mode(self):
        lead = self.runtime.prepare(self.lead())
        self.mode(lead, 'single')
        message = {'id': 'lost-reply', 'params': {'threadId': lead['threadId'], 'callId': 'lost-reply',
                    'tool': 'orchestration_peers', 'arguments': {}}}
        with patch.object(self.runtime, 'reply', side_effect=OSError('fixture write failure')):
            self.runtime.dynamic(message)
        self.assertNotIn('deliveredMode', self.runtime.agent(lead['id']))
        delivered = self.tool(lead, 'orchestration_peers', {})
        self.assertTrue(self.has_mode(delivered))
        self.assertFalse(self.has_mode(self.tool(lead, 'orchestration_peers', {})))

    def test_synthetic_batch_bound_preserves_history_and_authorized_reference(self):
        lead = self.lead()
        full = packed({'id': 'monitor-1', 'status': 'failed', 'exitCode': 7, 'tail': 'α\\"' * 20000,
                       'log': '/durable/monitor.log'})
        with self.runtime.db() as db:
            for i in range(32):
                self.runtime.enqueue(db, lead, 'monitor_exit', full, 'monitor-' + str(i))
            rows = [dict(r) for r in db.execute('SELECT * FROM runtime_events WHERE agent=?', (lead['id'],))]
            before = [r['text'] for r in rows]
        text = self.runtime.model_event_text(rows)
        self.assertLess(len(text.encode()), 27000)
        self.assertIn('event:monitor-0', text)
        pieces, offset = [], 0
        while True:
            page = self.runtime.model_read(lead['id'], {'output_ref': 'event:monitor-0', 'offset': offset})
            pieces.append(page['text'])
            if page['nextOffset'] is None:
                break
            offset = page['nextOffset']
        self.assertEqual(''.join(pieces), full)
        with self.runtime.db() as db:
            self.assertEqual(before, [r[0] for r in db.execute('SELECT text FROM runtime_events WHERE agent=?', (lead['id'],))])
        other = self.lead('Other')
        with self.assertRaisesRegex(ValueError, 'not owned'):
            self.runtime.model_read(other['id'], {'output_ref': 'event:monitor-0'})
        user = {'id': 'user', 'kind': 'user', 'text': 'Actual instruction ' * 5000}
        self.assertEqual(self.runtime.model_event_text([user]), user['text'])

    def test_event_reader_rechecks_sender_team_and_keeps_provenance(self):
        lead = self.start(self.lead())
        worker = self.worker(lead)
        self.runtime.chat_message(worker['id'], lead['id'], 'Original sender text', 'message')
        key = 'event:chat:message:' + lead['id']
        value = self.runtime.model_read(lead['id'], {'output_ref': key})
        self.assertEqual(json.loads(value['text'])['sender'], worker['id'])
        self.assertIn('turn_id', value['identity'])
        other = self.lead('Other')
        self.agent_update(worker, rootId=other['id'])
        with self.assertRaisesRegex(ValueError, 'team'):
            self.runtime.model_read(lead['id'], {'output_ref': key})

    def test_chat_model_pages_keep_every_cursor_and_recover_full_messages(self):
        lead = self.lead()
        worker = self.worker(lead)
        body = 'α"' * 5500
        for i in range(102):
            receipt = self.runtime.chat_message(worker['id'], lead['id'], body + str(i), 'history-' + str(i))
        room = receipt['room']
        full = self.runtime.chat_read(room, lead['id'])
        self.assertEqual(len(full['messages']), 100)
        ids, before = [], None
        while True:
            page = self.runtime.chat_read(room, lead['id'], before, model=True)
            self.assertLess(len(packed(page).encode()), 16000)
            ids.extend(m['id'] for m in page['messages'])
            before = page['nextBefore']
            if before is None:
                break
        self.assertEqual(len(ids), 102)
        self.assertEqual(len(set(ids)), 102)
        first = self.value(self.tool(lead, 'orchestration_chat_read', {'room_id': room}))
        self.assertIn('nextBefore', first)
        self.assertNotIn('outputRef', first)
        message = first['messages'][-1]
        self.assertTrue(message['truncated'])
        read = self.runtime.model_read(lead['id'], {**{'output_ref': message['read']['output_ref']}, 'offset': len(body)})
        self.assertEqual(read['text'], '101')
        other = self.lead('Other')
        with self.assertRaisesRegex(ValueError, 'participant'):
            self.runtime.model_read(other['id'], {'output_ref': message['read']['output_ref']})
        projected = self.runtime.model_tool_result(lead['id'], 'large-history', {'success': True,
            'contentItems': [{'type': 'inputText', 'text': packed(full)}]})
        self.assertEqual(self.value(projected)['summary']['nextBefore'], full['nextBefore'])

    def test_peer_cursor_discovers_each_room_once_with_bounded_pages(self):
        lead = self.lead()
        for i in range(63):
            worker = self.worker(lead, str(i))
            self.runtime.chat_message(worker['id'], lead['id'], 'Room discovery', 'room-' + str(i))
        agent_ids, room_ids, args = [], [], {}
        while True:
            page = self.runtime.model_directory(lead['id'], 'orchestration_peers', args)
            self.assertLess(len(packed(page).encode()), 16000)
            agent_ids.extend(a['id'] for a in page['items'])
            room_ids.extend(r['id'] for r in page['rooms'])
            if not page['nextCursor']:
                break
            args = {'cursor': page['nextCursor']}
        self.assertEqual(len(agent_ids), 64)
        self.assertEqual(len(set(agent_ids)), 64)
        self.assertEqual(len(room_ids), 63)
        self.assertEqual(len(set(room_ids)), 63)

    def test_unknown_progress_and_complaint_payloads_remain_visible(self):
        rows = []
        base = {'sender': 'worker', 'room': 'room', 'progress_key': 'topic',
                'progress_version': 1, 'importance': 'progress'}
        for i, changes in enumerate(({'sender': {}}, {'room': []}, {'progress_key': ''},
                                     {'progress_version': -1}, {'progress_version': True})):
            rows.append({'id': str(i), 'kind': 'agent_message', 'text': packed({**base, **changes, 'text': 'marker-' + str(i)})})
        rows.append({'id': 'valid', 'kind': 'agent_message', 'text': packed({**base, 'text': 'Valid progress'})})
        rows.append({'id': 'unknown', 'kind': 'complaint', 'preserveComplaint': True,
                     'text': packed({'complaints': [{'complaint_id': 'unknown', 'text': 'Unknown complaint'}]})})
        text = self.runtime.model_event_text(rows)
        for i in range(5):
            self.assertIn('marker-' + str(i), text)
        self.assertIn('Unknown complaint', text)


if __name__ == '__main__':
    unittest.main(verbosity=2)

#!/usr/bin/env python3
"""Server team boundaries through model tools, history, and native dispatch."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import os
import runpy
import socket
import sys
from pathlib import Path
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import codex_api_client

spec = importlib.util.spec_from_file_location('team_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class TeamChatIsolation(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update
    tool = f.WorkspaceContract.tool

    def historical(self, sender, recipient, global_room=False):
        room = ({'id': 'broadcast:all', 'kind': 'broadcast', 'rootId': 'all'} if global_room else
                {'id': 'private:' + ':'.join(sorted([sender['id'], recipient['id']])),
                 'kind': 'private', 'members': sorted([sender['id'], recipient['id']])})
        room['updated'] = time.time()
        key = 'history:' + room['id']
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'rooms', room)
            payload = json.dumps({'sender': sender['id'], 'room': room['id'], 'message_id': key,
                                  'text': 'isolationsecret'})
            self.runtime.enqueue(db, recipient, 'agent_message', payload, key)
            db.execute('INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES (?,?,?,?,?,?)',
                       (key, room['id'], sender['id'], 'isolationsecret', room['updated'],
                        json.dumps({recipient['id']: 'queued'})))
        return room, key

    def test_combined_feed_scope_and_pagination(self):
        lead, other = self.lead(), self.lead("Other")
        worker, outsider = self.worker(lead), self.worker(other)
        first, _ = self.historical(lead, worker)
        self.historical(other, outsider)
        page = self.runtime.chat_read("feed:" + lead["id"], limit=1)
        self.assertEqual([m["room"] for m in page["messages"]], [first["id"]])
        self.assertIsNone(page["nextBefore"])
        before = page["messages"][0]["seq"]
        self.assertEqual(self.runtime.chat_read("feed:" + lead["id"], before=before)["messages"], [])
        with self.assertRaises(ValueError):
            self.runtime.chat_read("feed:" + lead["id"], viewer=worker["id"])
        with self.assertRaises(ValueError):
            self.runtime.chat_read("feed:" + lead["id"], model=True)
        with self.assertRaises(ValueError):
            self.runtime.chat_read("feed:" + worker["id"])

    def test_room_pages_are_byte_bounded_bidirectional_and_targeted(self):
        lead, outsider = self.lead(), self.lead("Outside")
        worker = self.worker(lead)
        receipt = self.runtime.chat_message(lead["id"], worker["id"], "seed", "room-page-seed")
        with self.runtime.lock, self.runtime.db() as db:
            for index in range(180):
                db.execute(
                    "INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) "
                    "VALUES (?,?,?,?,?,?)",
                    (f"page-{index}", receipt["room"], lead["id"], "x" * 12000,
                     time.time() + index / 1000, "{}"),
                )
        original = self.runtime.chat_rooms

        def targeted(db, viewer=None, room_id=None, *, include_last_message=True):
            self.assertEqual(room_id, receipt["room"])
            self.assertFalse(include_last_message)
            return original(db, viewer, room_id, include_last_message=include_last_message)

        with patch.object(self.runtime, "chat_rooms", side_effect=targeted), \
                patch.object(self.runtime, "records", side_effect=AssertionError("global record scan")):
            latest = self.runtime.chat_read(receipt["room"], worker["id"])
            older = self.runtime.chat_read(receipt["room"], worker["id"],
                                           before=latest["nextBefore"])
            newer = self.runtime.chat_read(receipt["room"], worker["id"],
                                           after=older["nextAfter"])
        for page in (latest, older, newer):
            self.assertLessEqual(len(page["messages"]), 100)
            size = sum(len(message["text"].encode()) +
                       len(json.dumps(message["deliveries"]).encode()) + 256
                       for message in page["messages"])
            self.assertLessEqual(size, 1_000_000)
        self.assertIsNotNone(latest["nextBefore"])
        self.assertIsNotNone(older["nextAfter"])
        self.assertLess(older["messages"][-1]["seq"], latest["messages"][0]["seq"])
        self.assertEqual(newer["messages"][0]["seq"], older["nextAfter"] + 1)
        with self.assertRaisesRegex(ValueError, "not a participant"):
            self.runtime.chat_read(receipt["room"], outsider["id"])

    def test_cross_team_tools_fail_before_any_write_or_wake(self):
        lead, other = self.lead(), self.lead('Other')
        worker = self.worker(lead)
        for name, args in [
            ('orchestration_message', {'target': other['id'], 'text': 'Forbidden'}),
            ('orchestration_message', {'target': 'all', 'text': 'Forbidden'}),
            ('orchestration_send', {'agent_id': other['id'], 'text': 'Forbidden'}),
            ('orchestration_send', {'agent_id': 'all', 'text': 'Forbidden'}),
        ]:
            with self.subTest(name=name, args=args):
                result = self.tool(worker, name, args)
                self.assertFalse(result['success'], result)
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_chat_messages').fetchone()[0], 0)
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE agent=?", (other['id'],)).fetchone()[0], 0)
        self.assertEqual(self.runtime.agent(other['id'])['status'], other['status'])
        with self.assertRaisesRegex(ValueError, 'another team'):
            self.runtime.send(other['id'], 'Forbidden', 'direct-send', manual=False, resume=True,
                              sender=worker['id'], sender_epoch=worker['epoch'])

    def test_short_agent_ids_route_tools_and_reject_ambiguous_or_foreign_ids(self):
        lead, foreign = self.lead(), self.lead('Foreign')
        worker = self.worker(lead)
        short = worker['id'][:8]
        message = self.tool(lead, 'orchestration_message', {'target': short, 'text': 'Hello'})
        self.assertTrue(message['success'], message)
        sent = self.tool(lead, 'orchestration_send', {'agent_id': short, 'text': 'Continue'})
        self.assertTrue(sent['success'], sent)
        inspected = self.tool(lead, 'orchestration_agent_manage', {'action': 'inspect', 'agent_id': short})
        self.assertTrue(inspected['success'], inspected)
        self.assertEqual(json.loads(inspected['contentItems'][0]['text'])['agent']['id'], worker['id'])
        assigned = self.tool(lead, 'orchestration_task',
                             {'action': 'create', 'title': 'Short owner', 'owner': short})
        self.assertTrue(assigned['success'], assigned)
        missing = self.tool(lead, 'orchestration_message', {'target': 'ffffffff', 'text': 'Hello'})
        self.assertFalse(missing['success'])
        self.assertIn(worker['id'], missing['contentItems'][0]['text'])
        too_short = self.tool(lead, 'orchestration_message', {'target': worker['id'][:7], 'text': 'Hello'})
        self.assertFalse(too_short['success'])
        self.assertIn(worker['id'], too_short['contentItems'][0]['text'])
        denied = self.tool(lead, 'orchestration_message', {'target': foreign['id'][:8], 'text': 'Hello'})
        self.assertFalse(denied['success'])
        with self.runtime.lock, self.runtime.db() as db:
            for suffix in ('1111', '2222'):
                clone = {**worker, 'id': 'abcdef12-' + suffix, 'threadId': None,
                         'status': 'completed', 'autoWake': False}
                self.runtime.put(db, 'agents', clone)
        ambiguous = self.tool(lead, 'orchestration_send', {'agent_id': 'abcdef12', 'text': 'Hello'})
        self.assertFalse(ambiguous['success'])
        self.assertIn('abcdef12-1111', ambiguous['contentItems'][0]['text'])
        self.assertIn('abcdef12-2222', ambiguous['contentItems'][0]['text'])

    def test_same_team_resume_and_sender_provenance(self):
        lead = self.lead()
        child = self.worker(lead)
        self.agent_update(child, autoWake=False, status='paused')
        result = self.runtime.send(child['id'], 'Continue this task', 'resume-once', manual=False,
                                   resume=True, sender=lead['id'], sender_epoch=lead['epoch'])
        self.assertEqual(result['status'], 'queued')
        with self.runtime.db() as db:
            meta = json.loads(db.execute('SELECT record FROM runtime_event_meta WHERE id=?', ('resume-once',)).fetchone()[0])
        self.assertEqual(meta['senderId'], lead['id'])
        retry = self.runtime.send(child['id'], 'Continue this task', 'resume-once', manual=False,
                                  resume=True, sender=lead['id'], sender_epoch=lead['epoch'])
        self.assertEqual(retry['id'], result['id'])

    def test_historical_foreign_rooms_hidden_from_agent_reads_and_search(self):
        lead, other = self.lead(), self.lead('Other')
        worker = self.worker(lead)
        rooms = [self.historical(other, worker), self.historical(lead, worker, True)]
        self.assertEqual({p['id'] for p in self.runtime.peers(worker['id'])['peers']}, {lead['id'], worker['id']})
        self.assertEqual(self.runtime.peers(worker['id'])['rooms'], [])
        for room, key in rooms:
            with self.assertRaisesRegex(ValueError, 'not a participant'):
                self.runtime.chat_read(room['id'], worker['id'])
            self.assertEqual(self.runtime.chat_read(room['id'])['messages'][0]['text'], 'isolationsecret')
        self.assertFalse(any(r['type'] == 'room' for r in self.runtime.search_work('isolationsecret', worker['id'])['results']))
        self.assertEqual(len([r for r in self.runtime.search_work('isolationsecret')['results'] if r['type'] == 'room']), 2)

    def test_dispatch_cancels_old_foreign_input_and_keeps_same_team_input(self):
        lead, other = self.lead(), self.lead('Other')
        worker = self.worker(lead)
        room, event = self.historical(other, worker)
        self.runtime.chat_message(lead['id'], worker['id'], 'Allowed task', 'allowed')
        with patch.object(self.runtime, 'delivery_executor') as delivery_executor:
            self.runtime.dispatch()
        submit = delivery_executor.return_value.submit
        batches = [call.args[2] for call in submit.call_args_list if call.args[0] == self.runtime.start]
        self.assertEqual(len(batches), 1)
        self.assertEqual([r['id'] for r in batches[0]], ['chat:allowed:' + worker['id']])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?', (event,)).fetchone()[0], 'cancelled')
            self.assertEqual(json.loads(db.execute('SELECT deliveries FROM runtime_chat_messages WHERE id=?', (event,)).fetchone()[0])[worker['id']], 'cancelled')

    def test_user_control_cli_refuses_inherited_agent_before_network(self):
        executable = Path(__file__).resolve().parents[1] / 'scripts/codex-control'
        for variable in ('CODEX_AGENT_OWNER', 'CODEX_BOARD_OWNER'):
            for command in (['send', 'other', 'Forbidden'], ['transcript', 'other'], ['list']):
                with self.subTest(variable=variable, command=command), patch.dict(os.environ, {variable: 'worker'}), \
                        patch.object(sys, 'argv', ['codex-control', *command]), \
                        patch.object(codex_api_client, 'open_request',
                                     side_effect=AssertionError('network escape')) as network, \
                        patch.object(socket, 'create_connection',
                                     side_effect=AssertionError('socket escape')) as connect, \
                        patch.object(socket.socket, 'connect',
                                     side_effect=AssertionError('socket escape')) as socket_connect, \
                        patch('sys.stderr'):
                    with self.assertRaises(SystemExit) as error:
                        runpy.run_path(str(executable), run_name='__main__')
                    self.assertEqual(error.exception.code, 1)
                    network.assert_not_called()
                    connect.assert_not_called()
                    socket_connect.assert_not_called()

    def test_reserved_foreign_batch_cannot_reach_native_start(self):
        lead, other = self.lead(), self.lead('Other')
        worker = self.worker(lead)
        _, event = self.historical(other, worker)
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='reserved' WHERE id=?", (event,))
            rows = [dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (event,)).fetchone())]
        worker = self.agent_update(worker, status='starting', inFlight=True,
                                   startAttempt={'id': 'attempt', 'events': [event], 'epoch': worker['epoch'], 'submitted': False})
        with patch.object(self.runtime, 'prepare') as prepare, patch.object(self.runtime, 'submit_reserved') as submit:
            self.runtime.start(worker, rows)
        prepare.assert_not_called()
        submit.assert_not_called()
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?', (event,)).fetchone()[0], 'failed')


if __name__ == '__main__':
    unittest.main()

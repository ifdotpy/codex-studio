#!/usr/bin/env python3
"""Shared radio receipts and turn order against an independent fake native store."""
import importlib.util
import json
from pathlib import Path
import sys
import unittest
import uuid

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_radio import manage, tick, observe_item, select_pending, validate_event, guard_message, guard_input, route_question_answer, holds_floor

spec = importlib.util.spec_from_file_location('peers', Path(__file__).with_name('peer-teams-contract.py'))
peers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(peers)


class Radio(unittest.TestCase):
    request = peers.PeerTeams.request
    mutate_agent = peers.PeerTeams.mutate_agent
    def setUp(self):
        peers.PeerTeams.setUp(self)
        r = self.runtime
        self.interrupts = []
        class Pool:
            def submit(_, fn, *args):
                fn(*args)
        r.pool = Pool()
        r.interrupt = lambda a: self.interrupts.append(a['turnId'])
        def enqueue(db, a, kind, text, key):
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                       (key, a['id'], kind, text, 'pending', 0, a['epoch'], None, None))
        r.enqueue = enqueue
        with r.db() as db:
            db.execute('DROP TABLE runtime_events')
            db.execute('CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT)')
            db.execute('CREATE TABLE runtime_rooms(id TEXT PRIMARY KEY,record TEXT)')
            db.execute('CREATE TABLE runtime_chat_messages(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE,room TEXT,sender TEXT,text TEXT,created REAL,deliveries TEXT)')
            db.execute('CREATE TABLE runtime_requests(id TEXT PRIMARY KEY,record TEXT)')
            db.execute('CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY)')
            db.execute('CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)')
            db.execute('CREATE TABLE runtime_search_rows(id TEXT,search_rowid INTEGER)')
            db.execute('CREATE TABLE runtime_search(body TEXT)')
        for key in ('a', 'b'):
            self.mutate_agent(key, epoch=1, name=key, status='idle', inFlight=False)
        peers.manage(r, self.request())
        self.action('open')

    def room(self):
        with self.runtime.db() as db:
            return json.loads(db.execute('SELECT record FROM runtime_rooms').fetchone()[0])

    def action(self, action, **extra):
        body = dict(action='radio', radio_action=action, path=self.path, team_id=self.team_id,
                    request_id=str(uuid.uuid4()))
        if action != 'open':
            body['expected_revision'] = self.room()['radio']['revision']
        return manage(self.runtime, body | extra)

    def tick(self):
        with self.runtime.lock, self.runtime.db() as db:
            tick(self.runtime, db)
        return self.room()['radio']

    def start(self):
        state = self.tick()
        active = state['active']
        key, eid = active['agentId'], active['eventId']
        turn = 'native-' + eid
        self.mutate_agent(key, inFlight=True, status='running', turnId=turn,
                          startAttempt=dict(events=[eid], epoch=1, submitted=True,
                                            threadId='native-' + key, observedTurnId=turn))
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='delivered',turn_id=? WHERE id=?", (turn, eid))
        return key, turn

    def answer(self, key, turn, text='reply', item='answer', status='completed'):
        with self.runtime.db() as db:
            observe_item(self.runtime, db, key, key + ':' + item, 'assistant', text, {'turnId': turn})
            db.execute('INSERT INTO runtime_completed_turns VALUES (?)', (key + ':' + turn,))
        self.mutate_agent(key, inFlight=False, status='idle', lastCompletedTurn=turn, lastCompletedTurnStatus=status)
        return self.tick()

    def test_two_replies_share_all_text_and_stop(self):
        self.action('send', text='question')
        a, turn = self.start()
        with self.runtime.db() as db:
            observe_item(self.runtime, db, a, a + ':comment', 'assistant', 'comment', {'turnId': turn})
            observe_item(self.runtime, db, a, a + ':answer', 'assistant', 'draft', {'turnId': turn})
        self.answer(a, turn, 'full reply')
        b, second = self.start()
        self.assertNotEqual(a, b)
        with self.runtime.db() as db:
            text = db.execute('SELECT text FROM runtime_events WHERE turn_id=?', (second,)).fetchone()[0]
            self.assertIn('comment', text)
            self.assertIn('full reply', text)
            self.assertNotIn('draft', text)
        state = self.answer(b, second)
        self.assertEqual(state['status'], 'idle')
        self.assertIsNone(self.tick()['active'])

    def test_four_replies_and_target(self):
        self.action('send', text='debate', rounds=2)
        order = []
        for _ in range(4):
            a, turn = self.start()
            order.append(a)
            self.answer(a, turn)
        self.assertEqual(order, ['a', 'b', 'a', 'b'])
        self.assertIsNone(self.tick()['active'])
        self.action('send', text='only b', target='b', rounds=2)
        a, turn = self.start()
        self.assertEqual(a, 'b')
        self.assertEqual(self.answer(a, turn)['status'], 'idle')

    def test_idempotence_and_stale_revision(self):
        key = str(uuid.uuid4())
        rev = self.room()['radio']['revision']
        first = self.action('send', text='once', request_id=key)
        second = self.action('send', text='once', request_id=key, expected_revision=rev)
        self.assertEqual(first, second)
        with self.assertRaises(ValueError):
            self.action('send', text='different', request_id=key, expected_revision=rev)
        with self.assertRaises(ValueError):
            self.action('send', text='stale', expected_revision=rev)

    def test_unrelated_work_waits_and_radio_is_alone(self):
        with self.runtime.db() as db:
            self.runtime.enqueue(db, dict(id='a', epoch=1), 'user', 'independent', 'other')
        self.action('send', text='question')
        self.assertIsNone(self.tick()['active'])
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='delivered' WHERE id='other'")
        self.tick()
        with self.runtime.db() as db:
            rows = db.execute('SELECT * FROM runtime_events').fetchall()
            agent = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='a'").fetchone()[0])
            selected = select_pending(self.runtime, db, agent, rows)
            self.assertEqual(len(selected), 1)
            self.assertEqual(selected[0]['kind'], 'radio_turn')

    def test_pass_and_stop_exact_once(self):
        self.action('send', text='question')
        a, turn = self.start()
        self.action('pass', target='a')
        self.assertEqual(self.room()['radio']['next'], ['a'])
        self.action('stop')
        self.tick()
        self.tick()
        self.assertEqual(self.interrupts, [turn])
        self.assertEqual(self.answer(a, turn, status='interrupted')['status'], 'idle')
        with self.runtime.db() as db:
            agent = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='a'").fetchone()[0])
            self.assertTrue(agent['autoWake'])

    def test_membership_revocation_blocks_and_hides_new_output(self):
        self.action('send', text='secret')
        a, turn = self.start()
        peers.manage(self.runtime, self.request(expected_revision=1, members=['a', 'c']))
        with self.runtime.db() as db:
            observe_item(self.runtime, db, a, 'secret-output', 'assistant', 'private reply', {'turnId': turn})
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_chat_messages').fetchone()[0], 1)
        self.assertEqual(self.tick()['status'], 'blocked')
        with self.assertRaises(ValueError):
            self.action('open')

    def test_uncertain_does_not_repeat_or_advance(self):
        self.action('send', text='question')
        active = self.tick()['active']
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='uncertain' WHERE id=?", (active['eventId'],))
        self.assertEqual(self.tick()['status'], 'blocked')
        self.action('send', text='continue')
        self.assertEqual(self.tick()['active']['eventId'], active['eventId'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_events').fetchone()[0], 1)

    def test_unrelated_turn_and_direct_message(self):
        self.action('send', text='question')
        a, turn = self.start()
        with self.runtime.db() as db:
            observe_item(self.runtime, db, a, 'wrong', 'assistant', 'not shared', {'turnId': 'other'})
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_chat_messages').fetchone()[0], 1)
            with self.assertRaises(ValueError):
                guard_message(self.runtime, db, a, 'b')

    def test_completion_reconciles_full_text_and_preserves_stream_sequence(self):
        self.action('send', text='question')
        a, turn = self.start()
        full = 'x' * 25000
        with self.runtime.db() as db:
            observe_item(self.runtime, db, a, a + ':long', 'assistant', 'partial', {'turnId': turn})
            original = db.execute("SELECT seq,created FROM runtime_chat_messages WHERE sender=?", (a,)).fetchone()
            item = dict(role='assistant', text=full[:20000], truncated=True, turnId=turn)
            db.execute('INSERT INTO runtime_items VALUES (?,?,?,?)', (a + ':long', a, json.dumps(item), 0))
            rowid = db.execute('INSERT INTO runtime_search(body) VALUES (?)', (full,)).lastrowid
            db.execute('INSERT INTO runtime_search_rows VALUES (?,?)', (a + ':long', rowid))
        self.answer(a, turn)
        with self.runtime.db() as db:
            restored = db.execute('SELECT seq,created,text FROM runtime_chat_messages WHERE seq=?', (original['seq'],)).fetchone()
            self.assertEqual(restored['text'], full)
            self.assertEqual(restored['created'], original['created'])

    def test_observed_turn_before_ack_mirrors_but_does_not_advance(self):
        self.action('send', text='question')
        a, turn = self.start()
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='dispatching',turn_id=NULL")
            observe_item(self.runtime, db, a, 'early', 'assistant', 'early commentary', {'turnId': turn})
            self.assertEqual(db.execute("SELECT text FROM runtime_chat_messages WHERE sender=?", (a,)).fetchone()[0], 'early commentary')
        self.answer(a, turn)
        self.assertEqual(self.room()['radio']['status'], 'blocked')

    def test_stop_after_membership_revocation_interrupts_original_turn(self):
        self.action('send', text='question')
        a, turn = self.start()
        peers.manage(self.runtime, self.request(expected_revision=1, members=['a', 'c']))
        self.assertEqual(self.tick()['status'], 'blocked')
        self.action('stop')
        self.assertEqual(self.tick()['status'], 'stopping')
        self.tick()
        self.assertEqual(self.interrupts, [turn])
        self.assertEqual(self.answer(a, turn, status='interrupted')['status'], 'idle')

    def test_first_native_thread_binding_requires_exact_reserved_attempt(self):
        self.mutate_agent('a', threadId=None)
        self.action('send', text='first message')
        active = self.tick()['active']
        self.mutate_agent('a', threadId='new-thread', startAttempt={
            'events': [active['eventId']], 'epoch': 1, 'submitted': False})
        with self.runtime.db() as db:
            agent = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='a'").fetchone()[0])
            event = db.execute('SELECT * FROM runtime_events WHERE id=?', (active['eventId'],)).fetchone()
            self.assertIsNone(validate_event(self.runtime, db, {'id': agent['id']}, event))
        self.assertEqual(self.room()['radio']['active']['threadId'], 'new-thread')

    def test_cancelled_unsent_reply_can_resume_after_agent_stop(self):
        self.action('send', text='question')
        old = self.tick()['active']['eventId']
        self.mutate_agent('a', autoWake=False)
        self.assertIsNone(self.tick()['active'])
        self.mutate_agent('a', autoWake=True)
        self.action('send', text='continue')
        self.assertNotEqual(self.tick()['active']['eventId'], old)

    def test_native_stop_cancelled_pending_can_be_stopped_and_replaced(self):
        self.action('send', text='question')
        old = self.tick()['active']['eventId']
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='cancelled' WHERE id=?", (old,))
        self.mutate_agent('a', autoWake=False, epoch=2)
        self.action('stop')
        self.assertIsNone(self.room()['radio']['active'])
        self.mutate_agent('a', autoWake=True)
        self.action('send', text='continue')
        self.assertNotEqual(self.tick()['active']['eventId'], old)

    def test_cancelled_receipt_with_submission_evidence_is_not_cleared(self):
        self.action('send', text='question')
        active = self.tick()['active']
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='cancelled' WHERE id=?", (active['eventId'],))
        self.mutate_agent('a', startAttempt={'events': [active['eventId']], 'submitted': True})
        self.assertIsNotNone(self.tick()['active'])
        self.action('stop')
        self.assertIsNotNone(self.room()['radio']['active'])

    def test_reconciliation_uses_native_order_and_creation_times(self):
        self.action('send', text='question')
        a, turn = self.start()
        with self.runtime.db() as db:
            for key, at in [('later', 20), ('earlier', 10)]:
                item = dict(role='assistant', text=key, turnId=turn)
                db.execute('INSERT INTO runtime_items VALUES (?,?,?,?)', (a + ':' + key, a, json.dumps(item), at))
        self.answer(a, turn)
        with self.runtime.db() as db:
            rows = db.execute("SELECT text,created FROM runtime_chat_messages WHERE text IN ('earlier','later') ORDER BY seq").fetchall()
            self.assertEqual([(r['text'], r['created']) for r in rows], [('earlier', 10), ('later', 20)])

    def test_private_input_guard_allows_only_current_shared_question(self):
        self.action('send', text='question')
        a, turn = self.start()
        with self.runtime.db() as db:
            with self.assertRaisesRegex(ValueError, 'Send there or choose queue'):
                guard_input(self.runtime, db, a)
            question = dict(agent=a, method='agent/asyncQuestion', status='pending', epoch=1, turnId=turn)
            guard_input(self.runtime, db, a, question=question)
            with self.assertRaises(ValueError):
                guard_input(self.runtime, db, a, question=question | {'turnId': 'old'})
            guard_input(self.runtime, db, 'b')
        self.answer(a, turn)
        with self.runtime.db() as db:
            guard_input(self.runtime, db, a)

    def test_completed_question_holds_floor_and_answer_resumes_same_agent(self):
        self.action('send', text='question')
        a, turn = self.start()
        question = dict(id='question-1', agent=a, method='agent/asyncQuestion', status='pending', epoch=1, turnId=turn)
        with self.runtime.db() as db:
            self.runtime.put(db, 'requests', question)
        self.answer(a, turn, text='Which option?')
        self.assertEqual(self.tick()['active']['agentId'], a)
        self.assertEqual(self.tick()['status'], 'waiting')
        with self.runtime.db() as db:
            agent = json.loads(db.execute('SELECT record FROM runtime_agents WHERE id=?', (a,)).fetchone()[0])
            self.assertTrue(holds_floor(self.runtime, db, agent))
            self.assertEqual(select_pending(self.runtime, db, agent, [{'id': 'ordinary', 'kind': 'user'}]), [{'id': 'ordinary', 'kind': 'user'}])
            self.assertTrue(route_question_answer(self.runtime, db, question, 'Option A'))
            self.assertTrue(route_question_answer(self.runtime, db, question, 'Option A'))
            self.runtime.put(db, 'requests', question | {'status': 'answered'})
        state = self.tick()
        self.assertEqual(state['active']['agentId'], a)
        self.assertEqual(state['next'], ['b'])
        with self.runtime.db() as db:
            event = db.execute('SELECT text FROM runtime_events WHERE id=?', (state['active']['eventId'],)).fetchone()
            self.assertIn('Option A', event['text'])
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_chat_messages WHERE id='question-1:answer'").fetchone()[0], 1)

    def test_active_question_answer_is_mirrored_only_after_acceptance(self):
        self.action('send', text='question')
        a, turn = self.start()
        question = dict(id='question-1', agent=a, method='agent/asyncQuestion', status='pending', epoch=1, turnId=turn)
        with self.runtime.db() as db:
            self.assertFalse(route_question_answer(self.runtime, db, question, 'Option A'))
            self.assertIsNone(db.execute("SELECT 1 FROM runtime_chat_messages WHERE id='question-1:answer'").fetchone())
            self.assertFalse(route_question_answer(self.runtime, db, question, 'Option A', accepted=True))
            self.assertIsNotNone(db.execute("SELECT 1 FROM runtime_chat_messages WHERE id='question-1:answer'").fetchone())
        self.assertEqual(self.room()['radio']['next'], ['b'])

    def test_stopped_agent_rejected(self):
        self.mutate_agent('b', autoWake=False)
        with self.assertRaises(ValueError):
            self.action('send', text='question')


if __name__ == '__main__':
    unittest.main()

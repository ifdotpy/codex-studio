#!/usr/bin/env python3
"""A native index locates acceptance but cannot replace the saved receipt."""
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import codex_native_input_projection as projection
from codex_native_input_projection import accepted_turns


class SavedInput(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.path = self.home / 'sessions' / 'receipt.jsonl'
        self.path.parent.mkdir()
        self.item = {'type': 'userMessage', 'id': 'item', 'clientId': 'input',
                     'content': [{'type': 'text', 'text': 'Do the saved work.', 'text_elements': []}]}
        self.header = {'type': 'session_meta', 'ordinal': 4, 'payload': {'id': 'thread'}}
        self.start = {'type': 'event_msg', 'ordinal': 5,
                      'payload': {'type': 'task_started', 'turn_id': 'turn'}}
        self.raw_item = {'type': 'event_msg', 'ordinal': 6, 'payload': {
            'type': 'item_completed', 'thread_id': 'thread', 'turn_id': 'turn',
            'item': {'type': 'UserMessage', 'id': 'item', 'client_id': 'input',
                     'content': self.item['content']}}}
        self.save()
        with closing(sqlite3.connect(self.home / 'state_5.sqlite')) as db, db:
            db.execute('CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT)')
            db.execute('INSERT INTO threads VALUES (?,?)', ('thread', str(self.path)))
        with closing(sqlite3.connect(self.home / 'thread_history_1.sqlite')) as db, db:
            db.execute('CREATE TABLE thread_items(thread_id,turn_id,item_id,item_type,item_json,rollout_ordinal)')
            db.execute('CREATE TABLE thread_turns(thread_id,turn_id,rollout_ordinal,rollout_byte_offset)')
            db.execute('CREATE TABLE thread_history_projection_state(thread_id,next_rollout_byte_offset,next_rollout_ordinal)')
            db.execute('INSERT INTO thread_items VALUES (?,?,?,?,?,?)',
                       ('thread','turn','item','userMessage',json.dumps(self.item),6))
            db.execute('INSERT INTO thread_turns VALUES (?,?,?,?)', ('thread','turn',5,len(self.line(self.header))))
            db.execute('INSERT INTO thread_history_projection_state VALUES (?,?,?)',
                       ('thread', self.path.stat().st_size, 7))

    def line(self, value):
        return (json.dumps(value) + '\n').encode()

    def save(self):
        self.path.write_bytes(b''.join(self.line(x) for x in [self.header,self.start,self.raw_item]))

    def proof(self):
        return accepted_turns(self.home, 'thread', ['input'])

    def change_index(self, sql):
        with closing(sqlite3.connect(self.home / 'thread_history_1.sqlite')) as db, db:
            db.execute(sql)

    def test_exact_saved_acceptance(self):
        self.assertEqual(self.proof(), [{'id':'turn','startOutcome':'accepted',
            'clientUserMessageId':'input','items':[self.item]}])

    def test_absence_never_proves_rejection(self):
        self.assertEqual(accepted_turns(self.home, 'thread', ['missing']), [])

    def test_stale_index_without_saved_acceptance(self):
        self.raw_item['payload']['item']['client_id'] = 'another-input'
        self.save()
        self.assertEqual(self.proof(), [])

    def test_exact_thread_turn_and_item_required(self):
        for field in ['thread_id', 'turn_id']:
            with self.subTest(field=field):
                self.raw_item['payload'][field] = 'another'
                self.save()
                self.assertEqual(self.proof(), [])
                self.raw_item['payload'][field] = 'thread' if field == 'thread_id' else 'turn'
        self.raw_item['payload']['item']['id'] = 'another-item'
        self.save()
        self.assertEqual(self.proof(), [])

    def test_raw_content_must_match_index(self):
        self.raw_item['payload']['item']['content'] = [{'type':'text','text':'Changed'}]
        self.save()
        self.assertEqual(self.proof(), [])

    def test_saved_start_required(self):
        self.start['payload']['turn_id'] = 'another-turn'
        self.save()
        self.assertEqual(self.proof(), [])

    def test_duplicate_client_identity_refused(self):
        self.change_index('INSERT INTO thread_items SELECT * FROM thread_items')
        self.assertEqual(self.proof(), [])

    def test_projection_must_cover_saved_item(self):
        self.change_index('UPDATE thread_history_projection_state SET next_rollout_byte_offset=0')
        self.assertEqual(self.proof(), [])

    def test_rollout_outside_account_refused(self):
        outside = self.home.parent / (self.home.name + '-outside.jsonl')
        self.addCleanup(outside.unlink)
        outside.write_bytes(self.path.read_bytes())
        with closing(sqlite3.connect(self.home / 'state_5.sqlite')) as db, db:
            db.execute('UPDATE threads SET rollout_path=?',(str(outside),))
        self.assertEqual(self.proof(), [])

    def test_wrong_session_and_broken_offset_refused(self):
        self.header['payload']['id'] = 'another-thread'
        self.save()
        self.assertEqual(self.proof(), [])
        self.header['payload']['id'] = 'thread'
        self.save()
        self.change_index('UPDATE thread_turns SET rollout_byte_offset=1')
        self.assertEqual(self.proof(), [])

    def test_missing_databases_stay_missing(self):
        other = self.home / 'new-account'
        other.mkdir()
        self.assertEqual(accepted_turns(other,'thread',['input']), [])
        self.assertEqual(list(other.iterdir()), [])

    def test_symlink_swap_before_rollout_open_refused(self):
        outside = self.home.parent / (self.home.name + '-external.jsonl')
        self.addCleanup(outside.unlink)
        outside.write_bytes(self.path.read_bytes())
        original = projection._open_rollout

        def swap(home, relative):
            self.path.unlink()
            self.path.symlink_to(outside)
            return original(home, relative)

        with patch.object(projection, '_open_rollout', side_effect=swap):
            self.assertEqual(self.proof(), [])

    def test_fifo_refused_without_a_writer(self):
        self.path.unlink()
        os.mkfifo(self.path)
        self.assertEqual(self.proof(), [])

    def test_parent_directory_symlink_swap_refused(self):
        outside = self.home.parent / (self.home.name + '-external')
        outside.mkdir()
        self.addCleanup(outside.rmdir)
        outside_file = outside / self.path.name
        self.addCleanup(outside_file.unlink)
        outside_file.write_bytes(self.path.read_bytes())
        original = projection._open_rollout

        def swap(home, relative):
            self.path.unlink()
            self.path.parent.rmdir()
            self.path.parent.symlink_to(outside, target_is_directory=True)
            return original(home, relative)

        with patch.object(projection, '_open_rollout', side_effect=swap):
            self.assertEqual(self.proof(), [])

    def test_late_input_in_a_large_turn_is_proved_without_a_full_history_read(self):
        padding = [{'type':'response_item','ordinal':ordinal,
                    'payload':{'type':'reasoning','text':'x' * 900000}} for ordinal in [6,7,8]]
        self.raw_item['ordinal'] = 9
        self.path.write_bytes(b''.join(self.line(x) for x in
            [self.header,self.start,*padding,self.raw_item]))
        with closing(sqlite3.connect(self.home / 'thread_history_1.sqlite')) as db, db:
            db.execute('UPDATE thread_items SET rollout_ordinal=9')
            db.execute('UPDATE thread_history_projection_state SET next_rollout_byte_offset=?,next_rollout_ordinal=10',
                       (self.path.stat().st_size,))
        self.assertEqual(len(self.proof()), 1)

    def test_all_input_keys_share_one_scan_budget(self):
        records = [self.header]
        with closing(sqlite3.connect(self.home / 'thread_history_1.sqlite')) as db, db:
            db.execute('DELETE FROM thread_items')
            db.execute('DELETE FROM thread_turns')
            for ordinal, turn, key in [(5,'turn','input'), (10,'turn2','input2')]:
                offset = sum(len(self.line(record)) for record in records)
                records.append({'type':'event_msg','ordinal':ordinal,
                                'payload':{'type':'task_started','turn_id':turn}})
                records.extend({'type':'response_item','ordinal':i,
                                'payload':{'type':'reasoning','text':'x' * 900000}}
                               for i in range(ordinal + 1, ordinal + 4))
                item = {**self.item, 'id': key + '-item', 'clientId':key}
                records.append({'type':'event_msg','ordinal':ordinal + 4,'payload':{
                    'type':'item_completed','thread_id':'thread','turn_id':turn,
                    'item':{'type':'UserMessage','id':item['id'],'client_id':key,
                            'content':item['content']}}})
                db.execute('INSERT INTO thread_turns VALUES (?,?,?,?)',('thread',turn,ordinal,offset))
                db.execute('INSERT INTO thread_items VALUES (?,?,?,?,?,?)',
                           ('thread',turn,item['id'],'userMessage',json.dumps(item),ordinal + 4))
            self.path.write_bytes(b''.join(self.line(record) for record in records))
            db.execute('UPDATE thread_history_projection_state SET next_rollout_byte_offset=?,next_rollout_ordinal=15',
                       (self.path.stat().st_size,))
        with patch.object(projection, 'SCAN_LIMIT', 5 * 1024 * 1024):
            turns = accepted_turns(self.home,'thread',['input','input2'])
        self.assertEqual([turn['id'] for turn in turns], ['turn'])


if __name__ == '__main__':
    unittest.main()

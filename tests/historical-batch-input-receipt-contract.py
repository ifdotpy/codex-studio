#!/usr/bin/env python3
"""Exact historical batch acceptance retains the newer unsent input."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import sqlite3
import time
from types import SimpleNamespace
import unittest
import uuid

spec = importlib.util.spec_from_file_location('historical_input_fixture',
    Path(__file__).with_name('historical-context-input-wait-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_historical_input_receipts as batches


class HistoricalBatchReceipt(f.HistoricalInputWait):
    def batch(self):
        self.make_wait(native=False)
        self.primary = 'original-batch-primary'
        self.runtime.send(self.a['id'], 'Primary instruction.', message_id=self.primary)
        self.operation = 'turn:' + self.a['id'] + ':' + self.primary + ':attempt:' + str(uuid.uuid4())
        self.agent_update(self.runtime.agent(self.a['id']), model='gpt-6-luna',
                          effort='high', nativeEffort='high', fastMode=False,
                          cyberAccessProgram='standard', yoloMode=True)
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.a['id'], db)
            for key in (self.primary, self.old_id):
                metadata = json.loads(db.execute('SELECT record FROM runtime_event_meta WHERE id=?',
                                                 (key,)).fetchone()[0])
                metadata.update(modelEventProjection=1)
                if key == self.primary:
                    metadata.update(acceptedAt=1.5, contextManifest={
                        'epoch':[self.tid, 1], 'versions':{}, 'sequence':1})
                db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?',
                           (json.dumps(metadata), key))
            db.execute("UPDATE runtime_events SET status='delivered',turn_id='original-native-turn' WHERE id=?",
                       (self.primary,))
            rows = [dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (key,)).fetchone())
                    for key in (self.primary, self.old_id)]
            self.local_text = self.runtime.model_event_text(rows) + '\n\n[Saved context: retained]'
            self.runtime.item(db, agent['id'], self.primary, 'user', self.local_text, inputs=rows, assets=[])
        self.native_text = (self.local_text + '\n\n[Time awareness, message receipt time]\n'
                            'Message original-batch-primary accepted at 1970-01-01T00:00:01.500Z')
        self.request = {'method':'turn/start', 'params':{
            'threadId':self.tid, 'model':'gpt-6-luna', 'clientUserMessageId':self.primary,
            'input':[{'type':'text','text':self.native_text}], 'approvalPolicy':'never',
            'sandboxPolicy':{'type':'dangerFullAccess'}, 'serviceTier':'default',
            'cyberAccessProgram':'standard', 'effort':'high'}}
        self.digest = hashlib.sha256(json.dumps(self.request, sort_keys=True,
                                                separators=(',', ':')).encode()).hexdigest()
        self.journal = self.runtime.root / 'supervisor.sqlite3'
        with sqlite3.connect(self.journal) as db:
            # The deployed supervisor can have this older receipt schema.
            db.execute('CREATE TABLE operations(handle TEXT, operation_id TEXT, digest TEXT, '
                       'native_id INTEGER, accepted REAL, PRIMARY KEY(handle,operation_id))')
            db.execute('INSERT INTO operations VALUES (?,?,?,?,?)',
                       ('account:default', self.operation, self.digest, 17, 2.0))
        self.supervisor = {'stateDir':str(self.runtime.root), 'handle':'account:default', 'generation':1}
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(root=self.runtime.root, handle='account:default', generation=1)
        self.native_turns = [{'id':'original-native-turn', 'startOutcome':'accepted',
            'status':'completed', 'clientUserMessageId':self.primary,
            'items':[{'id':'accepted-primary-item', 'type':'userMessage', 'clientId':self.primary,
                      'content':[{'type':'text','text':self.native_text, 'text_elements':[]}]}]}]
        self.server.calls.clear()
        return self.capture_saved()

    def capture_saved(self):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.a['id'], db)
            events = db.execute('SELECT e.*,m.record AS metadata FROM runtime_events e '
                'JOIN runtime_event_meta m ON m.id=e.id WHERE e.id=?', (self.old_id,)).fetchall()
            return batches.capture_batches(db, agent, list(events))

    def proof(self, captures=None, turns=None, supervisor=None):
        return batches.accepted_batches(supervisor or self.supervisor,
            self.capture_saved() if captures is None else captures,
            self.native_turns if turns is None else turns)

    def test_exact_accepted_batch_proves_secondary_input(self):
        captures = self.batch()
        self.assertEqual(len(captures), 1)
        self.assertEqual(captures[0]['params'], self.request['params'])
        self.assertEqual(captures[0]['digest'], self.digest)
        self.assertEqual(self.proof(captures), {self.old_id:'original-native-turn'})
        self.assertEqual(self.capture_saved(), captures)
        self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
        self.assert_preserved_new_input()
        self.assertEqual(self.server.calls, [])

    def test_saved_batch_recovery_resolves_only_old_input(self):
        self.batch()
        result = f.repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'resolved', result)
        self.assertEqual(result['inputs'], [{'id':self.old_id, 'decision':'delivered',
                                            'turnId':'original-native-turn'}])
        self.assertEqual(self.receipt(self.old_id)['status'], 'delivered')
        self.assertEqual(self.receipt(self.primary)['turn_id'], 'original-native-turn')
        self.assert_preserved_new_input()
        self.assertFalse(self.runtime.agent(self.a['id']).get('contextRepairWait'))
        self.assertEqual(f.repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])['status'], 'not_needed')

    def test_automatic_check_resolves_the_same_batch_without_another_start(self):
        self.batch()
        self.runtime.dispatch()
        self.assertEqual(len(self.jobs), 1)
        self.run_check()
        self.assertEqual(self.receipt(self.old_id)['status'], 'delivered')
        self.assert_preserved_new_input()

    def test_digest_difference_cannot_prove_the_batch(self):
        self.batch()
        for field, value in (('model','another-model'), ('nativeEffort','medium'),
                             ('fastMode',True), ('yoloMode',False), ('threadId','another-thread')):
            with self.subTest(field=field):
                agent = self.runtime.agent(self.a['id'])
                self.agent_update(agent, **{field:value})
                self.assertEqual(self.proof(), {})
                self.agent_update(self.runtime.agent(self.a['id']), **{field:agent.get(field)})

    def test_exact_native_client_item_content_and_turn_are_required(self):
        captures = self.batch()
        for change in ('primary','content','item_id','turn_id','preparing','unknown','duplicate'):
            with self.subTest(change=change):
                turns = copy.deepcopy(self.native_turns)
                item = turns[0]['items'][0]
                if change == 'primary':
                    item['clientId'] = self.old_id
                elif change == 'content':
                    item['content'][0]['text'] += ' changed'
                elif change == 'item_id':
                    item['id'] = None
                elif change == 'turn_id':
                    turns[0]['id'] = 37
                elif change in {'preparing', 'unknown'}:
                    turns[0]['startOutcome'] = change
                else:
                    turns.append(copy.deepcopy(turns[0]))
                self.assertEqual(self.proof(captures, turns), {})
        self.assertEqual(self.proof(captures, []), {})
        self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')

    def test_empty_native_text_elements_are_the_only_content_normalization(self):
        captures = self.batch()
        for key in ('text_elements', 'textElements'):
            turns = copy.deepcopy(self.native_turns)
            part = turns[0]['items'][0]['content'][0]
            part.pop('text_elements', None)
            part[key] = []
            self.assertEqual(self.proof(captures, turns), {self.old_id:'original-native-turn'})
            part[key] = [{'start':0, 'end':1}]
            self.assertEqual(self.proof(captures, turns), {})
        turns = copy.deepcopy(self.native_turns)
        turns[0]['items'][0]['content'][0]['unknown_content'] = []
        self.assertEqual(self.proof(captures, turns), {})

    def test_supervisor_receipt_and_exact_account_are_required(self):
        captures = self.batch()
        wrong = {**self.supervisor, 'handle':'account:another-account'}
        self.assertEqual(self.proof(captures, supervisor=wrong), {})
        with sqlite3.connect(self.journal) as db:
            db.execute('DELETE FROM operations')
        self.assertEqual(self.proof(captures), {})
        with sqlite3.connect(self.journal) as db:
            db.execute('INSERT INTO operations VALUES (?,?,?,?,?)',
                       ('account:default', self.operation, 'f' * 64, 17, 2.0))
        self.assertEqual(self.proof(captures), {})

    def test_ambiguous_supervisor_attempts_are_not_acceptance(self):
        captures = self.batch()
        with sqlite3.connect(self.journal) as db:
            db.execute('INSERT INTO operations VALUES (?,?,?,?,?)',
                ('account:default', self.operation.rsplit(':', 1)[0] + ':' + str(uuid.uuid4()),
                 self.digest, 18, 3.0))
        self.assertEqual(self.proof(captures), {})

    def test_local_input_identity_and_metadata_changes_reject_capture(self):
        original = self.batch()
        with self.runtime.db() as db:
            old = dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (self.old_id,)).fetchone())
            for field, value in (('text','changed input'), ('created',9), ('kind','user'),
                                 ('agent','another-agent'), ('epoch',old['epoch'] + 1),
                                 ('status','cancelled')):
                with self.subTest(field=field):
                    db.execute('UPDATE runtime_events SET ' + field + '=? WHERE id=?', (value, self.old_id))
                    self.assertEqual(batches.capture_batches(db, self.runtime.agent(self.a['id'], db),
                        [dict(db.execute('SELECT e.*,m.record AS metadata FROM runtime_events e '
                            'JOIN runtime_event_meta m ON m.id=e.id WHERE e.id=?', (self.old_id,)).fetchone())]), [])
                    db.execute('UPDATE runtime_events SET ' + field + '=? WHERE id=?', (old[field], self.old_id))
            db.execute("UPDATE runtime_event_meta SET record=json_set(record,'$.timing.fixture',1) WHERE id=?",
                       (self.primary,))
        self.assertNotEqual(self.capture_saved(), original)

    def test_changed_batch_or_wrong_scope_cannot_use_native_text_match(self):
        self.batch()
        with self.runtime.db() as db:
            metadata = json.loads(db.execute('SELECT record FROM runtime_event_meta WHERE id=?',
                                            (self.old_id,)).fetchone()[0])
            for change in ('item','manifest','native','assets'):
                with self.subTest(change=change):
                    changed = copy.deepcopy(metadata)
                    if change == 'item':
                        changed['transcriptItemId'] = self.a['id'] + ':unrelated-primary'
                    elif change == 'manifest':
                        changed['contextManifest']['epoch'][0] = 'another-thread'
                    elif change == 'native':
                        changed['native'] = {'agent':self.a['id'], 'epoch':self.a['epoch'],
                            'threadId':self.tid, 'accountKey':'another-account'}
                    else:
                        changed['assets'] = ['unresolved-attachment']
                    db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?',
                               (json.dumps(changed), self.old_id))
                    agent = self.runtime.agent(self.a['id'], db)
                    row = dict(db.execute('SELECT e.*,m.record AS metadata FROM runtime_events e '
                        'JOIN runtime_event_meta m ON m.id=e.id WHERE e.id=?', (self.old_id,)).fetchone())
                    self.assertEqual(batches.capture_batches(db, agent, [row]), [])

    def test_locked_supervisor_does_not_write_or_wait_for_default_timeout(self):
        captures = self.batch()
        connection = sqlite3.connect(self.journal)
        try:
            connection.execute('BEGIN EXCLUSIVE')
            started = time.monotonic()
            self.assertEqual(self.proof(captures), {})
            self.assertLess(time.monotonic() - started, .5)
        finally:
            connection.rollback()
            connection.close()
        self.assertEqual(self.proof(captures), {self.old_id:'original-native-turn'})

    def test_stop_and_changed_connection_preserve_every_receipt(self):
        self.batch()
        original = self.runtime.agent(self.a['id'])
        for changes in ({'autoWake':False}, {'status':'paused'}, {'nativeFailureHold':True}):
            with self.subTest(changes=changes):
                self.agent_update(self.runtime.agent(self.a['id']), **changes)
                result = f.repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
                self.assertEqual(result['status'], 'waiting')
                self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
                self.assertEqual(self.server.calls, [])
                self.agent_update(self.runtime.agent(self.a['id']), **{key:original.get(key) for key in changes})
        self.before_history = lambda:self.runtime.connection_ids.update(default='changed-connection')
        result = f.repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'waiting')
        self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
        self.assert_preserved_new_input()

    def test_changed_local_batch_blocks_the_final_apply(self):
        self.batch()
        def change_batch():
            with self.runtime.db() as db:
                db.execute("UPDATE runtime_event_meta SET record=json_set(record,'$.timing.changed',1) WHERE id=?",
                           (self.primary,))
        self.before_history = change_batch
        result = f.repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'waiting', result)
        self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
        self.assert_preserved_new_input()

    def test_changed_supervisor_generation_blocks_the_final_apply(self):
        self.batch()
        self.before_history = lambda:setattr(self.server.proc, 'generation', 2)
        result = f.repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'waiting', result)
        self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
        self.assert_preserved_new_input()


if __name__ == '__main__':
    suite = unittest.TestSuite(HistoricalBatchReceipt(name) for name in HistoricalBatchReceipt.__dict__
                              if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())

#!/usr/bin/env python3
"""Transfer receipts, native boundaries, team identity and history conservation."""
import concurrent.futures
import importlib.util
import json
from pathlib import Path
import threading
import time
import unittest
import uuid
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('accounts_fixture', Path(__file__).with_name('runtime-accounts-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_account_transfer import transfer_store


class TransferContract(f.AccountContracts):
    # Only this subclass's transfer cases run below.
    def setUp(self):
        super().setUp()
        self.store = transfer_store(self.runtime)
        self.lead_agent = self.lead()
        self.set_agent(self.lead_agent['id'], status='complete', inFlight=False, threadId='native-source', turnId=None)
        self.target_server = self.runtime.connect(self.other_key)
        self.source_server = self.runtime.connect('default')
        self.pending = []
        self.native_calls = []
        original = self.source_server.call
        def call(method, params, timeout=60):
            self.native_calls.append((method, params))
            if method == 'thread/read':
                return {'thread': {'id': params['threadId'], 'status': {'type': 'idle'}, 'path': '/fixture/rollout.jsonl'}}
            if method == 'thread/backgroundTerminals/list': return {'data': []}
            if method == 'thread/unsubscribe': return {}
            return original(method, params, timeout)
        self.source_server.call = call
        self.source_server.after_events = lambda cb: cb()
        original_submit = self.target_server.submit
        def submit(method, params):
            if method not in {"thread/fork", "thread/start"}: return original_submit(method, params)
            future = concurrent.futures.Future()
            self.pending.append((method, params, future))
            return future
        self.target_server.submit = submit
        self.copy_patch = patch.object(self.store, 'copy_history', return_value=Path('/fixture/import.jsonl'))
        self.copy_patch.start()

    def tearDown(self):
        self.until(lambda:not self.store.running)
        self.copy_patch.stop()
        super().tearDown()

    def set_agent(self, aid, **changes):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(aid, db)
            a.update(changes)
            self.runtime.put(db, 'agents', a)
        return a

    def start_transfer(self):
        return self.store.request(self.lead_agent['id'], self.other_key, str(uuid.uuid4()))

    def tick(self):
        with self.runtime.lock, self.runtime.db() as db:
            self.store.tick(self.runtime.records(db, 'agents'))

    def until(self, fn):
        deadline = time.monotonic()+3
        while not fn():
            if time.monotonic()>deadline: self.fail('Transfer fixture deadline')
            time.sleep(.005)

    def receipt(self, key):
        with self.runtime.db() as db: return self.store.get(db, key)

    def complete_fork(self, index=0):
        self.until(lambda:not self.store.running)
        self.pending[index][2].set_result({'thread': {'id': 'target-thread-'+str(index)}, 'model':'gpt-6-astra'})

    def test_transfer_keeps_identity_history_queue_and_settings(self):
        aid = self.lead_agent['id']
        self.set_agent(aid, compactions=17, panelVersion=3)
        op = self.start_transfer()
        self.runtime.send(aid, 'Follow up once', 'message-1', delivery='after_tool')
        self.tick()
        self.until(lambda:len(self.pending)==1)
        self.assertEqual(self.runtime.agent(aid)['accountKey'], 'default')
        self.assertEqual(self.pending[0][0], 'thread/fork')
        self.assertTrue(self.pending[0][1]['deferGoalContinuation'])
        self.complete_fork()
        a = self.runtime.agent(aid)
        self.assertEqual(a['accountKey'], self.other_key)
        self.assertEqual(a['compactions'], 17)
        self.assertEqual(a['panelVersion'], 3)
        self.assertEqual(a['threadId'], 'target-thread-0')
        self.assertNotIn('accountTransferId', a)
        with self.runtime.db() as db:
            row = db.execute("SELECT * FROM runtime_events WHERE id='message-1'").fetchone()
            self.assertEqual(row['status'], 'pending')
            self.assertEqual(row['epoch'], a['epoch'])
        self.tick()
        self.assertEqual(self.receipt(op['id'])['status'], 'completed')
        self.assertFalse(any(method=='turn/start' for method, _ in self.native_calls))

    def test_busy_turn_waits_and_late_receipt_never_replays(self):
        aid=self.lead_agent['id']
        self.set_agent(aid,status='running',inFlight=True)
        op=self.start_transfer();self.tick()
        self.assertEqual(self.pending,[])
        self.set_agent(aid,status='complete',inFlight=False)
        self.tick();self.until(lambda:len(self.pending)==1)
        for _ in range(5):self.tick()
        self.assertEqual(len(self.pending),1)
        self.assertEqual(self.store.request(aid,self.other_key,op['id'])['id'],op['id'])
        with self.assertRaises(ValueError):self.store.request(aid,'default',op['id'])
        self.complete_fork()

    def test_cancel_then_late_fork_does_not_change_account(self):
        op=self.start_transfer();self.tick();self.until(lambda:len(self.pending)==1)
        self.store.action(op['id'],'cancel');self.complete_fork()
        a=self.runtime.agent(self.lead_agent['id'])
        self.assertEqual(a['accountKey'],'default')
        self.assertNotIn('accountTransferId',a)
        self.assertEqual(self.receipt(op['id'])['members'][a['id']]['result']['thread']['id'],'target-thread-0')

    def test_epoch_change_never_adopts_fork(self):
        op=self.start_transfer();self.tick();self.until(lambda:len(self.pending)==1)
        a=self.runtime.agent(self.lead_agent['id']);self.set_agent(a['id'],epoch=a['epoch']+1)
        self.complete_fork()
        self.assertEqual(self.runtime.agent(a['id'])['accountKey'],'default')
        self.assertEqual(self.receipt(op['id'])['members'][a['id']]['phase'],'unknown')

    def test_unknown_mutation_is_not_retried(self):
        op=self.start_transfer();self.tick();self.until(lambda:len(self.pending)==1)
        self.pending[0][2].set_exception(RuntimeError('Disconnected; outcome unknown'))
        self.store.action(op['id'],'retry')
        for _ in range(3):self.tick()
        self.assertEqual(len(self.pending),1)
        self.assertEqual(self.receipt(op['id'])['members'][self.lead_agent['id']]['phase'],'unknown')

    def test_unloaded_native_thread_uses_saved_context_without_background_rpc(self):
        original = self.source_server.call
        def call(method, params, timeout=60):
            if method == 'thread/read':
                return {'thread': {'id': params['threadId'], 'status': {'type': 'notLoaded'}, 'path': '/fixture/rollout.jsonl'}}
            if method in {'thread/unsubscribe', 'thread/backgroundTerminals/list'}:
                raise AssertionError('Unloaded threads have no native background terminal owner')
            return original(method, params, timeout)
        self.source_server.call = call
        self.start_transfer(); self.tick(); self.until(lambda: len(self.pending) == 1)
        self.complete_fork()
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountKey'], self.other_key)

    def test_native_background_command_defers_member(self):
        original=self.source_server.call
        self.source_server.call=lambda method,params,timeout=60: {'data':[{'processId':'owned'}]} if method=='thread/backgroundTerminals/list' else original(method,params,timeout)
        op=self.start_transfer();self.tick()
        self.until(lambda:not self.store.running)
        self.assertEqual(self.pending,[])
        self.assertFalse(any(m=='thread/unsubscribe' for m,p in self.native_calls))

    def test_unknown_delivery_blocks_transfer(self):
        aid=self.lead_agent['id'];op=self.start_transfer()
        with self.runtime.db() as db:
            a=self.runtime.agent(aid,db)
            self.runtime.enqueue(db,a,'user','Do not repeat','uncertain-message')
            db.execute("UPDATE runtime_events SET status='uncertain' WHERE id='uncertain-message'")
        self.tick();self.assertEqual(self.pending,[])

    def test_workers_created_during_transfer_are_included(self):
        aid=self.lead_agent['id'];op=self.start_transfer()
        worker=self.runtime.create({'name':'Worker','prompt':'Task'},parent=aid,defer=True)
        self.set_agent(worker['id'], status='waiting', inFlight=False, threadId=None)
        self.tick();self.until(lambda:len(self.pending)==2)
        self.assertIn(worker['id'],self.receipt(op['id'])['members'])
        self.complete_fork(0);self.complete_fork(1);self.tick()
        self.assertEqual(self.runtime.agent(worker['id'])['parentId'],aid)
        self.assertEqual(self.runtime.agent(worker['id'])['accountKey'],self.other_key)
        self.assertEqual(self.receipt(op['id'])['status'],'completed')

    def test_restart_retains_unknown_receipt_and_never_replays_it(self):
        op=self.start_transfer();self.tick();self.until(lambda:len(self.pending)==1)
        self.until(lambda:not self.store.running)
        from codex_account_transfer import AccountTransfers
        restarted=AccountTransfers(self.runtime)
        with self.runtime.db() as db:
            self.assertEqual(restarted.get(db,op['id'])['members'][self.lead_agent['id']]['phase'],'unknown')
        self.assertTrue(self.runtime.agent(self.lead_agent['id'])['accountTransfer']['needsAttention'])
        restarted.action(op['id'],'retry')
        with self.runtime.db() as db:restarted.tick(self.runtime.records(db,'agents'))
        self.assertEqual(len(self.pending),1)
        self.complete_fork()

    def test_scheduler_does_not_dispatch_pending_input_to_source(self):
        aid=self.lead_agent['id'];self.start_transfer()
        self.runtime.send(aid,'Run this on the destination','queued-input',delivery='after_tool')
        f.f.Runtime.dispatch(self.runtime)
        self.until(lambda:len(self.pending)==1)
        self.assertFalse(self.runtime.agent(aid)['inFlight'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='queued-input'").fetchone()[0],'pending')
        self.complete_fork()

    def test_failed_turn_continues_once_after_transfer(self):
        aid=self.lead_agent['id'];self.set_agent(aid,status='failed',nativeFailureHold=True,autoWake=True)
        op=self.start_transfer();self.tick();self.until(lambda:len(self.pending)==1);self.complete_fork()
        a=self.runtime.agent(aid);self.assertEqual(a['status'],'queued');self.assertNotIn('nativeFailureHold',a)
        with self.runtime.db() as db:
            rows=db.execute("SELECT * FROM runtime_events WHERE id=?",('account-transfer:'+op['id']+':'+aid,)).fetchall()
            self.assertEqual(len(rows),1);self.assertIn('saved context',rows[0]['text'])
        self.store.request(aid,self.other_key,op['id']);self.tick();self.assertEqual(len(self.pending),1)

    def test_explicit_pause_stays_paused_after_transfer(self):
        aid=self.lead_agent['id'];self.set_agent(aid,status='paused',autoWake=False)
        op=self.start_transfer();self.tick();self.until(lambda:len(self.pending)==1);self.complete_fork()
        self.assertEqual(self.runtime.agent(aid)['status'],'paused')
        with self.runtime.db() as db:
            self.assertIsNone(db.execute("SELECT 1 FROM runtime_events WHERE id=?",('account-transfer:'+op['id']+':'+aid,)).fetchone())

    def test_import_ancestry_keeps_exact_bytes_and_refuses_conflicts(self):
        self.copy_patch.stop()
        home=self.home;other=self.other
        def rollout(tid,base=None,extra=b''):
            path=home/'sessions/2026/09/10'/('rollout-'+tid+'.jsonl');path.parent.mkdir(parents=True,exist_ok=True)
            path.write_bytes((json.dumps({'type':'session_meta','payload':{'id':tid,'history_base':base}})+'\n').encode()+extra)
            return path
        parent_id=str(uuid.uuid4());parent=rollout(parent_id,extra=b'{"type":"event_msg","payload":{}}\n')
        boundary=parent.stat().st_size
        child=rollout(str(uuid.uuid4()),{'thread_id':parent_id,'end_byte_offset':boundary})
        parent.write_bytes(parent.read_bytes()+b'{"unrelated_later_turn":true}\n')
        copied=self.store.copy_history(home,other,child)
        self.assertEqual(copied.read_bytes(),child.read_bytes())
        self.assertEqual((other/'sessions'/'.studio-imports'/parent.name).stat().st_size,boundary)
        self.store.copy_history(home,other,child)
        copied.write_text('changed\n')
        with self.assertRaises(ValueError):self.store.copy_history(home,other,child)
        self.assertEqual(copied.read_text(),'changed\n')


if __name__ == '__main__':
    suite=unittest.TestSuite(TransferContract(name) for name in TransferContract.__dict__ if name.startswith('test_'))
    result=unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())

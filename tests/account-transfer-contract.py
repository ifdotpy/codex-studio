#!/usr/bin/env python3
"""Transfer receipts, native boundaries, team identity and history conservation."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
import importlib.util
import json
import queue
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
        self.set_agent(self.lead_agent['id'], status='completed', inFlight=False, threadId='native-source', turnId=None)
        # The fake fork copies this exact current catalog from its source.
        from codex_native_tools import mark_current
        with self.runtime.lock, self.runtime.db() as db:
            source = self.runtime.agent(self.lead_agent['id'], db)
            mark_current(source, self.runtime.tool_definitions(source))
            self.runtime.put(db, 'agents', source)
        self.target_server = self.runtime.connect(self.other_key)
        self.source_server = self.runtime.connect('default')
        # Native-tools dispatch checks app-server queue state separately from
        # `call`; make the fake expose the same idle field as the real server.
        self.target_server.pending = []
        self.source_server.pending = []
        self.target_server.callbacks = queue.Queue()
        self.source_server.callbacks = queue.Queue()
        self.target_server.clock_replies = queue.Queue()
        self.source_server.clock_replies = queue.Queue()
        self.pending = []
        self.native_calls = []
        original = self.source_server.call
        def call(method, params, timeout=60):
            self.native_calls.append((method, params))
            if method == 'thread/read':
                return {'thread': {'id': params['threadId'], 'status': {'type': 'idle'}, 'path': '/fixture/rollout.jsonl'}}
            if method == 'thread/backgroundTerminals/list': return {'data': []}
            if method == 'thread/queue/list': return {'data': []}
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
        self.drive_lazy_for_tests = True
        self.lazy_test_threads = []

    def tearDown(self):
        for worker in self.lazy_test_threads:
            worker.join(3)
        self.until(lambda:not self.store.running)
        self.copy_patch.stop()
        super().tearDown()

    def set_agent(self, aid, **changes):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(aid, db)
            a.update(changes)
            self.runtime.put(db, 'agents', a)
        return a

    def test_terminal_rejection_releases_transfer_without_replay(self):
        key = 'native-source:rejected-tool'
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.lead_agent['id'], db)
            record = {'id': key, 'agent': actor['id'], 'tool': 'orchestration_task',
                      'stage': 'failed', 'outcome': 'unknown', 'created': time.time(),
                      'finished': 123.0, 'result': {'success': False, 'contentItems': [
                          {'type': 'inputText', 'text': 'Supply a review decision with 1 to 32000 characters'}]}}
            self.runtime.put(db, 'tool_requests', record)
        transfer = self.start_transfer()
        self.tick()
        self.until(lambda: bool(self.pending))
        self.complete_fork()
        with self.runtime.db() as db:
            receipt = self.runtime.tool_request(key, db)
        self.assertEqual(receipt['outcome'], 'not_applied')
        self.assertEqual(receipt['finished'], 123.0)
        self.assertEqual(receipt['result'], record['result'])
        self.assertFalse(self.runtime.begin_tool_request(key))
        self.tick()
        self.assertEqual(self.receipt(transfer['id'])['status'], 'completed')

    def test_new_transfer_replaces_terminal_transfer_metadata(self):
        previous = self.start_transfer()
        self.store.action(previous['id'], 'cancel')
        current = self.start_transfer()
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountTransfer']['id'], current['id'])
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountTransfer']['status'], 'pending')

    def test_unknown_or_committed_operation_keeps_transfer_blocked(self):
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.lead_agent['id'], db)
            record = {'id': 'native-source:unknown', 'agent': actor['id'], 'tool': 'orchestration_task',
                      'stage': 'failed', 'outcome': 'unknown', 'created': time.time(),
                      'result': {'success': False, 'contentItems': [
                          {'type': 'inputText', 'text': 'Native response timed out; outcome unknown'}]}}
            self.runtime.put(db, 'tool_requests', record)
            self.assertEqual(self.store.local_blocker(db, actor), 'Waiting for the tool receipt')
            record['result']['contentItems'][0]['text'] = 'Supply a review decision with 1 to 32000 characters'
            self.runtime.put(db, 'tool_requests', record)
            db.execute('INSERT INTO runtime_operation_receipts VALUES (?,?,?)',
                       (record['id'], 'signature', json.dumps({'already': 'committed'})))
            self.assertEqual(self.store.local_blocker(db, actor), 'Waiting for the tool receipt')
            self.assertEqual(self.runtime.tool_request(record['id'], db)['outcome'], 'unknown')
        self.assertEqual(self.pending, [])

    def request_transfer(self):
        op = self.store.request(self.lead_agent['id'], self.other_key, str(uuid.uuid4()))
        self.current_transfer_id = op['id']
        return op

    def start_transfer(self):
        op = self.request_transfer()
        with self.runtime.db() as db:
            entity = json.loads(db.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (self.lead_agent['id'],)).fetchone()[0])['value']
        self.assertEqual(entity['accountTransferId'], op['id'])
        return op

    def tick(self):
        with self.runtime.lock, self.runtime.db() as db:
            self.store.tick(self.runtime.records(db, 'agents'))

    def until(self, fn):
        deadline = time.monotonic()+3
        while not fn():
            self.wake_one_lazy_for_test()
            if time.monotonic()>deadline:
                try:
                    state = self.receipt(getattr(self, 'current_transfer_id', ''))
                    detail = state['members'].get(self.lead_agent['id'])
                except Exception:
                    detail = None
                self.fail('Transfer fixture deadline: ' + repr(detail) + '; errors=' +
                          repr(getattr(self, 'lazy_test_errors', [])))
            time.sleep(.005)

    def wake_one_lazy_for_test(self):
        """Waiting for a fork receipt models the member's first native start."""
        if not self.drive_lazy_for_tests or any(not future.done() for _, _, future in self.pending):
            return
        if self.store.running:
            return
        if any(worker.is_alive() for worker in self.lazy_test_threads):
            return
        with self.runtime.db() as db:
            agents = self.runtime.records(db, 'agents')
        live_ids = {getattr(w, 'agent_id', None) for w in self.lazy_test_threads if w.is_alive()}
        agent = next((a for a in agents if a.get('lazyAccountTransfer') and
                      a['id'] not in live_ids and a['id'] not in self.store.running), None)
        if not agent:
            return
        worker = threading.Thread(target=self._test_first_start, args=(agent['id'],), daemon=True)
        worker.agent_id = agent['id']
        self.lazy_test_threads.append(worker)
        worker.start()

    def _test_first_start(self, aid):
        try:
            self.store.before_start(self.runtime.agent(aid))
        except Exception as error:
            self.lazy_test_errors = getattr(self, 'lazy_test_errors', [])
            self.lazy_test_errors.append((aid, repr(error)))

    def receipt(self, key):
        with self.runtime.db() as db: return self.store.get(db, key)

    def complete_fork(self, index=0):
        self.pending[index][2].set_result({'thread': {'id': 'target-thread-'+str(index)}, 'model':'gpt-6-astra'})
        for worker in self.lazy_test_threads:
            worker.join(3)
        for worker in list(self.store.workers):
            worker.join(3)

    def test_fork_preserves_only_a_verified_current_tool_catalog(self):
        from codex_native_tools import needs_refresh
        self.set_agent(self.lead_agent['id'], nativeToolCatalog={'threadId': 'native-source', 'digest': 'old'})
        self.start_transfer()
        self.tick()
        self.until(lambda: len(self.pending) == 1)
        self.complete_fork()
        target = self.runtime.agent(self.lead_agent['id'])
        self.assertTrue(needs_refresh(target, self.runtime.tool_definitions(target)))

    def test_account_catalog_reservation_blocks_source_and_destination(self):
        op = self.start_transfer()
        for account in ('default', self.other_key):
            self.runtime._native_tools_refreshing = {account}
            self.tick()
            self.assertEqual(self.pending, [])
            self.wake_one_lazy_for_test()
            self.until(lambda: self.receipt(op['id'])['members'][self.lead_agent['id']]['phase'] == 'blocked')
            member = self.receipt(op['id'])['members'][self.lead_agent['id']]
            self.assertEqual(member['phase'], 'blocked')
            self.assertIn('account tool catalog update', member['error'])
            self.runtime._native_tools_refreshing.clear()
            self.store.action(op['id'], 'retry')
        self.tick()
        self.until(lambda: len(self.pending) == 1)
        self.complete_fork()
        from codex_native_tools import needs_refresh
        target = self.runtime.agent(self.lead_agent['id'])
        self.assertFalse(needs_refresh(target, self.runtime.tool_definitions(target)))

    def test_destination_catalog_reservation_is_rechecked_before_submit(self):
        op = self.start_transfer()
        def reserve_destination(*_):
            with self.runtime.lock:
                self.runtime._native_tools_refreshing = {self.other_key}
            return Path('/fixture/import.jsonl')
        with patch.object(self.store, 'copy_history', side_effect=reserve_destination):
            self.tick()
            self.wake_one_lazy_for_test()
            self.until(lambda: self.receipt(op['id'])['members'][self.lead_agent['id']]['phase'] == 'blocked')
        self.assertEqual(self.pending, [])
        member = self.receipt(op['id'])['members'][self.lead_agent['id']]
        self.assertEqual(member['phase'], 'blocked')
        self.assertIn('account tool catalog update', member['error'])
        self.store.action(op['id'], 'retry')
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime._native_tools_refreshing.clear()
            saved = self.store.get(db, op['id'])
            saved['members'][self.lead_agent['id']]['nextCheck'] = 0
            self.store.save(db, saved)
        self.tick()
        self.until(lambda: len(self.pending) == 1)
        self.complete_fork()

    def test_deleted_unsent_member_releases_only_its_transfer_pointer(self):
        op = self.start_transfer()
        aid = self.lead_agent['id']
        before = self.set_agent(aid, deletedAt=time.time(), status='paused', autoWake=False)
        self.tick()
        after = self.runtime.agent(aid)
        self.assertNotIn('accountTransferId', after)
        for field in ('accountKey', 'threadId', 'epoch', 'autoWake', 'status', 'deletedAt'):
            self.assertEqual(after[field], before[field])
        self.assertEqual(self.pending, [])
        self.assertEqual(self.receipt(op['id'])['status'], 'completed')
        self.assertEqual(after['accountTransfer']['id'], op['id'])
        self.assertEqual(after['accountTransfer']['status'], 'completed')

    def test_deleted_member_preserves_unknown_transfer_receipt(self):
        op = self.start_transfer()
        aid = self.lead_agent['id']
        self.set_agent(aid, deletedAt=time.time(), status='paused', autoWake=False)
        with self.runtime.lock, self.runtime.db() as db:
            saved = self.store.get(db, op['id'])
            saved['members'][aid].update(phase='unknown', nativeMethod='thread/fork', submittedAt=time.time())
            self.store.save(db, saved)
        self.tick()
        self.assertEqual(self.runtime.agent(aid)['accountTransferId'], op['id'])
        self.assertEqual(self.receipt(op['id'])['members'][aid]['phase'], 'unknown')
        self.assertEqual(self.receipt(op['id'])['status'], 'pending')
        self.assertEqual(self.pending, [])

    def test_native_queue_blocks_transfer_until_empty(self):
        original = self.source_server.call
        queued = [{'id': 'native-accepted-input'}]
        def call(method, params, timeout=60):
            if method == 'thread/read':
                return {'thread': {'id': params['threadId'], 'status': {'type': 'notLoaded'},
                                   'path': '/fixture/rollout.jsonl'}}
            if method == 'thread/queue/list':
                return {'data': list(queued)}
            return original(method, params, timeout)
        self.source_server.call = call
        op = self.start_transfer()
        self.tick()
        self.wake_one_lazy_for_test()
        self.until(lambda: self.receipt(op['id'])['members'][self.lead_agent['id']]['phase'] == 'blocked')
        self.assertEqual(self.pending, [])
        self.assertIn('queued input', self.receipt(op['id'])['members'][self.lead_agent['id']]['error'])
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountKey'], self.other_key)
        self.assertEqual(queued, [{'id': 'native-accepted-input'}])
        queued.clear()
        self.store.action(op['id'], 'retry')
        self.until(lambda: bool(self.pending))
        self.complete_fork()
        self.tick()
        self.assertEqual(self.receipt(op['id'])['status'], 'completed')
        self.assertEqual(len(self.pending), 1)

    def test_native_input_arriving_during_copy_blocks_fork(self):
        original = self.source_server.call
        reads = []
        def call(method, params, timeout=60):
            if method == 'thread/queue/list':
                reads.append(params)
                return {'data': [] if len(reads) == 1 else [{'id': 'late-native-input'}]}
            return original(method, params, timeout)
        self.source_server.call = call
        op = self.start_transfer()
        self.tick()
        self.wake_one_lazy_for_test()
        self.until(lambda: self.receipt(op['id'])['members'][self.lead_agent['id']]['phase'] == 'blocked')
        self.assertEqual(len(reads), 2)
        self.assertEqual(self.pending, [])
        self.assertIn('queued input', self.receipt(op['id'])['members'][self.lead_agent['id']]['error'])

    def test_native_queue_read_failure_does_not_transfer(self):
        original = self.source_server.call
        def call(method, params, timeout=60):
            if method == 'thread/queue/list':
                raise TimeoutError('Native queue response unavailable')
            return original(method, params, timeout)
        self.source_server.call = call
        op = self.start_transfer()
        self.tick()
        self.wake_one_lazy_for_test()
        self.until(lambda: self.receipt(op['id'])['members'][self.lead_agent['id']]['phase'] == 'blocked')
        self.assertEqual(self.pending, [])
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountKey'], self.other_key)
        self.assertIn('Native queue response unavailable', self.receipt(op['id'])['members'][self.lead_agent['id']]['error'])
        self.assertNotEqual(self.receipt(op['id'])['status'], 'completed')

    def test_destinations_have_independent_native_receipt_slots(self):
        same = self.lead()
        self.set_agent(same['id'], status='completed', threadId=None)
        same_op = self.store.request(same['id'], self.other_key, str(uuid.uuid4()))
        opposite = self.runtime.create({'cwd': str(self.root), 'prompt': '', 'account_key': self.other_key}, draft=True)
        self.set_agent(opposite['id'], status='completed', threadId=None)
        other_op = self.store.request(opposite['id'], 'default', str(uuid.uuid4()))
        original = self.source_server.submit
        independent = []
        def submit(method, params):
            if method == 'thread/start':
                future = concurrent.futures.Future()
                independent.append(future)
                return future
            return original(method, params)
        self.source_server.submit = submit
        first = self.start_transfer()
        # Reserve the first destination before scheduling both other leads.
        with self.runtime.db() as db:
            self.store.tick([self.runtime.agent(self.lead_agent['id'], db)])
        self.until(lambda: len(self.pending) == 1)
        # A separate destination can start while this target has an unresolved
        # receipt; exercise the other member at its lazy first-start boundary.
        worker = threading.Thread(target=self.store.before_start,
                                  args=(self.runtime.agent(opposite['id']),), daemon=True)
        worker.start()
        self.until(lambda: bool(independent))
        self.assertEqual(len(self.pending), 1)
        self.assertEqual(self.receipt(same_op['id'])['members'][same['id']]['phase'], 'lazy')
        independent[0].set_result({'thread': {'id': 'independent-target'}})
        worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(self.runtime.agent(opposite['id'])['accountKey'], 'default')
        self.assertEqual(self.runtime.agent(opposite['id'])['threadId'], 'independent-target')
        self.complete_fork()
        self.wake_one_lazy_for_test()
        self.until(lambda: len(self.pending) == 2)
        self.complete_fork(1)

    def test_ready_receipt_commits_without_delay_or_native_preparation(self):
        op = self.start_transfer()
        self.tick()
        self.until(lambda: len(self.pending) == 1)
        aid = self.lead_agent['id']
        self.set_agent(aid, workspaceOperation={'id': 'temporary'})
        self.complete_fork()
        self.assertEqual(self.receipt(op['id'])['members'][aid]['phase'], 'blocked')
        self.assertIn('workspace operation', self.receipt(op['id'])['members'][aid]['error'])
        self.set_agent(aid, workspaceOperation=None)
        self.store.action(op['id'], 'retry')
        with patch.object(self.runtime, 'catalog', side_effect=AssertionError('No catalog reread')):
            self.wake_one_lazy_for_test()
            self.until(lambda: self.receipt(op['id'])['members'][aid]['phase'] == 'completed')
        self.assertEqual(self.runtime.agent(aid)['accountKey'], self.other_key)
        self.assertEqual(len(self.pending), 1)

    def test_orphan_exact_pending_pointer_recovers_terminal_summary(self):
        old = self.start_transfer()
        self.store.action(old['id'], 'cancel')
        terminal = self.runtime.agent(self.lead_agent['id'])['accountTransfer']
        current = self.start_transfer()
        self.set_agent(self.lead_agent['id'], accountTransfer=terminal)
        self.tick()
        self.until(lambda: len(self.pending) == 1)
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountTransfer']['id'], current['id'])
        self.complete_fork()

    def test_unreferenced_waiting_transfer_settles_without_replacing_current_summary(self):
        old = self.start_transfer()
        self.store.action(old['id'], 'cancel')
        current = self.start_transfer()
        self.tick()
        self.until(lambda: bool(self.pending))
        self.complete_fork()
        summary = self.runtime.agent(self.lead_agent['id'])['accountTransfer']
        self.assertEqual(summary['id'], current['id'])
        self.assertEqual(summary['status'], 'completed')

        with self.runtime.lock, self.runtime.db() as db:
            # A late save from the older operation must not replace the newer
            # terminal summary. It also recreates the stale pending receipt.
            self.store.save(db, old)

        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountTransfer']['id'], current['id'])
        self.tick()
        self.assertEqual(self.receipt(old['id'])['status'], 'cancelled')
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountTransfer']['id'], current['id'])

    def test_conflicting_pending_pointer_is_not_submitted(self):
        op = self.start_transfer()
        self.set_agent(self.lead_agent['id'], accountTransfer={'id': 'other-pending', 'status': 'pending'})
        self.tick()
        self.assertEqual(self.pending, [])
        self.assertEqual(self.receipt(op['id'])['members'][self.lead_agent['id']]['phase'], 'lazy')
        self.set_agent(self.lead_agent['id'], accountTransfer={'id': op['id'], 'status': 'pending'})

    def test_restart_preserves_terminal_receipt_timestamp(self):
        from codex_account_transfer import AccountTransfers
        op = self.start_transfer()
        self.store.action(op['id'], 'cancel')
        before = self.receipt(op['id'])
        summary = self.runtime.agent(self.lead_agent['id'])['accountTransfer']
        AccountTransfers(self.runtime)
        self.assertEqual(self.receipt(op['id']), before)
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountTransfer'], summary)

    def test_transfer_keeps_identity_history_queue_and_settings(self):
        aid = self.lead_agent['id']
        self.set_agent(aid, compactions=17, panelVersion=3)
        op = self.start_transfer()
        self.runtime.send(aid, 'Follow up once', 'message-1', delivery='after_tool')
        self.tick()
        self.until(lambda:len(self.pending)==1)
        self.assertEqual(self.runtime.agent(aid)['accountKey'], self.other_key)
        self.assertEqual(self.pending[0][0], 'thread/fork')
        self.assertTrue(self.pending[0][1]['deferGoalContinuation'])
        self.complete_fork()
        self.assertEqual(self.receipt(op['id'])['status'], 'completed')
        self.assertEqual(self.runtime.agent(aid)['accountTransfer']['status'], 'completed')
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
        self.set_agent(aid,status='completed',inFlight=False)
        self.tick();self.until(lambda:len(self.pending)==1)
        for _ in range(5):self.tick()
        self.assertEqual(len(self.pending),1)
        self.assertEqual(self.store.request(aid,self.other_key,op['id'])['id'],op['id'])
        with self.assertRaises(ValueError):self.store.request(aid,'default',op['id'])
        self.complete_fork()

    def test_cancel_before_first_start_restores_source_without_native_calls(self):
        op=self.start_transfer()
        self.store.action(op['id'],'cancel')
        a=self.runtime.agent(self.lead_agent['id'])
        self.assertEqual(a['accountKey'],'default')
        self.assertNotIn('accountTransferId',a)
        self.assertNotIn('lazyAccountTransfer',a)
        self.assertEqual(self.native_calls,[])
        self.assertEqual(self.pending,[])

    def test_cancel_after_lazy_fork_submission_waits_for_exact_receipt(self):
        op=self.start_transfer();self.tick();self.until(lambda:len(self.pending)==1)
        with self.assertRaisesRegex(ValueError, 'history move has started'):
            self.store.action(op['id'],'cancel')
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountKey'],self.other_key)
        self.complete_fork()
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['threadId'],'target-thread-0')

    def test_epoch_change_never_adopts_fork(self):
        op=self.start_transfer();self.tick();self.until(lambda:len(self.pending)==1)
        a=self.runtime.agent(self.lead_agent['id']);self.set_agent(a['id'],epoch=a['epoch']+1)
        self.complete_fork()
        self.assertEqual(self.runtime.agent(a['id'])['accountKey'],self.other_key)
        self.assertEqual(self.runtime.agent(a['id'])['threadId'],'native-source')
        self.assertEqual(self.receipt(op['id'])['members'][a['id']]['phase'],'blocked')

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

    def test_previous_backend_uncertainty_is_preserved_without_replay(self):
        aid = self.lead_agent['id']
        with self.runtime.db() as db:
            a = self.runtime.agent(aid, db)
            self.runtime.enqueue(db, a, 'user', 'Do not repeat', 'historical-unknown')
            db.execute("UPDATE runtime_events SET status='uncertain',created=? WHERE id='historical-unknown'",
                       (self.runtime.started_at - 60,))
            self.runtime.put(db, 'tool_requests', {'id': 'old-tool', 'agent': aid,
                'stage': 'interrupted', 'outcome': 'unknown', 'created': self.runtime.started_at - 60})
        op = self.start_transfer()
        self.tick(); self.until(lambda: len(self.pending) == 1); self.complete_fork()
        self.until(lambda: self.receipt(op['id'])['members'][aid]['phase'] == 'completed')
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='historical-unknown'").fetchone()[0], 'uncertain')
            self.assertEqual(json.loads(db.execute("SELECT record FROM runtime_tool_requests WHERE id='old-tool'").fetchone()[0])['outcome'], 'unknown')
        self.assertFalse(any(method in {'turn/start', 'turn/steer'} for method, params in self.native_calls))

    def test_active_tool_from_before_boot_still_blocks(self):
        aid = self.lead_agent['id']
        with self.runtime.db() as db:
            self.runtime.put(db, 'tool_requests', {'id': 'active-tool', 'agent': aid,
                'stage': 'running', 'outcome': 'unknown', 'created': self.runtime.started_at - 60})
        self.start_transfer(); self.tick()
        self.assertEqual(self.pending, [])

    def test_codex_subagents_move_and_claude_subagent_stays_with_reason(self):
        idle = self.runtime.create({'name': 'Idle Codex', 'prompt': 'Task'}, parent=self.lead_agent['id'], defer=True)
        self.set_agent(idle['id'], status='waiting', autoWake=True, inFlight=False, threadId='native-idle', turnId=None)
        queued = self.runtime.create({'name': 'Queued Codex', 'prompt': 'Task'}, parent=self.lead_agent['id'], defer=True)
        self.set_agent(queued['id'], status='queued', autoWake=True, inFlight=False, threadId='native-queued', turnId=None)
        self.runtime.send(queued['id'], 'Deliver this pending event once', 'queued-idle-input')
        running = self.runtime.create({'name': 'Running Codex', 'prompt': 'Task'}, parent=self.lead_agent['id'], defer=True)
        self.set_agent(running['id'], status='running', autoWake=True, inFlight=True, threadId='native-running', turnId='turn-running')
        self.runtime.send(running['id'], 'Deliver this queued input once', 'queued-running-input')
        with self.runtime.accounts.lock:
            self.runtime.accounts.data['accounts']['claude-fixture'] = {
                'id': 'claude-fixture', 'provider': 'claude', 'home': str(self.root / 'claude-home'),
                'label': 'Claude fixture', 'status': 'ready'}
        claude = self.runtime.create({'name': 'Claude worker', 'prompt': 'Task'}, parent=self.lead_agent['id'], defer=True)
        self.set_agent(claude['id'], provider='claude', accountKey='claude-fixture', status='completed', inFlight=False)
        op = self.start_transfer()
        self.assertEqual(set(self.receipt(op['id'])['members']),
                         {self.lead_agent['id'], idle['id'], queued['id'], running['id'], claude['id']})
        self.assertEqual(self.receipt(op['id'])['members'][claude['id']]['phase'], 'left')
        self.assertIn('destination account uses codex', self.receipt(op['id'])['members'][claude['id']]['reason'])
        self.tick()
        self.until(lambda: any(method == 'turn/interrupt' for method, _ in self.native_calls))
        self.set_agent(running['id'], status='interrupted', inFlight=False, turnId=None)
        self.until(lambda: len(self.pending) == 1)
        self.complete_fork()
        self.until(lambda: self.receipt(op['id'])['members'][self.lead_agent['id']]['phase'] == 'completed')
        # The interrupted active member may fork alongside the first lazy
        # wakeup. Settle each exact receipt and let queued members wake next.
        for index in range(1, 4):
            self.tick()
            self.wake_one_lazy_for_test()
            self.until(lambda index=index: len(self.pending) > index)
            self.complete_fork(index)
        self.until(lambda: self.receipt(op['id'])['members'][idle['id']]['phase'] == 'completed')
        self.tick()
        self.until(lambda: self.receipt(op['id'])['status'] == 'completed')
        self.assertEqual(self.receipt(op['id'])['status'], 'completed')
        self.assertEqual(self.runtime.agent(idle['id'])['accountKey'], self.other_key)
        self.assertEqual(self.runtime.agent(running['id'])['accountKey'], self.other_key)
        self.assertEqual(self.runtime.agent(claude['id'])['accountKey'], 'claude-fixture')
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['workerDefaults']['accountKey'], self.other_key)
        self.assertTrue(self.receipt(op['id'])['members'][running['id']]['interruptReason'].startswith('Moved to account '))
        self.assertEqual(self.runtime.agent(queued['id'])['accountKey'], self.other_key)
        self.assertEqual(len([m for m in self.pending if m[0] in {'thread/fork', 'thread/start'}]), 4)
        with self.runtime.db() as db:
            for aid in (running['id'],):
                rows = db.execute("SELECT id,text FROM runtime_events WHERE id=?",
                    ('account-transfer:' + op['id'] + ':' + aid,)).fetchall()
                self.assertEqual(len(rows), 1)
                self.assertIn('Continue the existing task', rows[0]['text'])
            running_queued = db.execute("SELECT id,status FROM runtime_events WHERE id='queued-running-input'").fetchall()
            self.assertEqual([(row['id'], row['status']) for row in running_queued], [('queued-running-input', 'pending')])
            self.assertIsNone(db.execute("SELECT 1 FROM runtime_events WHERE id=?",
                ('account-transfer:' + op['id'] + ':' + idle['id'],)).fetchone())
            self.assertIsNone(db.execute("SELECT 1 FROM runtime_events WHERE id=?",
                ('account-transfer:' + op['id'] + ':' + queued['id'],)).fetchone())
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='queued-idle-input'").fetchone()[0], 'pending')
        moved_thread = self.runtime.agent(running['id'])['threadId']
        f.f.Runtime.dispatch(self.runtime)
        self.until(lambda: self.runtime.delivery_receipt('queued-running-input')['status'] == 'delivered')
        starts = [params for method, params in self.target_server.calls
                  if method == 'turn/start' and params.get('threadId') == moved_thread]
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'queued-running-input')
        queued_thread = self.runtime.agent(queued['id'])['threadId']
        self.until(lambda: self.runtime.delivery_receipt('queued-idle-input')['status'] == 'delivered')
        queued_starts = [params for method, params in self.target_server.calls
                         if method == 'turn/start' and params.get('threadId') == queued_thread]
        self.assertEqual(len(queued_starts), 1)
        self.assertEqual(queued_starts[0]['clientUserMessageId'], 'queued-idle-input')
        idle_thread = self.runtime.agent(idle['id'])['threadId']
        self.assertFalse(any(method == 'turn/start' and params.get('threadId') == idle_thread
                             for method, params in self.target_server.calls))
        next_worker = self.runtime.create({'name': 'Next worker', 'prompt': 'Next task'},
                                          parent=self.lead_agent['id'], defer=True)
        self.assertEqual(next_worker['accountKey'], self.other_key)

    def test_subagents_scope_keeps_claude_lead_and_moves_codex_descendants(self):
        with self.runtime.accounts.lock:
            self.runtime.accounts.data['accounts']['claude-fixture'] = {
                'id': 'claude-fixture', 'provider': 'claude', 'home': str(self.root / 'claude-home'),
                'label': 'Claude fixture', 'status': 'ready'}
        self.set_agent(self.lead_agent['id'], provider='claude', accountKey='claude-fixture',
                        workerDefaults={**self.runtime.agent(self.lead_agent['id'])['workerDefaults'],
                                        'accountKey': 'default'})
        codex = self.runtime.create({'name': 'Codex worker', 'prompt': 'Task'},
                                    parent=self.lead_agent['id'], defer=True)
        self.set_agent(codex['id'], status='completed', inFlight=False, threadId='native-codex', turnId=None)
        claude = self.runtime.create({'name': 'Claude worker', 'prompt': 'Task'},
                                     parent=self.lead_agent['id'], defer=True)
        self.set_agent(claude['id'], provider='claude', accountKey='claude-fixture', status='completed',
                       inFlight=False, threadId='native-claude', turnId=None)
        request_id = str(uuid.uuid4())
        op = self.store.request(self.lead_agent['id'], self.other_key, request_id, scope='subagents')
        self.assertEqual(op['scope'], 'subagents')
        self.assertEqual(set(op['members']), {codex['id'], claude['id']})
        self.assertEqual(op['members'][claude['id']]['phase'], 'left')
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountKey'], 'claude-fixture')
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['workerDefaults']['accountKey'], self.other_key)
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountTransfer']['scope'], 'subagents')
        replay = self.store.request(self.lead_agent['id'], self.other_key, request_id, scope='subagents')
        self.assertEqual(replay['id'], op['id'])
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.store.request(self.lead_agent['id'], 'default', request_id, scope='subagents')
        self.tick()
        self.until(lambda: len(self.pending) == 1)
        self.assertEqual(self.pending[0][1].get('threadId'), 'native-codex')
        self.complete_fork()
        self.until(lambda: self.receipt(op['id'])['members'][codex['id']]['phase'] == 'completed')
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountKey'], 'claude-fixture')
        self.assertEqual(self.runtime.agent(codex['id'])['accountKey'], self.other_key)
        self.assertEqual(self.runtime.agent(claude['id'])['accountKey'], 'claude-fixture')
        self.assertEqual(len(self.pending), 1, 'the Claude lead is not forked or started')
        next_worker = self.runtime.create({'name': 'Next worker', 'prompt': 'Next task'},
                                          parent=self.lead_agent['id'], defer=True)
        self.assertEqual(next_worker['accountKey'], self.other_key)

    def test_team_transfer_after_a_finished_lead_transfer_moves_descendants(self):
        # The lead already moved in an earlier transfer; its descendants did not.
        first = self.start_transfer()
        self.tick()
        self.until(lambda: len(self.pending) == 1)
        self.complete_fork()
        self.until(lambda: self.receipt(first['id'])['status'] == 'completed')
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountKey'], self.other_key)
        worker = self.runtime.create({'name': 'Old worker', 'prompt': 'Task'},
                                     parent=self.lead_agent['id'], defer=True)
        self.set_agent(worker['id'], accountKey='default', status='completed', inFlight=False,
                       threadId='native-worker', turnId=None)
        op = self.store.request(self.lead_agent['id'], self.other_key, str(uuid.uuid4()))
        self.assertEqual(op['members'][self.lead_agent['id']]['phase'], 'completed')
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountTransfer']['id'], op['id'])
        self.tick()
        self.until(lambda: len(self.pending) == 2)
        self.assertEqual(self.pending[1][1].get('threadId'), 'native-worker')
        self.complete_fork(1)
        self.until(lambda: self.receipt(op['id'])['status'] == 'completed')
        self.assertEqual(self.runtime.agent(worker['id'])['accountKey'], self.other_key)

    def test_duplicate_commit_of_a_moved_agent_is_completed_not_unknown(self):
        op = self.start_transfer()
        self.tick()
        self.until(lambda: len(self.pending) == 1)
        self.complete_fork()
        self.until(lambda: self.receipt(op['id'])['status'] == 'completed')
        lead = self.lead_agent['id']
        with self.runtime.lock, self.runtime.db() as db:
            saved = self.store.get(db, op['id'])
            # A second commit of the same receipt finds the agent already moved.
            saved['members'][lead]['phase'] = 'ready'
            saved['status'] = 'pending'
            self.store.commit(db, saved, self.runtime.agent(lead, db))
            self.assertEqual(saved['members'][lead]['phase'], 'completed')
            # A record saved as unknown by the old code settles on the next tick.
            saved['members'][lead].update(phase='unknown', error='Agent state changed during transfer.')
            saved['status'] = 'pending'
            self.store.save(db, saved)
        self.tick()
        self.until(lambda: self.receipt(op['id'])['status'] == 'completed')
        self.assertEqual(self.receipt(op['id'])['members'][lead]['phase'], 'completed')
        self.assertEqual(len(self.pending), 1, 'no second fork')

    def test_restart_during_interrupt_does_not_repeat_or_lose_queued_event(self):
        aid = self.lead_agent['id']
        self.set_agent(aid, status='running', autoWake=True, inFlight=True, threadId='native-source', turnId='turn-running')
        op = self.start_transfer()
        self.tick()
        self.until(lambda: any(method == 'turn/interrupt' for method, _ in self.native_calls))
        self.until(lambda: not self.store.running)
        before = len([method for method, _ in self.native_calls if method == 'turn/interrupt'])
        from codex_account_transfer import AccountTransfers
        restarted = AccountTransfers(self.runtime)
        with self.runtime.db() as db:
            restarted.tick(self.runtime.records(db, 'agents'))
        self.assertEqual(len([method for method, _ in self.native_calls if method == 'turn/interrupt']), before)
        self.assertEqual(self.receipt(op['id'])['members'][aid]['phase'], 'blocked')
        self.assertIn('No request was repeated', self.receipt(op['id'])['members'][aid]['error'])
        self.set_agent(aid, status='interrupted', inFlight=False, turnId=None)
        restarted.action(op['id'], 'retry')
        self.runtime._account_transfers = restarted
        self.store = restarted
        with patch.object(restarted, 'copy_history', return_value=Path('/fixture/import.jsonl')):
            self.tick()
            self.until(lambda: bool(self.pending))
        self.complete_fork()

    def test_cancel_after_confirmed_interrupt_resumes_source_once(self):
        aid = self.lead_agent['id']
        self.set_agent(aid, status='running', autoWake=True, inFlight=True,
                       threadId='native-source', turnId='turn-cancel')
        op = self.start_transfer()
        self.tick()
        self.until(lambda: any(method == 'turn/interrupt' for method, _ in self.native_calls))
        self.until(lambda: not self.store.running)
        member = self.receipt(op['id'])['members'][aid]
        self.assertEqual(member['interruptOutcome'], 'acknowledged')
        cancelled = self.store.action(op['id'], 'cancel')
        self.assertEqual(cancelled['status'], 'cancelled')
        self.assertEqual(self.runtime.agent(aid)['accountKey'], 'default')
        with self.runtime.db() as db:
            rows = db.execute("SELECT id,text FROM runtime_events WHERE id=?",
                ('account-transfer-cancel:' + op['id'] + ':' + aid,)).fetchall()
            self.assertEqual(len(rows), 1)
            self.assertIn('Continue the existing task on this account', rows[0]['text'])
        self.assertEqual(self.pending, [])

    def test_preparation_lock_retries_without_a_model_turn(self):
        from codex_native_errors import NativeRpcError
        op = self.start_transfer(); self.tick(); self.until(lambda: len(self.pending) == 1)
        self.pending[0][2].set_exception(NativeRpcError({'code': -32603,
            'message': 'failed to prepare paginated fork: database is locked'}))
        aid = self.lead_agent['id']
        self.until(lambda: self.receipt(op['id'])['members'][aid]['phase'] == 'lazy')
        m = self.receipt(op['id'])['members'][aid]
        self.assertEqual(m['preparationRejections'][0]['outcome'], 'fork_not_created')
        self.assertGreater(m['nextCheck'], time.time())
        self.tick(); self.assertEqual(len(self.pending), 1)

    def test_arbitrary_database_error_is_not_retried(self):
        from codex_native_errors import NativeRpcError
        op = self.start_transfer(); self.tick(); self.until(lambda: len(self.pending) == 1)
        self.pending[0][2].set_exception(NativeRpcError({'code': -32603, 'message': 'database is locked'}))
        self.until(lambda: self.receipt(op['id'])['members'][self.lead_agent['id']]['phase'] == 'unknown')
        self.tick(); self.assertEqual(len(self.pending), 1)

    def test_preparation_retries_are_bounded_and_keep_all_rejections(self):
        from codex_native_errors import NativeRpcError
        op = self.start_transfer()
        aid = self.lead_agent['id']
        error = NativeRpcError({'code': -32603, 'message': 'failed to prepare paginated fork: database is locked'})
        for attempt in range(4):
            with self.runtime.db() as db:
                current = self.store.get(db, op['id'])
                current['members'][aid].update(phase='submitted', submittedAt=attempt)
                self.store.save(db, current)
            self.assertTrue(self.store.retry_preparation(op['id'], aid, error))
        member = self.receipt(op['id'])['members'][aid]
        self.assertEqual(member['phase'], 'blocked')
        self.assertEqual(len(member['preparationRejections']), 4)

    def test_workers_created_during_transfer_are_excluded(self):
        aid=self.lead_agent['id'];op=self.start_transfer()
        worker=self.runtime.create({'name':'Worker','prompt':'Task'},parent=aid,defer=True)
        self.set_agent(worker['id'], status='waiting', inFlight=False, threadId=None)
        self.tick();self.until(lambda:len(self.pending)==1)
        self.assertNotIn(worker['id'],self.receipt(op['id'])['members'])
        self.assertNotIn('accountTransferId', self.runtime.agent(worker['id']))
        self.complete_fork(0);self.until(lambda:not self.store.futures)
        self.tick()
        self.assertEqual(self.runtime.agent(worker['id'])['parentId'],aid)
        # New workers inherit the instant target binding from their parent.
        self.assertEqual(self.runtime.agent(worker['id'])['accountKey'],self.other_key)
        self.assertEqual(self.receipt(op['id'])['status'],'completed')

    def test_new_request_extends_an_existing_team_transfer_exactly_once(self):
        op = self.start_transfer()
        worker = self.runtime.create({'name': 'Late worker', 'prompt': 'Task'},
                                     parent=self.lead_agent['id'], defer=True)
        self.set_agent(worker['id'], status='waiting', inFlight=False, threadId=None)
        request_id = str(uuid.uuid4())
        extended = self.store.request(self.lead_agent['id'], self.other_key, request_id)
        self.assertEqual(extended['id'], op['id'])
        self.assertIn(worker['id'], self.receipt(op['id'])['members'])
        self.assertEqual(self.store.request(self.lead_agent['id'], self.other_key, request_id), extended)
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.store.request(self.lead_agent['id'], 'default', request_id)
        self.tick(); self.until(lambda: len(self.pending) == 1)
        self.complete_fork()
        self.until(lambda: self.runtime.agent(self.lead_agent['id'])['accountKey'] == self.other_key)
        # The late worker inherited the already committed target account and
        # has no source native session to fork.
        self.tick()
        self.assertEqual(len(self.pending), 1)
        self.tick()
        self.assertEqual(self.runtime.agent(worker['id'])['accountKey'], self.other_key)
        self.assertIsNone(self.runtime.agent(worker['id'])['threadId'])
        self.assertEqual(self.receipt(op['id'])['status'], 'completed')

    def test_deployment_removes_only_unsent_subagent_member(self):
        op = self.start_transfer()
        worker = self.runtime.create({'name': 'Legacy worker', 'prompt': 'Task'},
                                     parent=self.lead_agent['id'], defer=True)
        self.set_agent(worker['id'], accountTransferId=op['id'], status='waiting',
                       inFlight=False, threadId=None)
        with self.runtime.lock, self.runtime.db() as db:
            saved = self.store.get(db, op['id'])
            saved['members'][worker['id']] = {
                'phase': 'waiting', 'sourceAccountKey': 'default', 'sourceThreadId': None,
            }
            self.store.save(db, saved)
        self.set_agent(self.lead_agent['id'], accountTransfer={
            'id': 'newer-operation', 'status': 'cancelled',
        })
        from fixtures.current_cleanup_state import convert_unsent_transfer_members
        with self.runtime.lock, self.runtime.db() as db:
            self.assertEqual(convert_unsent_transfer_members(self.runtime, db), 1)
            self.assertEqual(convert_unsent_transfer_members(self.runtime, db), 0)
        self.tick()
        receipt = self.receipt(op['id'])
        self.assertNotIn(worker['id'], receipt['members'])
        self.assertNotIn('accountTransferId', self.runtime.agent(worker['id']))
        self.until(lambda: len(self.pending) == 1)
        self.complete_fork()

    def test_restart_retains_unknown_receipt_and_never_replays_it(self):
        op=self.start_transfer();self.tick();self.until(lambda:len(self.pending)==1)
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
        aid=self.lead_agent['id']
        self.runtime.conversation_settings(aid, {'next_turn': True, 'request_id': 'destination-choice',
                                                 'model': 'gpt-5.6-sol', 'effort': 'low'})
        self.start_transfer()
        self.runtime.send(aid,'Run this on the destination','queued-input',delivery='after_tool')
        f.f.Runtime.dispatch(self.runtime)
        self.until(lambda:len(self.pending)==1)
        self.assertTrue(self.runtime.agent(aid)['inFlight'])
        with self.runtime.db() as db:
            status=db.execute("SELECT status FROM runtime_events WHERE id='queued-input'").fetchone()[0]
            self.assertIn(status, {'pending','reserved'})
        self.assertFalse(any(method=='turn/start' for method, _ in self.target_server.calls))
        self.complete_fork()
        self.until(lambda: self.runtime.delivery_receipt('queued-input')['status'] == 'delivered')
        starts = [params for method, params in self.target_server.calls if method == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['threadId'], 'target-thread-0')
        self.assertEqual((starts[0]['model'], starts[0]['effort']), ('gpt-5.6-sol', 'low'))
        self.assertEqual(self.runtime.delivery_receipt('queued-input')['status'], 'delivered')

    def test_first_native_start_of_failed_turn_continues_once_after_transfer(self):
        aid=self.lead_agent['id'];self.set_agent(aid,status='failed',nativeFailureHold=True,autoWake=True)
        # This models an explicit native start, not a background history copy.
        op=self.start_transfer();self.until(lambda:len(self.pending)==1);self.complete_fork()
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

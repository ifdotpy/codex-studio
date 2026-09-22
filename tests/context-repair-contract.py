#!/usr/bin/env python3
"""Context repair provenance, exact identity, and lost-receipt contracts."""
import concurrent.futures
import copy
import importlib.util
import json
from pathlib import Path
import time
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location('workspace', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_context_repair as repair


class Server:
    def __init__(self, path, tid):
        self.path, self.tid = path, tid
        self.calls, self.futures = [], []
        self.hold = False
        self.status = 'idle'
        self.items = []
        self.queue = []
        self.terminals = []
        self.turn_status = 'completed'
        self.no_turns = False
        self.before_read = None

    def call(self, method, params, timeout=10):
        self.calls.append((method, copy.deepcopy(params)))
        if method == 'thread/read':
            if self.before_read:
                self.before_read()
            return {'thread': {'id': self.tid, 'path': str(self.path), 'status': {'type': self.status} if isinstance(self.status, str) else self.status}}
        if method == 'thread/turns/list':
            return {'data': [] if self.no_turns else [{'id': 'turn', 'status': self.turn_status}]}
        if method == 'thread/items/list':
            return {'data': self.items}
        if method == 'thread/backgroundTerminals/list' and self.status == 'notLoaded':
            raise RuntimeError(json.dumps({'code': -32600, 'message': 'thread not found: ' + self.tid}))
        return {'data': self.queue if method == 'thread/queue/list' else self.terminals if method == 'thread/backgroundTerminals/list' else []}

    def submit(self, method, params):
        self.calls.append((method, copy.deepcopy(params)))
        future = concurrent.futures.Future()
        self.futures.append(future)
        if method != 'thread/fork' or not self.hold:
            future.set_result({'thread': {'id': str(uuid.uuid4())}} if method == 'thread/fork' else {})
        return len(self.futures), method, future

    def on_result(self, ticket, callback):
        ticket[2].add_done_callback(callback)

    def after_events(self, callback):
        callback()


class ContextRepair(unittest.TestCase):
    def agent_update(self, agent, **changes):
        updated = f.WorkspaceContract.agent_update(self, agent, **changes)
        if 'threadId' in changes:
            from codex_native_tools import digest, mark_current
            tools = self.runtime.tool_definitions(updated)
            if (agent.get('nativeToolCatalog') or {}).get('digest') == digest(tools):
                # This fixture changes the native identity with the same tools.
                mark_current(updated, tools)
                with self.runtime.lock, self.runtime.db() as db:
                    self.runtime.put(db, 'agents', updated)
        return updated

    lead = f.WorkspaceContract.lead
    def setUp(self):
        f.WorkspaceContract.setUp(self)
        self.tid = str(uuid.uuid4())
        self.a = self.agent_update(self.lead(), threadId=self.tid, status='idle', inFlight=False)
        from codex_native_tools import catalog, mark_current
        tools = self.runtime.tool_definitions(self.a)
        mark_current(self.a, tools)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.a)
        self.home = self.root / 'native-home'
        self.path = self.home / 'sessions' / ('rollout-2026-09-14-' + self.tid + '.jsonl')
        self.path.parent.mkdir(parents=True)
        self.text = json.dumps({'id': 'monitor-fixture', 'exitCode': 0, 'stdout': 'retained-event-' * 700})
        self.event = {'id': 'monitor:fixture', 'kind': 'monitor_exit', 'text': self.text,
                      'turn_id': 'turn', 'agent': self.a['id'], 'epoch': self.a['epoch'],
                      'status': 'delivered', 'created': 1, 'error': None}
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                       tuple(self.event[k] for k in ('id','agent','kind','text','status','created','epoch','turn_id','error')))
        message = {'type': 'message', 'role': 'user', 'id':'message-event',
            'internal_chat_message_metadata_passthrough': {'turn_id': 'turn'},
            'content': [{'type': 'input_text', 'text': '[Orchestration event: monitor_exit]\n' + self.text + '\n\nPreserve this instruction.'}]}
        self.records = [
            {'type': 'session_meta', 'payload': {'id': self.tid, 'dynamic_tools': catalog(tools)}},
            {'type': 'turn_context', 'payload': {'turn_id': 'turn'}},
            {'type': 'response_item', 'payload': copy.deepcopy(message)},
            {'type': 'event_msg', 'payload': {'type': 'user_message', 'message': self.text}},
            {'type': 'compacted', 'payload': {'message': 'Preserve summary.', 'replacement_history': [
                copy.deepcopy(message), {'type': 'function_call_output', 'call_id': 'command', 'output': 'exitCode=0 receipt-192'},
                {'type': 'message', 'role': 'user', 'content': [{'type': 'input_image', 'image_url': 'fixture-image'},
                    {'type': 'input_text', 'text': 'Preserve real user text.'}]}]}}]
        self.records.append({'type':'event_msg','payload':{'type':'task_complete','turn_id':'turn'}})
        self.write_records()
        self.server = Server(self.path, self.tid)
        self.patches = [patch.object(self.runtime, 'connect', return_value=self.server),
                        patch.object(self.runtime.accounts, 'home', return_value=self.home),
                        patch.object(self.runtime, 'connection_current', return_value=True)]
        for p in self.patches:
            p.start()

    def tearDown(self):
        for p in self.patches:
            p.stop()
        f.WorkspaceContract.tearDown(self)

    def write_records(self):
        self.path.write_text(''.join(json.dumps(r) + '\n' for r in self.records))

    def forks(self):
        return [p for m, p in self.server.calls if m == 'thread/fork']

    def test_fork_carries_only_the_exact_current_source_catalog(self):
        expected_digest = self.a['nativeToolCatalog']['digest']
        repaired = repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(repaired['nativeToolCatalog'], {
            'threadId': repaired['threadId'], 'digest': expected_digest})

    def test_fork_does_not_certify_an_outdated_source_catalog(self):
        previous = {'threadId': self.tid, 'digest': 'outdated-catalog'}
        self.agent_update(self.a, nativeToolCatalog=previous)
        repaired = repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(repaired['nativeToolCatalog'], previous)
        self.assertNotEqual(repaired['nativeToolCatalog']['threadId'], repaired['threadId'])

    def test_copy_preserves_user_history_and_tool_results(self):
        source = self.path.read_bytes()
        result = repair.repair_idle(self.runtime, self.a['id'])
        receipt = result['contextRepair']
        self.assertEqual(receipt['phase'], 'completed')
        self.assertNotEqual(result['threadId'], self.tid)
        self.assertEqual(receipt['source']['threadId'], self.tid)
        self.assertEqual(self.path.read_bytes(), source)
        clean = [json.loads(x) for x in Path(receipt['snapshot']['copyPath']).read_text().splitlines()]
        self.assertEqual(clean[3], self.records[3])
        self.assertEqual(clean[4]['payload']['message'], 'Preserve summary.')
        self.assertEqual(clean[4]['payload']['replacement_history'][1:], self.records[4]['payload']['replacement_history'][1:])
        self.assertIn('Preserve this instruction.', json.dumps(clean))
        self.assertIn('event:monitor:fixture', json.dumps(clean))
        self.assertLess(len(json.dumps(clean)), len(source) - 10000)
        self.assertEqual(Path(receipt['snapshot']['copyPath']).stat().st_mode & 0o777, 0o600)
        repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(len(self.forks()), 1)
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT text FROM runtime_events WHERE id=?', (self.event['id'],)).fetchone()[0], self.text)

    def test_interrupted_native_turn_supplies_missing_terminal_marker_in_copy(self):
        self.records.pop()
        self.write_records()
        self.server.turn_status = 'interrupted'
        source = self.path.read_bytes()
        result = repair.repair_idle(self.runtime, self.a['id'])
        receipt = result['contextRepair']
        self.assertEqual(receipt['phase'], 'completed')
        self.assertEqual(self.path.read_bytes(), source)
        clean = [json.loads(x) for x in Path(receipt['snapshot']['copyPath']).read_text().splitlines()]
        self.assertEqual(clean[-1]['payload']['type'], 'turn_aborted')
        self.assertEqual(clean[-1]['payload']['turn_id'], 'turn')

    def test_saved_native_tool_output_is_authorized_after_repair(self):
        request = {'id': 'saved-tool', 'params': {'threadId': self.tid, 'callId': 'saved-tool',
            'tool': 'orchestration_read', 'arguments': {'output_ref': 'saved-output'}}}
        receipt = self.runtime.reserve_tool_request(request, self.a.get('accountKey', 'default'))
        key = receipt['id']
        result = {'success':True, 'contentItems':[{'type':'inputText','text':'Saved command receipt walnut-271.'}]}
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', (key, json.dumps(result)))
            self.runtime.finish_tool_request(key, result, db=db)
        a = self.agent_update(self.a, autoWake=True)
        changed = repair.repair_idle(self.runtime, a['id'])
        self.assertEqual(changed['accountHistory'][-1]['threadId'], self.tid)
        output = self.runtime.model_read(a['id'], {'output_ref':key})
        self.assertEqual(output['text'], 'Saved command receipt walnut-271.')
        recovered = self.runtime.request_action(a['id'], {'action':'get','request_id':key})
        self.assertEqual(recovered['stage'], 'completed')
        self.assertEqual(recovered['result'], result)

    def test_user_quote_and_other_turn_never_authorize_replacement(self):
        self.records[2]['payload']['content'][0]['text'] = 'The user quotes this: ' + self.records[2]['payload']['content'][0]['text']
        self.records[4]['payload']['replacement_history'][0]['internal_chat_message_metadata_passthrough']['turn_id'] = 'other-turn'
        self.write_records()
        result = repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(result['contextRepair']['phase'], 'unchanged')
        self.assertEqual(self.forks(), [])

    def test_unchanged_cache_survives_append_and_rechecks_after_compaction(self):
        self.records[2]['payload']['internal_chat_message_metadata_passthrough']['turn_id'] = 'other'
        self.records[4]['payload']['replacement_history'][0]['internal_chat_message_metadata_passthrough']['turn_id'] = 'other'
        self.write_records()
        repair.repair_idle(self.runtime, self.a['id'])
        count = len(self.server.calls)
        repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(len(self.server.calls), count)
        self.agent_update(self.a, compactions=1)
        repair.repair_idle(self.runtime, self.a['id'])
        self.assertGreater(len(self.server.calls), count)
        count = len(self.server.calls)
        with self.path.open('a') as stream:
            stream.write(json.dumps({'type':'event_msg','payload':{'type':'task_complete'}}) + '\n')
        repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(len(self.server.calls), count)
        self.assertEqual(self.forks(), [])

    def test_normal_chat_without_oversized_events_uses_no_native_calls(self):
        with self.runtime.db() as db:
            db.execute('DELETE FROM runtime_events WHERE agent=?', (self.a['id'],))
        self.assertEqual(repair.repair_before_start(self.runtime, self.a)['threadId'], self.tid)
        self.assertEqual(self.server.calls, [])

    def test_stale_preparing_repair_without_attempt_is_retired(self):
        stale = {
            'id': 'stale-repair', 'agent': self.a['id'],
            'source': {'id': self.a['id'], 'accountKey': self.a['accountKey'],
                       'epoch': self.a['epoch'], 'threadId': self.tid},
            'phase': 'preparing', 'created': time.time() - 120,
            'updated': time.time() - 120,
        }
        self.agent_update(self.a, status='queued', autoWake=True,
                          inFlight=False, contextRepair=stale)
        with self.runtime.db() as db:
            repair.recover_context_failures(self.runtime, db,
                                            self.runtime.records(db, 'agents'))
        current = self.runtime.agent(self.a['id'])
        self.assertNotIn('contextRepair', current)
        self.assertEqual(current['lastContextRepairCheck']['status'], 'superseded')
        self.assertIn('no native fork', current['lastContextRepairCheck']['supersededReason'])

    def test_pending_repair_blocks_prepare_and_dispatch_but_preserves_queued_send(self):
        self.agent_update(self.a, autoWake=True, contextRepair={'id':'repair-exact','phase':'unknown'})
        result = self.runtime.send(self.a['id'], 'Keep this user message.', message_id='queued-exact', delivery='steer')
        self.assertEqual(result['id'], 'queued-exact')
        with self.assertRaisesRegex(ValueError, 'exact native receipt'):
            self.runtime.prepare_locked(self.runtime.agent(self.a['id']))
        self.runtime.dispatch()
        with self.runtime.db() as db:
            row = db.execute('SELECT text,status FROM runtime_events WHERE id=?', ('queued-exact',)).fetchone()
        self.assertEqual(tuple(row), ('Keep this user message.', 'pending'))
        self.assertFalse(any(m in {'turn/start','thread/fork','thread/resume','review/start','thread/compact/start'} for m, p in self.server.calls))

    def test_existing_native_recovery_blocks_and_failed_turn_does_not(self):
        for name, marker in [
            ('nativeSafetyRetry', {'stage':'unknown'}),
            ('browserRecovery', {'stage':'reconnecting'}),
            ('browserRecovery', {'stage':'failed','nativeRequest':{'submittedAt':1}}),
            ('restartRecovery', {'stage':'held'})]:
            with self.subTest(name=name, marker=marker):
                a = self.agent_update(self.a, **{name:{**marker, 'threadId':self.tid,
                    'epoch':self.a['epoch'], 'accountKey':self.a['accountKey']}})
                with self.assertRaisesRegex(ValueError, 'existing native recovery'):
                    repair.repair_idle(self.runtime, a['id'])
                self.agent_update(self.a, **{name:None})
        self.agent_update(self.a, nativeFailureHold=True, status='failed')
        self.assertEqual(repair.repair_idle(self.runtime, self.a['id'])['contextRepair']['phase'], 'completed')

    def test_same_turn_user_quote_with_another_native_id_is_ambiguous(self):
        quote = copy.deepcopy(self.records[2])
        quote['payload']['id'] = 'message-user-quote'
        self.records.insert(3, quote)
        self.write_records()
        source = self.path.read_bytes()
        with self.assertRaisesRegex(ValueError, 'ambiguous'):
            repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(self.forks(), [])
        self.assertEqual(self.path.read_bytes(), source)

    def test_user_quote_in_mixed_native_input_is_preserved(self):
        quoted = '[Orchestration event: monitor_exit]\n' + self.text
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('user-quote',self.a['id'],'user',quoted,'delivered',2,self.a['epoch'],'turn',None))
        self.records[2]['payload']['content'][0]['text'] = quoted + '\n\n' + quoted
        self.records[4]['payload']['replacement_history'][0]['content'][0]['text'] = quoted + '\n\n' + quoted
        self.write_records()
        data = self.path.read_bytes()
        repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(self.path.read_bytes(), data)
        self.assertEqual(self.forks(), [])

    def test_prior_epoch_events_repair_and_projected_new_events_skip(self):
        self.agent_update(self.a, epoch=self.a['epoch'] + 1)
        changed = repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(changed['contextRepair']['phase'], 'completed')
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('projected',self.a['id'],'monitor_exit',self.text,'delivered',3,changed['epoch'],'next-turn',None))
            db.execute('INSERT INTO runtime_event_meta VALUES (?,?)', ('projected',json.dumps({'modelEventProjection':1})))
        before = len(self.server.calls)
        repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(len(self.server.calls), before)

    def test_actor_account_epoch_thread_and_attempt_changes_block_fork(self):
        for field, value in [('accountKey','other'), ('epoch',99), ('threadId','other'),
                             ('startAttempt',{'id':'other','submitted':False})]:
            with self.subTest(field=field):
                self.agent_update(self.a, contextRepair={}, accountKey=self.a['accountKey'], epoch=self.a['epoch'], threadId=self.tid, startAttempt=None)
                self.server.before_read = lambda field=field, value=value: self.agent_update(self.a, **{field:value})
                with self.assertRaisesRegex(ValueError, 'changed'):
                    repair.repair_idle(self.runtime, self.a['id'])
                self.assertEqual(self.forks(), [])

    def test_active_native_turn_and_pending_tool_output_block(self):
        self.server.status = 'active'
        with self.assertRaisesRegex(ValueError, 'idle native'):
            repair.repair_idle(self.runtime, self.a['id'])
        self.server.status = 'idle'
        self.server.items = [{'item': {'type': 'commandExecution', 'status': 'inProgress'}}]
        with self.assertRaisesRegex(ValueError, 'tool receipts'):
            repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(self.forks(), [])

    def test_unloaded_history_repairs_without_resuming_or_sending_input(self):
        self.server.status = 'notLoaded'
        result = repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(result['contextRepair']['phase'], 'completed')
        self.assertEqual(len(self.forks()), 1)
        methods = [m for m, _ in self.server.calls]
        self.assertIn('thread/queue/list', methods)
        self.assertNotIn('thread/backgroundTerminals/list', methods)
        self.assertNotIn('thread/resume', methods)
        self.assertNotIn('turn/start', methods)

    def test_unloaded_history_still_requires_empty_queue_and_terminal_receipts(self):
        self.server.status = 'notLoaded'
        for field, value, error in [
            ('queue', [{'id': 'queued'}], 'queued input'),
            ('turn_status', 'inProgress', 'terminal native turn'),
            ('items', [{'item': {'type': 'dynamicToolCall', 'status': 'inProgress'}}], 'tool receipts')]:
            with self.subTest(field=field):
                original = getattr(self.server, field)
                setattr(self.server, field, value)
                with self.assertRaisesRegex(ValueError, error):
                    repair.repair_idle(self.runtime, self.a['id'])
                setattr(self.server, field, original)
                self.assertEqual(self.forks(), [])

    def test_loaded_missing_thread_error_is_not_treated_as_idle(self):
        original = self.server.call
        def call(method, params, timeout=10):
            if method == 'thread/backgroundTerminals/list':
                raise RuntimeError('thread not found: ' + self.tid)
            return original(method, params, timeout)
        self.server.call = call
        with self.assertRaisesRegex(ValueError, 'native history read'):
            repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(self.forks(), [])

    def test_system_error_needs_no_queue_jobs_or_pending_native_tool_outcomes(self):
        self.server.status = 'systemError'
        for field, value, error in [
            ('queue', [{'id':'queued'}], 'queued input'),
            ('terminals', [{'id':'active-command'}], 'native commands'),
            ('turn_status', 'inProgress', 'terminal native turn'),
            ('no_turns', True, 'terminal native turn'),
            ('items', [{'item':{'type':'dynamicToolCall','status':'inProgress'}}], 'tool receipts')]:
            with self.subTest(field=field):
                original = getattr(self.server, field)
                setattr(self.server, field, value)
                with self.assertRaisesRegex(ValueError, error):
                    repair.repair_idle(self.runtime, self.a['id'])
                setattr(self.server, field, original)
                self.assertEqual(self.forks(), [])
                self.assertFalse(any(m == 'thread/unsubscribe' for m, p in self.server.calls))
        self.server.turn_status = 'failed'
        result = repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(result['contextRepair']['phase'], 'completed')
        self.assertEqual(len(self.forks()), 1)
        unsubscribes = [p for m,p in self.server.calls if m == 'thread/unsubscribe']
        self.assertTrue(all(p['threadId'] == self.tid for p in unsubscribes))

    def test_native_pending_permission_and_user_input_are_not_idle(self):
        for flag in ('waitingOnApproval', 'waitingOnUserInput'):
            self.server.status = {'type':'active', 'activeFlags':[flag]}
            with self.assertRaisesRegex(ValueError, 'native status:.*' + flag):
                repair.repair_idle(self.runtime, self.a['id'])
            self.assertEqual(self.forks(), [])
            self.assertFalse(any(m == 'thread/unsubscribe' for m, p in self.server.calls))

    def test_monitor_and_unknown_tool_receipts_block(self):
        with self.runtime.db() as db:
            self.runtime.put(db, 'monitors', {'id':'active', 'agent':self.a['id'], 'status':'running'})
        with self.assertRaisesRegex(ValueError, 'monitors'):
            repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(self.server.calls, [])

    def test_unknown_fork_never_repeats_and_late_receipt_settles(self):
        self.server.hold = True
        with patch.object(repair, 'WAIT_SECONDS', 0.01), self.assertRaisesRegex(RuntimeError, 'outcome unknown'):
            repair.repair_idle(self.runtime, self.a['id'])
        a = self.runtime.agent(self.a['id'])
        self.assertEqual(a['contextRepair']['phase'], 'unknown')
        self.assertEqual(a['threadId'], self.tid)
        with self.assertRaisesRegex(ValueError, 'exact native receipt'):
            repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(len(self.forks()), 1)
        new = str(uuid.uuid4())
        self.server.futures[0].set_result({'thread': {'id': new}})
        self.assertEqual(self.runtime.agent(self.a['id'])['threadId'], new)
        self.assertEqual(self.runtime.agent(self.a['id'])['contextRepair']['phase'], 'completed')
        self.assertEqual(len(self.forks()), 1)

    def test_prepared_attempt_survives_late_repair(self):
        from codex_runtime import PreparationPending
        a = self.agent_update(self.a, inFlight=True, status='starting', startAttempt={
            'id':'start-exact', 'submitted':False, 'events':[]})
        self.server.hold = True
        with patch.object(repair, 'WAIT_SECONDS', 0.01), self.assertRaises(PreparationPending) as caught:
            repair.repair_before_start(self.runtime, a)
        self.server.futures[0].set_result({'thread': {'id': 'new-native'}})
        caught.exception.future.result(1)
        current = self.runtime.agent(a['id'])
        self.assertEqual(current['startAttempt']['id'], 'start-exact')
        self.assertEqual(current['startAttempt']['threadId'], 'new-native')
        self.assertFalse(current['startAttempt']['submitted'])
        self.assertEqual(repair.repair_before_start(self.runtime, current)['threadId'], 'new-native')
        self.assertEqual(len(self.forks()), 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)

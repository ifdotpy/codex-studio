#!/usr/bin/env python3
"""Maintenance waits preserve accepted input and terminal unknown receipts."""
import concurrent.futures
import copy
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('actions', Path(__file__).with_name('native-action-context-repair-contract.py'))
f = importlib.util.module_from_spec(spec); spec.loader.exec_module(f)
repair, eventually = f.f.repair, f.f.f.eventually


class ContextWait(f.NativeActionRepair):
    def setUp(self):
        super().setUp()
        submit = self.server.submit
        def call(method, params):
            if method != 'turn/start':
                return submit(method, params)
            self.server.calls.append((method, copy.deepcopy(params)))
            future = concurrent.futures.Future()
            future.set_result({'turn':{'id':'resumed-turn','status':'inProgress'}})
            return 'resumed-rpc', method, future
        self.server.submit = call

    def put(self, table, value):
        with self.runtime.db() as db:
            self.runtime.put(db, table, value)

    def monitor(self):
        self.put('monitors', {'id':'exact-active-monitor','agent':self.a['id'],'status':'running'})

    def clear_monitor(self):
        self.put('monitors', {'id':'exact-active-monitor','agent':self.a['id'],'status':'completed'})

    def due(self):
        a = self.runtime.agent(self.a['id'])
        a['contextRepairWait']['nextCheckAt'] = 0
        self.agent_update(a, contextRepairWait=a['contextRepairWait'])
        self.runtime.dispatch()

    def test_active_monitor_does_not_block_normal_input_after_compaction(self):
        source = self.path.read_bytes()
        self.agent_update(self.a, compactions=1, contextRepair={
            'phase':'unchanged', 'source':{'threadId':self.tid},
            'compactions':0, 'checkedEventIds':[self.event['id']]})
        self.monitor()
        self.runtime.send(self.a['id'], 'Keep one exact request.', message_id='wait-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        current = self.runtime.agent(self.a['id'])
        self.assertFalse(current.get('contextRepairWait'))
        self.assertEqual(current['threadId'], self.tid)
        self.assertEqual(self.forks(), [])
        self.assertEqual(source, self.path.read_bytes())
        self.runtime.dispatch()
        starts = [p for m,p in self.server.calls if m == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'wait-user')
        with self.runtime.db() as db:
            monitor = json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?',
                                          ('exact-active-monitor',)).fetchone()[0])
        self.assertEqual(monitor['status'], 'running')

    def test_existing_monitor_wait_resumes_same_input_without_fork(self):
        self.monitor()
        self.runtime.send(self.a['id'], 'Keep one exact request.', message_id='wait-user')
        # Reproduce the pre-update mandatory maintenance gate.
        with patch.object(repair, '_optional_monitor_repair', return_value=False):
            self.runtime.dispatch()
            eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        waiting = self.runtime.agent(self.a['id'])
        self.assertEqual(waiting['status'], 'queued')
        self.assertIn('monitors: exact-active-monitor', waiting['error'])
        attempt_id = waiting['startAttempt']['id']
        self.due()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(current['startAttempt']['id'], attempt_id)
        self.assertEqual(current['startAttempt']['events'], ['wait-user'])
        self.assertEqual(len(self.forks()), 0)
        self.assertEqual(current['threadId'], self.tid)
        starts = [p for m,p in self.server.calls if m == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'wait-user')

    def command(self, **changes):
        record = {'id':'exact-command', 'agent':self.a['id'], 'kind':'command',
                  'type':'commandExecution', 'status':'running', 'processId':'27427'}
        record.update(changes)
        self.put('tasks', record)
        return record

    def test_active_command_does_not_block_normal_input_or_change_its_receipt(self):
        command = self.command()
        source = self.path.read_bytes()
        self.runtime.send(self.a['id'], 'Read the existing process result.', message_id='command-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        current = self.runtime.agent(self.a['id'])
        self.assertFalse(current.get('contextRepairWait'))
        self.assertEqual(current['threadId'], self.tid)
        self.assertEqual(self.forks(), [])
        self.assertEqual(source, self.path.read_bytes())
        with self.runtime.db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_tasks WHERE id=?', (command['id'],)).fetchone()[0])
        self.assertEqual(saved, command)
        self.runtime.dispatch()
        starts = [p for m,p in self.server.calls if m == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'command-user')

    def test_existing_command_wait_resumes_same_attempt(self):
        self.command()
        self.runtime.send(self.a['id'], 'Continue with the existing command.', message_id='command-user')
        with patch.object(repair, '_optional_monitor_repair', return_value=False):
            self.runtime.dispatch()
            eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        waiting = self.runtime.agent(self.a['id'])
        self.assertIn('tasks: exact-command', waiting['error'])
        attempt_id = waiting['startAttempt']['id']
        self.due()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(current['startAttempt']['id'], attempt_id)
        self.assertEqual(current['threadId'], self.tid)
        self.assertEqual(self.forks(), [])
        self.assertEqual(len([m for m,p in self.server.calls if m == 'turn/start']), 1)

    def test_optional_command_path_preserves_unknown_and_tool_holds(self):
        self.runtime.send(self.a['id'], 'Preserve uncertain operations.', message_id='unknown-command-user')
        # Claim the start synchronously to inspect the same pre-submission guard.
        self.command(status='unknown')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        for changes in ({'status':'unknown'}, {'status':'pending'}, {'processId':None}, {'kind':'tool'}):
            with self.subTest(changes=changes):
                self.command(**changes)
                self.due()
                self.assertFalse(any(m == 'turn/start' for m,p in self.server.calls))
                self.assertEqual(self.forks(), [])
        self.command()
        self.put('requests', {'id':'approval','agent':self.a['id'],'status':'pending',
                              'method':'item/commandExecution/requestApproval'})
        self.due()
        self.assertFalse(any(m == 'turn/start' for m,p in self.server.calls))

    def test_optional_monitor_path_preserves_uncertain_input_gate(self):
        self.legacy_uncertain()
        self.monitor()
        self.runtime.send(self.a['id'], 'Preserve all receipts.', message_id='uncertain-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        self.due()
        self.assertFalse(any(m == 'turn/start' for m,p in self.server.calls))
        self.assertEqual(self.forks(), [])

    def test_optional_monitor_path_preserves_other_local_blockers(self):
        self.monitor()
        self.runtime.send(self.a['id'], 'Wait for required approval.', message_id='approval-user')
        self.put('requests', {'id':'approval','agent':self.a['id'],'status':'pending',
                              'method':'item/commandExecution/requestApproval'})
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        self.due()
        self.assertFalse(any(m == 'turn/start' for m,p in self.server.calls))
        self.assertEqual(self.forks(), [])

    def test_async_question_survives_fork_and_answer_uses_current_thread_once(self):
        question = {'id':'question-exact','agent':self.a['id'],'epoch':self.a['epoch'],
            'accountKey':self.a['accountKey'],'method':'agent/asyncQuestion','status':'pending',
            'params':{'threadId':self.tid,'questions':[{'id':'color','question':'Choose the color.'}]}}
        self.put('requests',question)
        self.runtime.send(self.a['id'],'Continue while the question is pending.',message_id='question-continue')
        self.runtime.dispatch()
        eventually(lambda:self.runtime.agent(self.a['id']).get('turnId')=='resumed-turn')
        a=self.runtime.agent(self.a['id'])
        with self.runtime.db() as db:
            saved=json.loads(db.execute('SELECT record FROM runtime_requests WHERE id=?',(question['id'],)).fetchone()[0])
        self.assertEqual(saved,question)
        self.assertNotEqual(a['threadId'],self.tid)
        self.agent_update(a,status='idle',inFlight=False,turnId=None)
        answer={'answers':{'color':{'answers':['Green']}}}
        self.assertEqual(self.runtime.answer(question['id'],answer),{'status':'answered'})
        self.assertTrue(self.runtime.answer(question['id'],answer)['replayed'])
        self.runtime.dispatch()
        eventually(lambda:len([m for m,p in self.server.calls if m=='turn/start'])==2)
        answers=[p for m,p in self.server.calls if m=='turn/start' and p.get('clientUserMessageId')=='question-exact:answer']
        self.assertEqual(len(answers),1)
        self.assertEqual(answers[0]['threadId'],a['threadId'])
        self.assertIn('Green',json.dumps(answers[0]['input']))
        self.assertEqual(len(self.forks()),1)

    def test_native_blocking_requests_still_prevent_repair(self):
        for method in ('item/tool/requestUserInput','item/commandExecution/requestApproval','unknown/request'):
            with self.subTest(method=method):
                record={'id':'blocking-exact','agent':self.a['id'],'method':method,'status':'pending'}
                self.put('requests',record)
                with self.assertRaisesRegex(ValueError,'requests: blocking-exact'):
                    repair.repair_idle(self.runtime,self.a['id'])
                self.assertEqual(self.forks(),[])

    def test_failed_tool_response_preserves_unknown_outcome(self):
        record = {'id':'failed-tool','agent':self.a['id'],'stage':'failed','outcome':'unknown',
                  'finished':1,'result':{'success':False,'contentItems':[{'type':'inputText','text':'Unknown command watch'}]}}
        self.put('tool_requests', record)
        self.assertEqual(repair.repair_idle(self.runtime, self.a['id'])['contextRepair']['phase'], 'completed')
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.tool_request(record['id'],db), record)

    def test_native_history_timeout_defers_exact_start_then_resumes_once(self):
        from codex_runtime import ResponseTimeout
        original = self.server.call
        unavailable = [True]
        def call(method, params, timeout=10):
            if unavailable[0] and method == 'thread/items/list':
                raise ResponseTimeout('thread/items/list response timed out; outcome unknown')
            return original(method, params, timeout)
        self.server.call = call
        self.runtime.send(self.a['id'], 'Preserve accepted input.', message_id='read-timeout-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        a = self.runtime.agent(self.a['id'])
        self.assertEqual(a['status'], 'queued')
        self.assertFalse(a['inFlight'])
        self.assertFalse(a['startAttempt']['submitted'])
        attempt_id = a['startAttempt']['id']
        self.assertIn('native history read (thread/items/list)', a['error'])
        self.assertEqual(self.forks(), [])
        self.due()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        self.assertEqual(self.runtime.agent(self.a['id'])['contextRepairWait']['checks'], 2)
        self.assertEqual(self.runtime.agent(self.a['id']).get('contextRepairHistory', []), [])
        unavailable[0] = False
        self.due()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        a = self.runtime.agent(self.a['id'])
        self.assertEqual(a['startAttempt']['id'], attempt_id)
        self.assertEqual(a['startAttempt']['events'], ['read-timeout-user'])
        self.assertEqual(len(self.forks()), 1)
        self.assertEqual(len([m for m,p in self.server.calls if m == 'turn/start']), 1)

    def test_interrupted_receipt_requires_exact_old_turn_native_terminal_item(self):
        record = {'id':'missing-receipt','agent':self.a['id'],'stage':'interrupted','outcome':'unknown',
                  'threadId':self.tid,'accountKey':self.a['accountKey'],'turnId':'old-turn','callId':'old-call'}
        self.put('tool_requests', record)
        original = self.server.call
        native_status = ['inProgress']
        def call(method, params, timeout=10):
            if method == 'thread/items/list' and params['turnId'] == 'old-turn':
                self.server.calls.append((method,copy.deepcopy(params)))
                return {'data':[{'turnId':'old-turn','item':{'type':'dynamicToolCall','id':'old-call',
                                 'status':native_status[0],'success':False}}]}
            return original(method,params,timeout)
        self.server.call = call
        with self.assertRaisesRegex(ValueError, 'exact tool receipt: missing-receipt'):
            repair.repair_idle(self.runtime,self.a['id'])
        self.assertEqual(self.forks(), [])
        native_status[0] = 'failed'
        repair.repair_idle(self.runtime,self.a['id'])
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.tool_request(record['id'],db), record)
        self.assertTrue(any(p.get('turnId') == 'old-turn' for m,p in self.server.calls if m == 'thread/items/list'))

    def test_foreign_receipt_never_uses_same_call_id_on_current_thread(self):
        record = {'id':'foreign-receipt','agent':self.a['id'],'stage':'interrupted','outcome':'unknown',
                  'threadId':'old-native','accountKey':'other-account','turnId':'turn','callId':'same-call'}
        self.put('tool_requests', record)
        self.server.items = [{'item':{'type':'dynamicToolCall','id':'same-call','status':'failed','success':False}}]
        with self.assertRaisesRegex(ValueError, 'original account/thread receipt'):
            repair.repair_idle(self.runtime,self.a['id'])
        self.assertEqual(self.forks(), [])
        self.agent_update(self.a, accountHistory=[{'threadId':'old-native','accountKey':'other-account'}])
        repair.repair_idle(self.runtime,self.a['id'])
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.tool_request(record['id'],db), record)

    def test_paged_native_items_reject_pending_on_later_page(self):
        original = self.server.call
        terminal = [False]
        def call(method,params,timeout=10):
            if method == 'thread/items/list':
                self.server.calls.append((method,copy.deepcopy(params)))
                if not params.get('cursor'):
                    return {'data':[{'turnId':'turn','item':{'id':str(i),'type':'reasoning'}} for i in range(1000)], 'nextCursor':'page-2'}
                self.assertEqual(params['cursor'],'page-2')
                return {'data':[{'turnId':'turn','item':{'id':'last-tool','type':'dynamicToolCall',
                    'status':'failed' if terminal[0] else 'inProgress','success':False}}]}
            return original(method,params,timeout)
        self.server.call = call
        with self.assertRaisesRegex(ValueError, 'complete native tool receipts'):
            repair.repair_idle(self.runtime,self.a['id'])
        self.assertEqual(self.forks(), [])
        terminal[0] = True
        repair.repair_idle(self.runtime,self.a['id'])
        self.assertEqual(len(self.forks()), 1)

    def test_missing_terminal_tail_defers_without_unloading_or_forking(self):
        terminal = self.records.pop()
        self.write_records()
        self.runtime.send(self.a['id'], 'Preserve the latest turn.', message_id='tail-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        a = self.runtime.agent(self.a['id'])
        self.assertIn('saved terminal turn: turn', a['error'])
        attempt_id = a['startAttempt']['id']
        self.assertFalse(a['startAttempt']['submitted'])
        self.assertEqual(self.forks(), [])
        self.assertFalse(any(m == 'thread/unsubscribe' for m,p in self.server.calls))
        self.records.append(terminal)
        self.write_records()
        self.due()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        self.assertEqual(self.runtime.agent(a['id'])['startAttempt']['id'], attempt_id)
        self.assertEqual(len(self.forks()), 1)

    def test_exact_unchanged_repair_preparation_failure_is_recovered(self):
        error = 'Thread preparation belongs to an earlier agent state'
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('unchanged-user',self.a['id'],'user','Keep this','failed',2,self.a['epoch'],None,error))
        a = self.agent_update(self.a,status='failed',error=error,inFlight=False,startAttempt={
            'id':'unchanged-attempt','submitted':False,'events':['unchanged-user'],
            'epoch':self.a['epoch'],'accountKey':self.a['accountKey']})
        receipt = {'id':'unchanged-operation','phase':'unchanged','source':repair._identity(a),
                   'settings':self.runtime.preparation_settings(a),'snapshot':{'eventIds':[]}}
        a = self.agent_update(a,contextRepair=receipt)
        with self.runtime.lock,self.runtime.db() as db:
            repair.recover_context_failures(self.runtime,db,[a])
        self.assertTrue(self.runtime.agent(a['id'])['contextRepairWait']['historicalFailureRecovered'])
        self.due()
        eventually(lambda:self.runtime.agent(a['id']).get('turnId')=='resumed-turn')
        self.assertEqual(self.runtime.agent(a['id'])['startAttempt']['id'],'unchanged-attempt')

    def test_old_source_cleanup_unknown_never_repeats_fork_or_input(self):
        original = self.server.submit
        cleanup = concurrent.futures.Future()
        def submit(method, params):
            if method == 'thread/unsubscribe':
                self.server.calls.append((method, copy.deepcopy(params)))
                self.assertEqual(params['threadId'], self.tid)
                self.assertNotEqual(self.runtime.agent(self.a['id'])['threadId'], self.tid)
                return 'source-cleanup-rpc', method, cleanup
            return original(method, params)
        self.server.submit = submit
        self.runtime.send(self.a['id'], 'Keep the successful fork.', message_id='cleanup-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        eventually(lambda: (self.runtime.agent(self.a['id'])['contextRepair'].get('sourceCleanup') or {}).get('requestId'))
        cleanup.set_exception(RuntimeError('Cleanup disconnected; outcome unknown'))
        a = self.runtime.agent(self.a['id'])
        self.assertEqual(a['contextRepair']['phase'], 'completed')
        self.assertEqual(a['contextRepair']['sourceCleanup']['phase'], 'unknown')
        self.assertEqual(a['turnId'], 'resumed-turn')
        self.runtime.dispatch()
        self.assertEqual(len(self.forks()), 1)
        self.assertEqual(len([m for m,p in self.server.calls if m == 'thread/start']), 0)
        self.assertEqual(len([m for m,p in self.server.calls if m == 'turn/start']), 1)
        self.assertEqual(len([m for m,p in self.server.calls if m == 'thread/unsubscribe']), 1)

    def legacy_uncertain(self, count=1):
        rows = [('legacy-offline-'+str(i),self.a['id'],'agent_message','Preserve unknown receipt '+str(i),
                 'uncertain',2,self.a['epoch'],None,'Codex app-server is offline') for i in range(count)]
        with self.runtime.db() as db:
            db.executemany('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',rows)
            db.execute('INSERT INTO runtime_completed_turns VALUES (?)',(self.a['id']+':turn',))
        self.records[-1]['payload']['completed_at'] = 3
        self.write_records()
        return rows

    def test_21_historical_offline_receipts_stay_unknown_and_are_not_replayed(self):
        original = self.legacy_uncertain(21)
        self.runtime.send(self.a['id'],'Submit only the new input.',message_id='new-after-offline')
        self.runtime.dispatch()
        eventually(lambda:self.runtime.agent(self.a['id']).get('turnId')=='resumed-turn')
        a = self.runtime.agent(self.a['id'])
        proof = a['contextRepair']['historicalInputProof']
        self.assertEqual(len(proof['events']),21)
        self.assertEqual(proof['deliveryOutcome'],'unknown')
        self.assertEqual(proof['terminalTurnId'],'turn')
        with self.runtime.db() as db:
            for row in original:
                self.assertEqual(tuple(db.execute('SELECT * FROM runtime_events WHERE id=?',(row[0],)).fetchone()),row)
        starts = [p for m,p in self.server.calls if m=='turn/start']
        self.assertEqual(len(starts),1)
        self.assertNotIn('Preserve unknown receipt',json.dumps(starts))
        self.assertEqual(a['startAttempt']['events'],['new-after-offline'])

    def test_current_or_identified_uncertain_input_still_blocks(self):
        rows = self.legacy_uncertain()
        a = self.agent_update(self.a,startAttempt={'id':'safe-attempt','submitted':False,'events':[]})
        for mutation in ('metadata','current','other-error','known-turn','reserved','dispatching'):
            with self.subTest(mutation=mutation):
                with self.runtime.db() as db:
                    db.execute('DELETE FROM runtime_event_meta WHERE id=?',(rows[0][0],))
                    db.execute("UPDATE runtime_events SET error=?,turn_id=NULL,status='uncertain' WHERE id=?",('Codex app-server is offline',rows[0][0]))
                    if mutation=='metadata':
                        db.execute('INSERT INTO runtime_event_meta VALUES (?,?)',(rows[0][0],json.dumps({'native':{
                            'connectionId':self.runtime.connection_ids['default'],'threadId':self.tid}})))
                    elif mutation=='other-error':
                        db.execute('UPDATE runtime_events SET error=? WHERE id=?',('Native response timed out; outcome unknown',rows[0][0]))
                    elif mutation=='known-turn':
                        db.execute('UPDATE runtime_events SET turn_id=? WHERE id=?',('turn',rows[0][0]))
                    elif mutation in {'reserved','dispatching'}:
                        db.execute('UPDATE runtime_events SET status=? WHERE id=?',(mutation,rows[0][0]))
                a=self.agent_update(a,startAttempt={'id':'safe-attempt','submitted':False,
                    'events':[rows[0][0]] if mutation=='current' else []})
                with self.assertRaisesRegex(ValueError,'confirmed input receipt'):
                    repair.repair_idle(self.runtime,a['id'])
                self.assertEqual(self.forks(),[])

    def test_historical_receipts_need_later_saved_and_observed_terminal_without_events(self):
        rows = self.legacy_uncertain()
        a=self.agent_update(self.a,startAttempt={'id':'safe-attempt','submitted':False,'events':[]})
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_event_meta VALUES (?,?)',(self.event['id'],json.dumps({'modelEventProjection':1})))
            db.execute('DELETE FROM runtime_completed_turns')
        with self.assertRaisesRegex(ValueError,'later confirmed native terminal'):
            repair.repair_idle(self.runtime,a['id'])
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_completed_turns VALUES (?)',(a['id']+':turn',))
        self.records[-1]['payload']['completed_at']=1
        self.write_records()
        with self.assertRaisesRegex(ValueError,'later confirmed native terminal'):
            repair.repair_idle(self.runtime,a['id'])
        self.records[-1]['payload']['completed_at']=3
        self.write_records()
        result=repair.repair_idle(self.runtime,a['id'])
        self.assertEqual(result['contextRepair']['phase'],'unchanged')
        self.assertEqual(result['contextRepair']['historicalInputProof']['events'][0]['id'],rows[0][0])
        count=len(self.server.calls)
        repair.repair_idle(self.runtime,a['id'])
        self.assertEqual(len(self.server.calls),count)
        self.assertEqual(self.forks(),[])

    def test_existing_failed_exact_batch_is_recovered_without_new_ids(self):
        error = 'Context repair waits for commands, monitors, and tool receipts'
        ids = ['old-user-' + str(i) for i in range(19)]
        with self.runtime.db() as db:
            for key in ids:
                db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                    (key,self.a['id'],'user','Original '+key,'failed',2,self.a['epoch'],None,error))
        self.agent_update(self.a,status='failed',error=error,inFlight=False,startAttempt={
            'id':'old-unsent','submitted':False,'events':ids,'epoch':self.a['epoch'],'accountKey':self.a['accountKey']})
        self.runtime.dispatch()
        waiting = self.runtime.agent(self.a['id'])
        self.assertEqual(waiting['status'],'queued')
        self.assertTrue(waiting['contextRepairWait']['historicalFailureRecovered'])
        self.assertEqual(waiting['startAttempt']['id'],'old-unsent')
        self.due()
        eventually(lambda:self.runtime.agent(self.a['id']).get('turnId')=='resumed-turn')
        current=self.runtime.agent(self.a['id'])
        self.assertEqual(current['startAttempt']['events'],ids)
        self.assertEqual(current['startAttempt']['id'],'old-unsent')
        self.assertEqual(len([m for m,p in self.server.calls if m=='turn/start']),1)

    def test_migration_preserves_user_pause_and_submission_uncertainty(self):
        error='Context repair waits for complete native tool receipts'
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('unsent',self.a['id'],'user','Keep this','failed',2,self.a['epoch'],None,error))
        base={'id':'held','submitted':False,'events':['unsent'],'epoch':self.a['epoch'],'accountKey':self.a['accountKey']}
        for fields in ({'autoWake':False},{'startAttempt':{**base,'submitted':True}}, {'startAttempt':{**base,'epoch':99}}):
            a=self.agent_update(self.a,status='failed',error=error,inFlight=False,autoWake=True,startAttempt=base)
            a=self.agent_update(a,**fields)
            with self.runtime.lock,self.runtime.db() as db:
                repair.recover_context_failures(self.runtime,db,[a])
            self.assertFalse(self.runtime.agent(a['id']).get('contextRepairWait'))
            self.assertEqual(self.runtime.agent(a['id'])['status'],'failed')

    def test_structured_provider_error_does_not_block_other_recovery(self):
        other = self.runtime.agent(self.a['id'])
        other.update(id='provider-limited', status='failed', error={
            'message':'Usage limit reached', 'codexErrorInfo':'usageLimitExceeded'},
            nativeFailureHold=True, inFlight=False)
        self.put('agents', other)
        error = 'Context repair waits for complete native tool receipts'
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('mixed-unsent',self.a['id'],'user','Preserve this input','failed',2,self.a['epoch'],None,error))
        a = self.agent_update(self.a,status='failed',error=error,inFlight=False,startAttempt={
            'id':'mixed-attempt','submitted':False,'events':['mixed-unsent'],
            'epoch':self.a['epoch'],'accountKey':self.a['accountKey']})
        with self.runtime.lock,self.runtime.db() as db:
            repair.recover_context_failures(self.runtime,db,[other,a])
        self.assertEqual(self.runtime.agent(other['id'])['error'], other['error'])
        self.assertTrue(self.runtime.agent(other['id'])['nativeFailureHold'])
        self.assertTrue(self.runtime.agent(a['id'])['contextRepairWait']['historicalFailureRecovered'])
        self.due()
        eventually(lambda:self.runtime.agent(a['id']).get('turnId')=='resumed-turn')
        self.assertEqual(len([m for m,p in self.server.calls if m=='turn/start']),1)

    def test_native_action_wait_keeps_receipt_and_resumes_once(self):
        self.monitor()
        result=self.runtime.native_action(self.a['id'],'review','wait-review')
        self.assertEqual(result['outcome']['status'],'pending')
        current=self.runtime.agent(self.a['id'])
        attempt=current['startAttempt']['id']
        self.assertEqual(current['status'],'queued')
        self.clear_monitor();self.due()
        eventually(lambda:self.runtime.agent(self.a['id']).get('turnId')=='review-turn')
        replay=self.runtime.native_action(self.a['id'],'review','wait-review')
        self.assertEqual(replay['outcome']['status'],'acknowledged')
        self.assertEqual(self.runtime.agent(self.a['id'])['startAttempt']['id'],attempt)
        self.assertEqual(len([m for m,p in self.server.calls if m=='review/start']),1)

    def test_superseded_action_wait_retires_only_exact_unsent_receipt(self):
        self.monitor()
        result=self.runtime.native_action(self.a['id'],'review','superseded-review')
        self.assertEqual(result['outcome']['status'],'pending')
        a=self.runtime.agent(self.a['id'])
        a=self.agent_update(a,epoch=a['epoch']+1,error='Keep newer error')
        with self.runtime.lock,self.runtime.db() as db:
            repair.claim_context_wait(self.runtime,db,a)
            outcome=json.loads(db.execute('SELECT outcome FROM runtime_native_action_receipts WHERE id=?',('superseded-review',)).fetchone()[0])
        self.assertTrue(outcome['notSubmitted'])
        self.assertEqual(outcome['status'],'failed')
        self.assertEqual(self.runtime.agent(a['id'])['error'],'Keep newer error')
        self.assertFalse(any(m=='review/start' for m,p in self.server.calls))


if __name__=='__main__':
    suite=unittest.TestSuite(ContextWait(name) for name in ContextWait.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())

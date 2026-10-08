#!/usr/bin/env python3
"""Exact terminal task proof releases only the unchanged unsent context wait."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
import copy
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('context_wait_fixture',
    Path(__file__).with_name('context-repair-wait-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
repair, eventually = f.repair, f.eventually


class StaleTaskWait(f.ContextWait):
    def setUp(self):
        super().setUp()
        self.jobs = []
        original = self.runtime.recovery_pool.submit
        def submit(function, *args):
            if function.__name__ != '_run_task_wait_check':
                return original(function, *args)
            self.jobs.append((function, args))
            return concurrent.futures.Future()
        self.capture = patch.object(self.runtime.recovery_pool, 'submit', side_effect=submit)
        self.capture.start()
        self.addCleanup(self.capture.stop)

    def task(self, kind='dynamicToolCall', **changes):
        value = {'id':self.a['id'] + ':old-item', 'agent':self.a['id'], 'itemId':'old-item',
                 'turnId':'old-turn', 'kind':'command' if kind == 'commandExecution' else 'tool',
                 'type':kind, 'status':'running', 'created':1}
        value.update(changes)
        self.put('tasks', value)
        return value

    def receipt(self, task, **changes):
        value = {'id':self.tid + ':' + task['itemId'], 'agent':self.a['id'],
                 'accountKey':'default', 'threadId':self.tid, 'turnId':task['turnId'],
                 'callId':task['itemId'], 'epoch':-1, 'stage':'completed', 'outcome':'applied',
                 'finished':2, 'result':{'success':True, 'contentItems':[]}}
        value.update(changes)
        self.put('tool_requests', value)
        return value

    def wait(self, task=None, *, tools=None, expected_jobs=1):
        self.runtime.send(self.a['id'], 'Keep the exact input.', message_id='stale-task-input')
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.a['id'], db)
            attempt = {'id':'unchanged-start', 'events':['stale-task-input'], 'submitted':False,
                       'epoch':agent['epoch'], 'accountKey':agent['accountKey']}
            agent.update(startAttempt=attempt, status='queued', inFlight=False,
                contextRepair={'phase':'unchanged', 'source':{'threadId':self.tid},
                               'compactions':agent.get('compactions', 0),
                               'checkedEventIds':[self.event['id']]})
            if tools is not None:
                agent.update(activeTools=copy.deepcopy(tools), turnId='old-turn')
            error = ('Context repair waits for tasks: ' + task['id']) if task else \
                    'Context repair waits for the current agent operation'
            agent['contextRepairWait'] = {'source':repair._identity(agent), 'events':attempt['events'],
                'action':None, 'actionIdentity':None, 'actionRequestId':None,
                'scope':'local', 'error':error, 'nextCheckAt':0}
            self.runtime.put(db, 'agents', agent)
        self.runtime.dispatch()
        self.assertEqual(len(self.jobs), expected_jobs)

    def run_check(self):
        function, args = self.jobs.pop(0)
        function(*args)

    def saved_task(self, task):
        with self.runtime.db() as db:
            return json.loads(db.execute('SELECT record FROM runtime_tasks WHERE id=?',
                (task['id'],)).fetchone()[0])

    def native_item(self, task, **changes):
        item = {'id':task['itemId'], 'type':task['type'], 'status':'completed', 'exitCode':0}
        item.update(changes)
        self.server.items = [{'turnId':task['turnId'], 'item':item}]

    def ended_compaction_run(self, task, *, completed=True, **changes):
        run = {'id':'fixture-compaction-run', 'agent':self.a['id'], 'accountKey':'default',
            'epoch':self.a['epoch'], 'threadId':self.tid, 'turnId':task['turnId'],
            'created':1, 'finished':2, 'status':'interrupted'}
        run.update(changes)
        with self.runtime.db() as db:
            db.execute('DELETE FROM runtime_execution_runs WHERE id=?', (run['id'],))
            db.execute('DELETE FROM runtime_completed_turns WHERE id=?',
                       (self.a['id'] + ':' + task['turnId'],))
            if completed:
                db.execute('INSERT INTO runtime_completed_turns VALUES (?)',
                           (self.a['id'] + ':' + task['turnId'],))
            db.execute('INSERT INTO runtime_execution_runs VALUES (?,?,?,?,?,?,?,?)',
                (run['id'], run['agent'], run['accountKey'], run['epoch'], run['threadId'],
                 run['turnId'], run['created'], json.dumps(run)))

    def test_ended_compaction_releases_existing_wait_without_resending_input(self):
        task = self.task('contextCompaction')
        receipt = self.receipt(task, outcome='unknown')
        self.wait(task, expected_jobs=0)
        before = self.runtime.agent(self.a['id'])
        self.ended_compaction_run(task)
        # Runtime's session-name worker may finish an independent metadata RPC
        # while this test drives the wait transition. Keep the no-replay
        # assertion focused on calls that can resend or restart this input.
        calls = [(method, params) for method, params in self.server.calls
                 if method != 'thread/name/set']
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(self.a['id'], db)
            current['contextRepairWait']['nextCheckAt'] = 0
            job = repair.claim_context_wait(self.runtime, db, current)
            self.assertEqual(job['kind'], 'turn')
            self.assertEqual(job['attempt'], before['startAttempt'])
            self.assertEqual([row['id'] for row in job['rows']], ['stale-task-input'])
            self.assertEqual(db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?',
                ('stale-task-input',)).fetchone()[:], ('reserved', None))
            self.assertEqual(self.runtime.tool_request(receipt['id'], db), receipt)
            self.assertIsNone(repair.claim_context_wait(self.runtime, db, current))
        self.assertEqual(self.saved_task(task), {**task, 'status':'interrupted', 'finished':2})
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(current['startAttempt'], before['startAttempt'])
        self.assertIs(current['startAttempt']['submitted'], False)
        self.assertFalse(current.get('contextRepairWait'))
        self.assertEqual(current['status'], 'starting')
        self.assertEqual([(method, params) for method, params in self.server.calls
                          if method != 'thread/name/set'], calls)

    def test_compaction_wait_keeps_nonterminal_missing_or_changed_identity(self):
        task = self.task('contextCompaction')
        self.wait(task, expected_jobs=0)
        for completed, changes in ((False, {}), (True, {'status':'running'}),
                (True, {'finished':None}), (True, {'accountKey':'other'}),
                (True, {'threadId':'other'}), (True, {'epoch':self.a['epoch'] + 1}),
                (True, {'turnId':'other'}), (True, {'agent':'other'})):
            with self.subTest(completed=completed, changes=changes):
                self.ended_compaction_run(task, completed=completed, **changes)
                with self.runtime.lock, self.runtime.db() as db:
                    agent = self.runtime.agent(self.a['id'], db)
                    agent['contextRepairWait']['nextCheckAt'] = 0
                    result = repair.claim_context_wait(self.runtime, db, agent)
                    self.assertEqual(result, {'waiting':True})
                    self.assertEqual(self.runtime.agent(self.a['id'], db)['startAttempt'],
                                     agent['startAttempt'])
                    self.assertEqual(db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?',
                        ('stale-task-input',)).fetchone()[:], ('pending', None))
                self.assertEqual(self.saved_task(task), task)
                self.assertFalse(self.jobs)
        self.assertFalse(any(method == 'turn/start' for method, params in self.server.calls))

    def test_compaction_wait_keeps_changed_task_identity_and_duplicate_run(self):
        task = self.task('contextCompaction')
        self.wait(task, expected_jobs=0)
        self.ended_compaction_run(task)
        for changes in ({'itemId':'other'}, {'accountKey':'other'}, {'threadId':'other'},
                        {'epoch':self.a['epoch'] + 1}, {'processId':'native-command'}):
            with self.subTest(changes=changes):
                changed = {**task, **changes}
                self.put('tasks', changed)
                with self.runtime.lock, self.runtime.db() as db:
                    agent = self.runtime.agent(self.a['id'], db)
                    with self.assertRaisesRegex(ValueError, 'Context repair waits for tasks'):
                        repair._local_idle(self.runtime, db, agent, agent['startAttempt']['id'])
                self.assertEqual(self.saved_task(task), changed)
        self.put('tasks', task)
        with self.runtime.lock, self.runtime.db() as db:
            run = db.execute('SELECT * FROM runtime_execution_runs WHERE id=?',
                             ('fixture-compaction-run',)).fetchone()
            duplicate = list(run)
            duplicate[0] = 'duplicate-compaction-run'
            db.execute('INSERT INTO runtime_execution_runs VALUES (?,?,?,?,?,?,?,?)', duplicate)
            agent = self.runtime.agent(self.a['id'], db)
            with self.assertRaisesRegex(ValueError, 'Context repair waits for tasks'):
                repair._local_idle(self.runtime, db, agent, agent['startAttempt']['id'])
        self.assertEqual(self.saved_task(task), task)

    def test_compaction_backlog_reconciliation_has_a_transaction_bound(self):
        task = self.task('contextCompaction')
        self.wait(task, expected_jobs=0)
        tasks = [task]
        for index in range(repair.COMPACTION_SETTLE_LIMIT):
            item = 'compaction-' + str(index)
            tasks.append(self.task('contextCompaction', id=self.a['id'] + ':' + item,
                itemId=item, turnId='ended-turn-' + str(index)))
        for index, saved in enumerate(tasks):
            self.ended_compaction_run(saved, id='fixture-ended-compaction-' + str(index))
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.a['id'], db)
            with self.assertRaisesRegex(ValueError, 'Context repair waits for tasks'):
                repair._local_idle(self.runtime, db, agent, agent['startAttempt']['id'])
            running = db.execute("SELECT count(*) FROM runtime_tasks WHERE json_extract(record,'$.agent')=? "
                "AND json_extract(record,'$.status')='running'", (self.a['id'],)).fetchone()[0]
            self.assertEqual(running, 1)
            self.assertEqual(repair._local_idle(self.runtime, db, agent, agent['startAttempt']['id']), [])
            self.assertEqual(db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?',
                ('stale-task-input',)).fetchone()[:], ('pending', None))
        self.assertFalse(any(method == 'turn/start' for method, params in self.server.calls))

    def test_exact_old_epoch_tool_receipt_resumes_same_input_once(self):
        task = self.task()
        receipt = self.receipt(task)
        self.wait(task)
        self.run_check()
        self.assertEqual(self.saved_task(task)['status'], 'completed')
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(current['threadId'], self.tid)
        self.assertIs(current['startAttempt']['submitted'], False)
        self.assertFalse(any(m == 'thread/items/list' for m,p in self.server.calls))
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.tool_request(receipt['id'], db), receipt)
        self.due()
        eventually(lambda:self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(current['startAttempt']['id'], 'unchanged-start')
        self.assertEqual(current['threadId'], self.tid)
        self.runtime.dispatch()
        starts = [p for m,p in self.server.calls if m == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'stale-task-input')

    def test_command_requires_exact_terminal_native_item(self):
        task = self.task('commandExecution')
        self.native_item(task, exitCode=7)
        self.wait(task)
        self.run_check()
        saved = self.saved_task(task)
        self.assertEqual(saved['status'], 'failed')
        self.assertEqual(saved['exitCode'], 7)
        calls = [p for m,p in self.server.calls if m == 'thread/items/list']
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]['threadId'], self.tid)
        self.assertEqual(calls[0]['turnId'], 'old-turn')
        self.assertFalse(any(m == 'thread/turns/list' for m,p in self.server.calls))

    def test_unsupported_task_types_do_not_schedule_repeated_native_reads(self):
        for kind in ('computerToolCall', 'collabAgentToolCall'):
            with self.subTest(kind=kind):
                task = self.task(kind)
                self.wait(task, expected_jobs=0)
                self.assertEqual(self.saved_task(task)['status'], 'running')
                self.assertFalse(any(m == 'thread/items/list' for m,p in self.server.calls))

    def test_externalized_tool_result_uses_the_exact_saved_payload(self):
        task = self.task()
        receipt = self.receipt(task, result={'success':True,
            'contentItems':[{'type':'inputText', 'text':'saved-result-' * 20000}]})
        with self.runtime.db() as db:
            stored = json.loads(db.execute('SELECT record FROM runtime_tool_requests WHERE id=?',
                (receipt['id'],)).fetchone()[0])
        self.assertIn('result', stored['_payloadBlobs'])
        self.wait(task)
        self.run_check()
        self.assertEqual(self.saved_task(task)['status'], 'completed')
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.tool_request(receipt['id'], db), receipt)

    def test_missing_active_or_mismatched_native_item_preserves_old_command(self):
        task = self.task('commandExecution')
        self.wait(task)
        for items in ([], [{'turnId':'other-turn', 'item':{'id':'old-item',
                          'type':'commandExecution', 'status':'completed'}}],
                      [{'turnId':'old-turn', 'item':{'id':'other-item',
                          'type':'commandExecution', 'status':'completed'}}],
                      [{'turnId':'old-turn', 'item':{'id':'old-item',
                          'type':'dynamicToolCall', 'status':'completed'}}],
                      [{'turnId':'old-turn', 'item':{'id':'old-item',
                          'type':'commandExecution', 'status':'inProgress'}}],
                      [{'turnId':'old-turn', 'item':{'id':'old-item',
                          'type':'commandExecution', 'status':'completed'}},
                       {'turnId':'old-turn', 'item':{'id':'old-item',
                          'type':'dynamicToolCall', 'status':'completed'}}],
                      [{'turnId':'old-turn', 'item':None}]):
            with self.subTest(items=items):
                self.server.items = items
                self.run_check()
                self.assertEqual(self.saved_task(task), task)
                self.assertIs(self.runtime.agent(self.a['id'])['startAttempt']['submitted'], False)
                self.due()
        self.run_check()

    def test_unknown_or_wrong_scope_receipt_does_not_complete_task(self):
        task = self.task()
        self.wait(task)
        for changes in ({'outcome':'unknown'}, {'outcome':'not_applied'}, {'stage':'running'},
                        {'finished':None}, {'accountKey':'other'}, {'threadId':'other'},
                        {'turnId':'other'}, {'callId':'other'}, {'agent':'other'}):
            with self.subTest(changes=changes):
                self.receipt(task, **changes)
                self.run_check()
                self.assertEqual(self.saved_task(task), task)
                self.due()
        self.run_check()

    def test_native_read_failure_preserves_task_and_bounds_timeout(self):
        task = self.task('commandExecution')
        self.wait(task)
        original = self.server.call
        def call(method, params, timeout=10):
            if method == 'thread/items/list':
                self.assertGreater(timeout, 0)
                self.assertLessEqual(timeout, repair.TASK_CHECK_SECONDS)
                raise RuntimeError('The native read response was lost')
            return original(method, params, timeout)
        self.server.call = call
        self.run_check()
        self.assertEqual(self.saved_task(task), task)
        wait = self.runtime.agent(self.a['id'])['contextRepairWait']
        self.assertIn('response was lost', wait['lastTaskCheckError'])
        self.assertFalse(wait.get('taskCheckId'))
        self.assertGreaterEqual(wait['nextCheckAt'] - wait['lastTaskCheckAt'], 14.9)

    def test_agent_input_connection_and_task_races_preserve_task(self):
        task = self.task('commandExecution')
        self.wait(task)
        original = self.server.call
        changes = [({'epoch':self.a['epoch'] + 1}, None), ({'threadId':'other'}, None),
                   ({'accountKey':'other'}, None), ({'autoWake':False}, None),
                   ({'turnId':'replacement-turn'}, None),
                   ({'inFlight':True}, None), ({'startAttempt':{'id':'other'}}, None),
                   ({}, 'input'), ({}, 'connection'), ({}, 'task')]
        initial = self.runtime.agent(self.a['id'])
        for update, mutation in changes:
            with self.subTest(update=update, mutation=mutation):
                self.native_item(task)
                def call(method, params, timeout=10):
                    if method == 'thread/items/list':
                        if update:
                            self.agent_update(self.runtime.agent(self.a['id']), **update)
                        if mutation == 'input':
                            with self.runtime.db() as db:
                                db.execute("UPDATE runtime_events SET status='uncertain' WHERE id='stale-task-input'")
                        if mutation == 'connection':
                            self.runtime.connection_ids['default'] = 'replacement'
                        if mutation == 'task':
                            self.put('tasks', {**task, 'turnId':'replacement-turn'})
                    return original(method, params, timeout)
                self.server.call = call
                self.run_check()
                self.assertEqual(self.saved_task(task)['status'], 'running')
                restored = copy.deepcopy(initial)
                restored['contextRepairWait'].update(taskCheckId=None, taskCheckOwner=None, nextCheckAt=0)
                self.put('agents', restored)
                self.put('tasks', task)
                self.runtime.connection_ids['default'] = 'fixture-connection'
                with self.runtime.db() as db:
                    db.execute("UPDATE runtime_events SET status='pending' WHERE id='stale-task-input'")
                self.server.call = original
                self.runtime.dispatch()
        self.run_check()

    def test_active_tools_remove_only_exact_terminal_items_from_same_turn(self):
        tools = [{'id':'done-tool', 'type':'dynamicToolCall'},
                 {'id':'working-command', 'type':'commandExecution'},
                 {'id':'absent-tool', 'type':'dynamicToolCall'}]
        self.server.items = [{'turnId':'old-turn', 'item':{'id':'done-tool',
            'type':'dynamicToolCall', 'status':'completed'}}, {'turnId':'old-turn',
            'item':{'id':'working-command', 'type':'commandExecution', 'status':'inProgress'}}]
        self.wait(tools=tools)
        self.run_check()
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(current['activeTools'], tools[1:])
        self.assertEqual(current['turnId'], 'old-turn')
        self.assertEqual(current['threadId'], self.tid)
        self.assertIs(current['startAttempt']['submitted'], False)
        self.assertFalse(any(m == 'turn/start' for m,p in self.server.calls))

    def test_active_tools_change_during_read_does_not_clear_or_hold_check_lease(self):
        tools = [{'id':'done-tool', 'type':'dynamicToolCall'}]
        self.wait(tools=tools)
        original = self.server.call
        def call(method, params, timeout=10):
            if method == 'thread/items/list':
                self.agent_update(self.runtime.agent(self.a['id']),
                                  activeTools=tools + [{'id':'new-tool', 'type':'dynamicToolCall'}])
                return {'data':[{'turnId':'old-turn', 'item':{'id':'done-tool',
                    'type':'dynamicToolCall', 'status':'completed'}}]}
            return original(method, params, timeout)
        self.server.call = call
        self.run_check()
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(len(current['activeTools']), 2)
        self.assertFalse(current['contextRepairWait'].get('taskCheckId'))


if __name__ == '__main__':
    suite = unittest.TestSuite(StaleTaskWait(name) for name in StaleTaskWait.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())

#!/usr/bin/env python3
"""Real source callbacks survive a repair fork without changing the new turn."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location('context_fixture', Path(__file__).with_name('context-repair-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
repair = f.repair


class LateCallback(f.ContextRepair):
    def setUp(self):
        super().setUp()
        self.parent = self.lead('Parent')
        self.parent = self.agent_update(self.parent, autoWake=True, status='waiting')
        self.a = self.agent_update(self.a, parentId=self.parent['id'], rootId=self.parent['id'],
                                   isLead=False, autoWake=True, lastAnswer='Exact source result.')
        self.runtime._fast_delivery_enabled = False
        with self.runtime.db() as db:
            db.execute('DELETE FROM runtime_completed_turns')

    def callback(self, *, thread=None, turn='turn', status='completed', account='default', connection=None):
        self.runtime.notification({'method':'turn/completed', 'params':{
            'threadId':thread or self.tid, 'turn':{'id':turn, 'status':status}}}, account,
            connection or self.runtime.connection_ids['default'])

    def result_rows(self, turn='turn'):
        with self.runtime.db() as db:
            return [dict(row) for row in db.execute('SELECT * FROM runtime_events WHERE id=?',
                ('child:' + self.a['id'] + ':' + turn,))]

    def fork(self):
        result = repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(result['contextRepair']['phase'], 'completed')
        self.assertNotEqual(result['threadId'], self.tid)
        f.f.eventually(lambda: (self.runtime.agent(self.a['id']).get('contextRepair') or {}).get(
            'sourceCleanup', {}).get('phase') == 'completed')
        return self.runtime.agent(self.a['id'])

    def test_real_source_callback_after_fork_notifies_parent_once(self):
        current = self.fork()
        self.callback()
        rows = self.result_rows()
        self.assertEqual(len(rows), 1)
        payload = json.loads(rows[0]['text'])
        self.assertEqual(payload['result'], 'Exact source result.')
        self.assertEqual(payload['status'], 'completed')
        self.assertEqual(rows[0]['status'], 'pending')
        self.assertEqual(self.runtime.agent(self.parent['id'])['status'], 'queued')
        self.callback()
        self.assertEqual(self.result_rows(), rows)
        self.assertEqual(self.runtime.agent(self.a['id']), current)
        with self.runtime.db() as db:
            self.assertIsNotNone(db.execute('SELECT 1 FROM runtime_completed_turns WHERE id=?',
                                          (self.a['id'] + ':turn',)).fetchone())

    def test_real_source_callback_preserves_new_turn_and_exact_input(self):
        current = self.fork()
        current = self.agent_update(current, turnId='new-turn', inFlight=True, status='running',
            lastAnswer='New answer.', error='Keep the new error.', activeTools=[{'id':'new-tool'}],
            activity={'phase':'tool'}, startAttempt={'id':'new-attempt','submitted':True,
                'events':['new-exact-input'], 'epoch':current['epoch'], 'accountKey':'default',
                'threadId':current['threadId'], 'turnId':'new-turn'})
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('new-exact-input',current['id'],'user','Keep this new input.','uncertain',4,
                 current['epoch'],'new-turn','Keep the unknown input.'))
        self.callback()
        self.assertEqual(len(self.result_rows()), 1)
        self.assertEqual(json.loads(self.result_rows()[0]['text'])['result'], 'Exact source result.')
        self.assertEqual(self.runtime.agent(self.a['id']), current)
        with self.runtime.db() as db:
            self.assertEqual(tuple(db.execute('SELECT status,turn_id,error FROM runtime_events WHERE id=?',
                                             ('new-exact-input',)).fetchone()),
                             ('uncertain','new-turn','Keep the unknown input.'))

    def test_real_source_callback_preserves_stop_and_owner_scope(self):
        current = self.fork()
        for change in ({'epoch':current['epoch'] + 1, 'status':'paused', 'autoWake':False},
                       {'accountKey':'other-account'}, {'parentId':'other-parent'},
                       {'rootId':'other-root'}, {'threadId':'unrelated-thread'}):
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', {**current, **change})
                before = self.runtime.agent(current['id'])
                self.callback()
                self.assertEqual(self.result_rows(), [])
                self.assertEqual(self.runtime.agent(current['id']), before)
                with self.runtime.db() as db:
                    self.assertIsNone(db.execute('SELECT 1 FROM runtime_completed_turns WHERE id=?',
                                                (current['id'] + ':turn',)).fetchone())

    def test_real_source_callback_rejects_foreign_callback_scope(self):
        current = self.fork()
        for options in ({'thread':'foreign-thread'}, {'turn':'foreign-turn'},
                        {'status':'failed'}, {'account':'other-account'}, {'connection':'old-connection'}):
            with self.subTest(options=options):
                self.callback(**options)
                self.assertEqual(self.result_rows(), [])
                self.assertEqual(self.runtime.agent(current['id']), current)
        self.runtime.notification({'method':'turn/completed', 'params':{
            'threadId':self.tid, 'turn':{'id':'turn','status':'completed'}}}, 'default', None)
        self.assertEqual(self.result_rows(), [])

    def native_identity(self):
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(root=self.state, handle='account:default', generation=4)

    def test_real_source_callback_allows_new_connection_to_same_retained_child(self):
        self.native_identity()
        current = self.fork()
        self.runtime.connection_ids['default'] = 'new-backend-connection'
        self.callback()
        self.assertEqual(len(self.result_rows()), 1)
        self.assertEqual(self.runtime.agent(current['id']), current)

    def test_real_source_callback_rejects_new_native_generation(self):
        self.native_identity()
        current = self.fork()
        self.server.proc.generation += 1
        for connection in (self.runtime.connection_ids['default'], 'new-backend-connection'):
            self.runtime.connection_ids['default'] = connection
            self.callback()
            self.assertEqual(self.result_rows(), [])
            self.assertEqual(self.runtime.agent(current['id']), current)

    def test_real_source_callback_uses_saved_native_final_text(self):
        self.server.items = [{'turnId':'turn', 'item':{'id':'final-item','type':'agentMessage',
                                                      'text':'Native exact final text.'}}]
        current = self.fork()
        current = self.agent_update(current, lastAnswer='New turn text.')
        self.callback()
        self.assertEqual(json.loads(self.result_rows()[0]['text'])['result'], 'Native exact final text.')
        self.assertEqual(self.runtime.agent(current['id']), current)

    def test_source_callback_lookup_is_indexed_and_reads_no_native_state(self):
        self.fork()
        with self.runtime.db() as db:
            plans = [row[3] for row in db.execute('EXPLAIN QUERY PLAN SELECT record '
                                                'FROM runtime_context_terminal_receipts WHERE id=?', ('key',))]
            self.assertTrue(any('SEARCH' in plan and 'INDEX' in plan for plan in plans), plans)
            record = json.loads(db.execute('SELECT record FROM runtime_context_terminal_receipts').fetchone()[0])
            self.assertNotIn('startAttempt', record['sourceActor'])
            self.assertNotIn('prompt', record['sourceActor'])
        with patch.object(self.runtime, 'connect', side_effect=AssertionError('No native call')):
            self.callback()
        self.assertEqual(len(self.result_rows()), 1)

    def test_real_source_callback_survives_backend_restart_and_starts_parent_once(self):
        self.native_identity()
        self.fork()
        old = self.runtime
        old.servers.clear()
        old.servers.update(self.initial_servers)
        old.close()
        self.runtime = f.f.ControlledRuntime(self.state, f.f.WorkspaceServer)
        self.initial_servers = dict(self.runtime.servers)
        self.runtime._fast_delivery_enabled = False
        self.runtime.servers['default'] = self.server
        self.runtime.connection_ids['default'] = 'restart-connection'
        before = self.runtime.agent(self.a['id'])
        self.callback()
        self.assertEqual(len(self.result_rows()), 1)
        self.assertEqual(self.runtime.agent(self.a['id']), before)
        self.callback()
        self.assertEqual(len(self.result_rows()), 1)
        native = f.f.WorkspaceServer(self.state,
            lambda message: self.runtime.notification(message, 'default', 'restart-connection'),
            lambda message: self.runtime.request(message, 'default', 'restart-connection'), lambda: None)
        self.runtime.servers['default'] = native
        self.runtime.server = native
        self.runtime.image_workspace_support = lambda _repo: (False, 'disabled in protocol fixture')
        event_id = self.result_rows()[0]['id']
        self.runtime.dispatch()
        f.f.eventually(lambda: self.runtime.delivery_receipt(event_id)['status'] == 'delivered')
        self.runtime.dispatch()
        starts = [params for method, params in native.calls if method == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], event_id)
        native.close()

    def test_two_repairs_retain_both_exact_source_callback_receipts(self):
        first = self.fork()
        first_thread = first['threadId']
        text = json.dumps({'id':'second-monitor', 'exitCode':0, 'stdout':'second-output-' * 900})
        records = copy.deepcopy(self.records)
        records[0]['payload']['id'] = first_thread
        records[1]['payload']['turn_id'] = 'second-turn'
        message = records[2]['payload']
        message['internal_chat_message_metadata_passthrough']['turn_id'] = 'second-turn'
        message['content'][0]['text'] = '[Orchestration event: monitor_exit]\n' + text
        records[3]['payload']['message'] = text
        records[4]['payload']['replacement_history'][0] = copy.deepcopy(message)
        records[-1]['payload']['turn_id'] = 'second-turn'
        second_path = self.home / 'sessions' / ('second-' + first_thread + '.jsonl')
        second_path.write_text(''.join(json.dumps(r) + '\n' for r in records))
        self.server.tid, self.server.path = first_thread, second_path
        call = self.server.call
        def second_history(method, params, timeout=10):
            result = call(method, params, timeout)
            if method == 'thread/turns/list':
                result['data'][0]['id'] = 'second-turn'
            return result
        self.server.call = second_history
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('second-event',first['id'],'monitor_exit',text,'delivered',2,first['epoch'],'second-turn',None))
        second = repair.repair_idle(self.runtime, self.a['id'])
        f.f.eventually(lambda: self.runtime.agent(self.a['id'])['contextRepair']['sourceCleanup']['phase'] == 'completed')
        second = self.runtime.agent(self.a['id'])
        self.assertNotEqual(second['threadId'], first_thread)
        self.callback()
        self.callback(thread=first_thread, turn='second-turn')
        self.callback()
        self.callback(thread=first_thread, turn='second-turn')
        self.assertEqual(len(self.result_rows()), 1)
        self.assertEqual(len(self.result_rows('second-turn')), 1)
        self.assertEqual(self.runtime.agent(self.a['id']), second)

    def test_failed_source_callback_keeps_current_work_and_uses_source_assignment(self):
        self.server.turn_status = 'failed'
        work = {'id':'source-task', 'owner':self.a['id'], 'rootId':self.parent['id'],
                'status':'running', 'updated':1, 'results':[]}
        with self.runtime.db() as db:
            self.runtime.put(db, 'work', work)
        current = self.fork()
        current = self.agent_update(current, status='running', inFlight=True, turnId='new-turn', error=None)
        changed = {**work, 'status':'review', 'results':[{'id':'new-result', 'agent':self.a['id']}]}
        with self.runtime.db() as db:
            self.runtime.put(db, 'work', changed)
        self.callback(status='failed')
        self.assertEqual(self.runtime.agent(self.a['id']), current)
        with self.runtime.db() as db:
            self.assertEqual(json.loads(db.execute('SELECT record FROM runtime_work WHERE id=?',
                                                  (work['id'],)).fetchone()[0]), changed)
            event = db.execute('SELECT text FROM runtime_events WHERE id=?',
                ('child-stop:' + current['id'] + ':' + str(current['epoch']) + ':turn:turn',)).fetchone()
            payload = json.loads(event[0])
            self.assertEqual(payload['tasks'][0]['status'], 'running')
            self.assertIsNone(payload['tasks'][0]['current_result_id'])
            self.assertEqual(payload['thread_id'], self.tid)


if __name__ == '__main__':
    suite = unittest.TestSuite(LateCallback(name) for name in LateCallback.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())

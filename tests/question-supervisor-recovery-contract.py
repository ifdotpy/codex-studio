#!/usr/bin/env python3
"""A backend restart preserves an exact unanswered native Claude question."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
from contextlib import closing, contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_question_recovery import recover_native_questions
from codex_runtime import Runtime


class NoNativeServer:
    def __init__(self, *_args, **_kwargs):
        raise AssertionError('This fixture must not start a native process')


class RetainedServer:
    supervisor_mode = True

    def __init__(self, root, *, generation=9):
        self.proc = SimpleNamespace(root=root, handle='account:default', generation=generation)
        self.responses = []

    def write(self, value, **_kwargs):
        self.responses.append(copy.deepcopy(value))

    def close(self):
        pass

    def join_callbacks(self, **_kwargs):
        return True


class QuestionSupervisorRecovery(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-question-recovery-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.runtime = self.open_runtime()
        self.addCleanup(lambda: self.runtime.close())
        self.connection = 'before-restart'
        self.server = RetainedServer(self.root)
        self.attach(self.server, self.connection)
        now = time.time()
        self.owner = {'id':'owner', 'rootId':'owner', 'parentId':None, 'name':'Owner',
            'isLead':True, 'role':'orchestrator', 'provider':'claude', 'accountKey':'default',
            'threadId':'thread', 'turnId':'turn', 'epoch':1, 'turnEpoch':1,
            'status':'running', 'autoWake':True, 'inFlight':True, 'created':now - 20,
            'events':0, 'cwd':str(self.root), 'prompt':'Fixture', 'model':'fixture',
            'effort':'medium', 'tokensUsed':0, 'tokenBudget':None, 'tail':'',
            'activeTools':[{'id':'tool-question', 'type':'mcpToolCall', 'name':'AskUserQuestion'}]}
        self.item = {'id':'tool-question', 'type':'mcpToolCall', 'tool':'AskUserQuestion',
                     'status':'inProgress', 'arguments':{'questions':[{'question':'Delete organization?'}]}}
        self.run = {'id':'run', 'agent':'owner', 'accountKey':'default', 'epoch':1,
                    'threadId':'thread', 'turnId':'turn', 'status':'running', 'created':now - 10}
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.owner)
            self.runtime.item(db, 'owner', 'tool-question', 'tool', json.dumps(self.item), turnId='turn')
            db.execute('INSERT INTO runtime_execution_runs VALUES (?,?,?,?,?,?,?,?)',
                       ('run', 'owner', 'default', 1, 'thread', 'turn', now - 10, json.dumps(self.run)))
        self.rpc_id = 'claude:bff3535a-5386-4484-a568-3d78a0551f0e'
        self.runtime.request({'id':self.rpc_id, 'method':'item/tool/requestUserInput',
            'params':{'threadId':'thread', 'turnId':'turn', 'itemId':'tool-question',
                      'questions':[{'id':'0', 'question':'Delete organization?',
                                    'options':[{'label':'No'}, {'label':'Yes'}]}]}},
            'default', self.connection)
        with self.runtime.read_db() as db:
            self.request = json.loads(db.execute('SELECT record FROM runtime_requests').fetchone()[0])
        with closing(sqlite3.connect(self.root / 'supervisor.sqlite3')) as db, db:
            db.executescript('CREATE TABLE handles(id TEXT PRIMARY KEY,pid INTEGER,generation INTEGER,closed_at REAL);'
                             'CREATE TABLE child_identities(handle TEXT,pid INTEGER,start_time TEXT);'
                             'CREATE TABLE operations(handle TEXT,native_id TEXT);')
            db.execute('INSERT INTO handles VALUES (?,?,?,NULL)', ('account:default', 98765, 9))
            db.execute('INSERT INTO child_identities VALUES (?,?,?)', ('account:default', 98765, str(now - 100)))
        self.native_match = patch('codex_process_supervisor.process_start_matches', return_value=True)
        self.native_match.start()
        self.addCleanup(self.native_match.stop)

    def open_runtime(self):
        with patch.object(Runtime, 'schedule', lambda _runtime: None), \
                patch.object(Runtime, '_restore_startup_supervisor_handles', lambda _runtime: None):
            return Runtime(self.root, NoNativeServer)

    def attach(self, server, connection):
        self.runtime.servers['default'] = server
        self.runtime.connection_ids['default'] = connection
        self.runtime.offline_accounts.discard('default')

    def stored(self):
        with self.runtime.read_db() as db:
            return json.loads(db.execute('SELECT record FROM runtime_requests WHERE id=?',
                                         (self.request['id'],)).fetchone()[0])

    def update_request(self, **values):
        with self.runtime.db() as db:
            value = self.stored()
            value.update(values)
            self.runtime.put(db, 'requests', value)

    def restart(self, *, legacy=False, expected='expired'):
        if legacy:
            with self.runtime.db() as db:
                value = self.stored()
                value.pop('epoch', None)
                value.pop('supervisor', None)
                self.runtime.put(db, 'requests', value)
        self.runtime.close()
        self.runtime = self.open_runtime()
        self.connection = 'after-restart'
        self.server = RetainedServer(self.root)
        self.attach(self.server, self.connection)
        self.assertEqual(self.stored()['status'], expected)

    def restore(self, *, resumed=True):
        self.runtime.supervisor_reattached('default', self.connection, resumed)

    def test_restart_restores_form_and_exact_answer_once_without_input(self):
        self.assertEqual(self.request['epoch'], 1)
        self.assertEqual(self.request['supervisor']['generation'], 9)
        self.restart()
        self.restore()
        saved = self.stored()
        self.assertEqual(saved['status'], 'pending')
        self.assertEqual(saved['rpcId'], self.rpc_id)
        self.assertEqual(saved['connectionId'], self.connection)
        self.assertEqual(saved['params'], self.request['params'])
        self.assertEqual(self.server.responses, [])
        self.assertEqual(self.runtime.agent('owner')['status'], 'approval')
        with self.runtime.read_db() as db:
            row = db.execute("SELECT payload,deleted FROM sync_entities WHERE collection='request' AND id=?",
                             (saved['id'],)).fetchone()
            self.assertEqual(row[1], 0)
            self.assertEqual(json.loads(row[0])['value']['status'], 'pending')
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_events').fetchone()[0], 0)
        self.restore()
        self.assertEqual(self.stored(), saved)
        answer = {'answers':{'0':{'answers':['No']}}}
        self.runtime.answer(saved['id'], answer)
        self.assertTrue(self.runtime.answer(saved['id'], answer)['replayed'])
        self.assertEqual(self.server.responses, [{'id':self.rpc_id, 'result':answer}])

    def test_legacy_expired_request_uses_original_execution_run_epoch(self):
        self.restart(legacy=True)
        self.restore()
        saved = self.stored()
        self.assertEqual(saved['status'], 'pending')
        self.assertEqual(saved['epoch'], 1)
        self.assertEqual(self.server.responses, [])

    def test_replacement_child_and_unverified_process_do_not_restore(self):
        self.restart()
        self.server.proc.generation = 10
        self.restore()
        self.assertEqual(self.stored()['status'], 'expired')
        self.server.proc.generation = 9
        self.native_match.stop()
        with patch('codex_process_supervisor.process_start_matches', return_value=False):
            self.assertEqual(recover_native_questions(self.runtime, 'default', self.connection), 0)
        self.assertEqual(self.stored()['status'], 'expired')

    def test_legacy_new_child_birth_and_accepted_reply_remain_unknown(self):
        self.restart(legacy=True)
        with closing(sqlite3.connect(self.root / 'supervisor.sqlite3')) as db, db:
            db.execute('UPDATE child_identities SET start_time=?', (str(time.time() + 1),))
        self.restore()
        self.assertEqual(self.stored()['status'], 'expired')
        with closing(sqlite3.connect(self.root / 'supervisor.sqlite3')) as db, db:
            db.execute('UPDATE child_identities SET start_time=?', (str(self.request['createdAt'] - 100),))
            db.execute('INSERT INTO operations VALUES (?,?)', ('account:default', self.rpc_id))
        self.assertEqual(recover_native_questions(self.runtime, 'default', self.connection), 0)
        self.assertEqual(self.stored()['status'], 'expired')

    def test_answering_uncertain_and_answer_signature_never_restore(self):
        self.update_request(status='answering', answerSignature='saved-answer')
        self.restart(expected='uncertain')
        self.assertEqual(self.stored()['status'], 'uncertain')

    def test_fresh_transport_does_not_restore_expired_question(self):
        self.restart()
        self.restore(resumed=False)
        self.assertEqual(self.stored()['status'], 'expired')
        self.assertEqual(self.server.responses, [])

    def test_changed_epoch_thread_turn_item_and_request_do_not_restore(self):
        self.restart(legacy=True)
        self.restore(resumed=False)
        with self.runtime.db() as db:
            owner = self.runtime.agent('owner', db)
            owner.update(status='running', autoWake=True, inFlight=True)
            owner['restartRecovery'].update(stage='reattached')
            owner['supervisorRestore'] = {'status':'restored', 'reason':'live_handle_resumed',
                'accountKey':'default', 'epoch':1, 'threadId':'thread', 'turnId':'turn'}
            self.runtime.put(db, 'agents', owner)
        for field, value in (('epoch', 2), ('threadId', 'other'), ('turnId', 'other'),
                             ('accountKey', 'other'), ('activeTools', []), ('autoWake', False),
                             ('deletedAt', time.time()), ('nativeFailureHold', {'reason':'hold'}),
                             ('nativeThreadBlock', {'threadId':'thread'}), ('accountTransferId', 'transfer'),
                             ('workspaceOperation', {'id':'operation'}),
                             ('contextRepair', {'id':'repair', 'phase':'preparing'}),
                             ('contextRepairWait', {'error':'wait'})):
            with self.subTest(field=field), self.runtime.db() as db:
                changed = copy.deepcopy(owner)
                changed[field] = value
                self.runtime.put(db, 'agents', changed)
                db.commit()
                self.assertEqual(recover_native_questions(self.runtime, 'default', self.connection), 0)
                self.runtime.put(db, 'agents', owner)
        self.update_request(answerSignature='saved-answer')
        self.assertEqual(recover_native_questions(self.runtime, 'default', self.connection), 0)
        self.assertEqual(self.server.responses, [])

    def test_native_proof_runs_without_runtime_writer_and_race_cannot_restore(self):
        self.restart()
        from codex_tool_response_recovery import _native_proof
        def proof(runtime, server, request):
            self.assertFalse(runtime.lock._is_owned())
            with closing(sqlite3.connect(runtime.db_path, timeout=.05)) as db:
                db.execute('BEGIN IMMEDIATE')
                db.rollback()
            self.update_request(status='deleted')
            return _native_proof(runtime, server, request)
        with patch('codex_question_recovery._native_proof', proof):
            self.restore()
        self.assertEqual(self.stored()['status'], 'deleted')
        self.assertEqual(self.server.responses, [])

    def test_duplicate_admitted_during_native_proof_does_not_restore_old_request(self):
        self.restart()
        from codex_tool_response_recovery import _native_proof
        original = self.stored()
        duplicate = copy.deepcopy(original)
        duplicate.update(id='duplicate', status='pending', connectionId=self.connection,
                         rpcId='claude:12345678-1234-4123-8123-123456789012')
        def proof(runtime, server, request):
            self.assertFalse(runtime.lock._is_owned())
            with runtime.db() as db:
                runtime.put(db, 'requests', duplicate)
            self.assertEqual(self.stored(), original)
            return _native_proof(runtime, server, request)
        with patch('codex_question_recovery._native_proof', proof):
            self.restore()
        self.assertEqual(self.stored(), original)
        with self.runtime.read_db() as db:
            rows = [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_requests')]
        self.assertEqual([row['id'] for row in rows if row['status'] == 'pending'], ['duplicate'])
        self.assertEqual(self.runtime.agent('owner')['status'], 'running')
        self.assertEqual(self.server.responses, [])

    def test_malformed_params_and_native_item_do_not_break_account_reattach(self):
        self.restart()
        for params in ([], 'invalid', {'threadId':'thread', 'turnId':'turn', 'itemId':'tool-question',
                                       'questions':[]}):
            with self.subTest(params=params):
                self.update_request(params=params)
                self.restore()
                self.assertEqual(self.stored()['status'], 'expired')
        self.update_request(params=self.request['params'])
        for text in ('{"truncated":', '[]', 'null', '"unexpected"'):
            with self.subTest(text=text), self.runtime.db() as db:
                self.runtime.item(db, 'owner', 'tool-question', 'tool', text, turnId='turn')
                db.commit()
                self.restore()
                self.assertEqual(self.stored()['status'], 'expired')
        self.assertEqual(self.runtime.agent('owner')['status'], 'running')
        self.assertEqual(self.server.responses, [])

    def test_ambiguous_duplicate_question_requests_remain_expired(self):
        self.restart()
        duplicate = self.stored()
        duplicate['id'] = 'duplicate'
        duplicate['rpcId'] = 'claude:12345678-1234-4123-8123-123456789012'
        for status in ('expired', 'pending', 'answering', 'uncertain', 'answered', 'deleted'):
            with self.subTest(status=status), self.runtime.db() as db:
                duplicate['status'] = status
                self.runtime.put(db, 'requests', duplicate)
                db.commit()
                self.restore()
                self.assertEqual(self.stored()['status'], 'expired')
                self.assertEqual(self.server.responses, [])

    def test_question_lookup_uses_index_and_ignores_owner_history(self):
        self.restart()
        self.restore(resumed=False)
        with self.runtime.db() as db:
            owner = self.runtime.agent('owner', db)
            owner.update(status='running', autoWake=True, inFlight=True)
            owner['restartRecovery'].update(stage='reattached')
            owner['supervisorRestore'] = {'status':'restored', 'reason':'live_handle_resumed',
                'accountKey':'default', 'epoch':1, 'threadId':'thread', 'turnId':'turn'}
            self.runtime.put(db, 'agents', owner)
            historical = copy.deepcopy(self.stored())
            rows = []
            for index in range(2000):
                historical['id'] = 'history-' + str(index)
                historical['params']['itemId'] = 'old-tool-' + str(index)
                rows.append((historical['id'], json.dumps(historical)))
            db.executemany('INSERT INTO runtime_requests VALUES (?,?)', rows)
        original = self.runtime.read_db
        steps = 0
        queries = []
        @contextmanager
        def limited():
            nonlocal steps
            with original() as db:
                def progress():
                    nonlocal steps
                    steps += 100
                    return int(steps > 2000)
                db.set_progress_handler(progress, 100)
                db.set_trace_callback(queries.append)
                yield db
        with patch.object(self.runtime, 'read_db', limited):
            self.assertEqual(recover_native_questions(self.runtime, 'default', self.connection), 1)
        lookup = next(query for query in queries if 'FROM runtime_requests WHERE' in query)
        self.assertIn('LIMIT 2', lookup)
        with original() as db:
            plan = db.execute('EXPLAIN QUERY PLAN ' + lookup).fetchall()
        self.assertIn('runtime_request_native_question', str([tuple(row) for row in plan]))
        self.assertLessEqual(steps, 2000)


if __name__ == '__main__':
    unittest.main()
